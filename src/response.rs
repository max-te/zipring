use crate::fstree;
use crate::fstree::FsTreeNode;
use crate::rc_zip_monoio::find_entry_compressed_data;
use crate::request::AcceptedEncodings;
use crate::request::Request;
use const_format::formatcp;
use monoio::buf::IoBufMut;
use monoio::fs::File;
use monoio::io::AsyncWriteRentExt;
use monoio::io::OwnedWriteHalf;
use monoio::net::TcpStream;
use percent_encoding::AsciiSet;
use percent_encoding::CONTROLS;
use percent_encoding::utf8_percent_encode;
use rc_zip::fsm::EntryFsm;
use rc_zip::parse::Entry;
use rc_zip::parse::Method;
use std::io::Cursor;
use std::io::Write;
use tracing::Instrument as _;

use crate::Buf;

pub async fn respond(
    request: Request,
    file: &File,
    tree: &FsTreeNode,
    stream: &mut monoio::io::OwnedWriteHalf<TcpStream>,
    buf: Buf,
) -> std::io::Result<Buf> {
    let respond_span = tracing::info_span!("response").entered();
    match request {
        Request::Get {
            path,
            if_none_match,
            accepted_encodings,
            close: _,
        } => {
            respond_span.record("path", &path);
            let Some(node) = tree.find(&path) else {
                tracing::warn!(?path, "entry not found");
                return serve_not_found(stream)
                    .instrument(respond_span.exit())
                    .await
                    .map(|()| buf);
            };
            if let Some(crc32) = if_none_match
                && let Some(entry) = node.entry()
                && entry.crc32 == crc32
            {
                tracing::debug!("etag matches");
                serve_not_modified(stream, buf, entry.crc32)
                    .instrument(respond_span.exit())
                    .await
            } else {
                serve_node(stream, file, buf, node, accepted_encodings)
                    .instrument(respond_span.exit())
                    .await
            }
        }
        Request::Bad { status } => serve_bad_request(stream, status).await.map(|()| buf),
    }
}

async fn serve_bad_request(
    stream: &mut OwnedWriteHalf<TcpStream>,
    status: &'static str,
) -> std::io::Result<()> {
    stream.write_all(b"HTTP/1.1 ").await.0?;
    stream.write_all(status).await.0?;
    stream.write_all(b"\r\nContent-Length: 0\r\n\r\n").await.0?;
    Ok(())
}

async fn serve_not_found(stream: &mut OwnedWriteHalf<TcpStream>) -> Result<(), std::io::Error> {
    stream
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
        .await
        .0
        .map(|_| ())
}

async fn serve_not_implemented(
    stream: &mut OwnedWriteHalf<TcpStream>,
) -> Result<(), std::io::Error> {
    const MSG: &str = "This file content uses an unsupported compression method.";
    const RESPONSE: &str = formatcp!(
        "HTTP/1.1 500 Unsupported Compression\r\nContent-Length: {}\r\n\r\n{}",
        MSG.len(),
        MSG
    );

    stream.write_all(RESPONSE).await.0.map(|_| ())
}

async fn serve_not_modified(
    stream: &mut OwnedWriteHalf<TcpStream>,
    mut buf: Buf,
    crc32: u32,
) -> Result<Buf, std::io::Error> {
    const NOT_MODIFIED_TEMPLATE: &[u8] = b"HTTP/1.1 304 Not Modified\r\nETag: \"xxxxxxxx\"\r\n\r\n";
    const CRC_OFFSET: usize = position(NOT_MODIFIED_TEMPLATE, b'x').expect("template sould have x");

    (buf[0..NOT_MODIFIED_TEMPLATE.len()]).copy_from_slice(NOT_MODIFIED_TEMPLATE);
    let etag = &mut buf[CRC_OFFSET..{ CRC_OFFSET + size_of::<u32>() * 2 }];
    const_hex::encode_to_slice(crc32.to_le_bytes(), etag).unwrap();

    let slice = IoBufMut::slice_mut(buf, 0..NOT_MODIFIED_TEMPLATE.len());
    let (res, slice) = stream.write_all(slice).await;
    buf = slice.into_inner();
    res?;
    Ok(buf)
}

const fn position(haystack: &[u8], needle: u8) -> Option<usize> {
    let mut idx = 0;
    while idx < haystack.len() {
        if haystack[idx] == needle {
            return Some(idx);
        }
        idx += 1;
    }
    None
}

async fn serve_node(
    stream: &mut OwnedWriteHalf<TcpStream>,
    file: &File,
    buf: Buf,
    node: &fstree::FsTreeNode,
    accepted_encodings: AcceptedEncodings,
) -> Result<Buf, std::io::Error> {
    match node {
        fstree::FsTreeNode::Dir {
            name: _,
            entry: _,
            is_root,
            children,
            index_html_index,
        } => {
            if let Some(idx) = index_html_index
                && let fstree::FsTreeNode::File { entry, .. } = &children[*idx]
            {
                serve_entry(stream, file, buf, entry, accepted_encodings).await
            } else {
                serve_index(*is_root, children, stream, buf).await
            }
        }
        fstree::FsTreeNode::File { entry, .. } => {
            serve_entry(stream, file, buf, entry, accepted_encodings).await
        }
    }
}

struct ContentCompression {
    extra_len: u64,
    encoding: &'static str,
}

#[tracing::instrument(skip_all, level = "debug")]
async fn serve_entry(
    stream: &mut OwnedWriteHalf<TcpStream>,
    file: &File,
    mut buf: Buf,
    entry: &Entry,
    accepted_encodings: AcceptedEncodings,
) -> Result<Buf, std::io::Error> {
    let compression = match entry.method {
        Method::Deflate if accepted_encodings.gzip => Some(ContentCompression {
            encoding: "gzip",
            extra_len: 18,
        }),
        Method::Zstd if accepted_encodings.zstd => Some(ContentCompression {
            encoding: "zstd",
            extra_len: 0,
        }),
        _ => None,
    };

    if compression.is_some() {
        buf = send_header(stream, buf, entry, compression.as_ref()).await?;
        send_compressed_entry(stream, file, buf, entry).await
    } else if is_method_supported(entry.method) {
        buf = send_header(stream, buf, entry, compression.as_ref()).await?;
        send_decompressed_entry(stream, file, buf, entry).await
    } else {
        tracing::error!("Unsupported compression method {:?}", entry.method);
        serve_not_implemented(stream).await.map(|()| buf)
    }
}

async fn send_header(
    stream: &mut OwnedWriteHalf<TcpStream>,
    mut buf: Buf,
    entry: &Entry,
    compression: Option<&ContentCompression>,
) -> Result<Buf, std::io::Error> {
    // Prepare header in buffer
    let mime_type = mime_guess::from_path(&entry.name).first_or_text_plain();
    tracing::debug!(?mime_type);
    let mut cur = Cursor::new(buf);
    cur.write_all(b"HTTP/1.1 200 OK\r\n")?;

    cur.write_all(b"Content-Type: ")?;
    cur.write_all(mime_type.essence_str().as_bytes())?;
    cur.write_all(b"\r\n")?;

    if let Some(ContentCompression { encoding, .. }) = compression {
        cur.write_all(b"Content-Encoding: ")?;
        cur.write_all(encoding.as_bytes())?;
        cur.write_all(b"\r\n")?;
    }

    cur.write_all(b"Content-Length: ")?;
    let content_length = match compression {
        Some(ContentCompression { extra_len, .. }) => entry.compressed_size + extra_len,
        None => entry.uncompressed_size,
    };
    let mut intbuf = itoa::Buffer::new();
    cur.write_all(intbuf.format(content_length).as_bytes())?;

    cur.write_all(b"\r\nETag: \"")?;
    let mut etag = [0u8; 8];
    const_hex::encode_to_slice(entry.crc32.to_le_bytes(), &mut etag)
        .expect("u32 should always be encodable to 8 hex chars");
    cur.write_all(&etag)?;
    cur.write_all(b"\"\r\n")?;

    cur.write_all(b"Cache-control: max-age=180, public\r\n\r\n")?;
    // Send header
    let n = usize::try_from(cur.position()).unwrap();
    buf = cur.into_inner();
    let mut slice = IoBufMut::slice_mut(buf, 0..n);
    let res;
    (res, slice) = stream.write_all(slice).await;
    buf = slice.into_inner();
    res?;
    Ok(buf)
}

async fn send_compressed_entry(
    stream: &mut OwnedWriteHalf<TcpStream>,
    file: &File,
    buf: Buf,
    entry: &Entry,
) -> Result<Buf, std::io::Error> {
    let mut len = usize::try_from(entry.compressed_size).unwrap();
    let mut gzip_trailer = [0u8; 8];
    if entry.method == Method::Deflate {
        // Write gzip header for deflate
        stream
            .write_all(&[0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF])
            .await
            .0?;
        (gzip_trailer[0..4]).copy_from_slice(&entry.crc32.to_le_bytes());
        (gzip_trailer[4..8]).copy_from_slice(&entry.uncompressed_size.to_le_bytes()[0..4]);
    }

    let (mut offset, mut buf) = find_entry_compressed_data(file, entry, Some(buf)).await?;
    tracing::debug!("found compressed data");
    while len > 0 {
        let bytes_to_read = len.min(buf.len());
        let (res, slice) = file
            .read_at(IoBufMut::slice_mut(buf, 0..bytes_to_read), offset)
            .await;
        let n = res?;
        buf = slice.into_inner();
        offset += n as u64;
        len -= n;
        let slice = IoBufMut::slice_mut(buf, 0..n);
        let (res, slice) = stream.write_all(slice).await;
        res?;
        buf = slice.into_inner();
    }

    if entry.method == Method::Deflate {
        (buf[..8]).copy_from_slice(&gzip_trailer);
        let slice = IoBufMut::slice_mut(buf, ..8);
        let (res, slice) = stream.write_all(slice).await;
        res?;
        buf = slice.into_inner();
    }

    Ok(buf)
}

async fn send_decompressed_entry(
    stream: &mut OwnedWriteHalf<TcpStream>,
    file: &File,
    mut buf: Buf,
    entry: &Entry,
) -> Result<Buf, std::io::Error> {
    let mut res;
    let mut offset = entry.header_offset;
    let mut fsm = EntryFsm::new(None, None);
    loop {
        if fsm.wants_read() {
            let dst = fsm.space();
            let max_read = dst.len().min(buf.len());
            let mut slice = IoBufMut::slice_mut(buf, 0..max_read);
            (res, slice) = file.read_at(slice, offset).await;
            let n = res?;
            (dst[..n]).copy_from_slice(&slice[..n]);
            fsm.fill(n);
            offset += n as u64;
            buf = slice.into_inner();
        }
        fsm = match fsm.process(&mut buf) {
            Ok(rc_zip::fsm::FsmResult::Continue((fsm, outcome))) => {
                let bytes_out = outcome.bytes_written;
                let mut slice = IoBufMut::slice_mut(buf, 0..bytes_out);
                (res, slice) = stream.write_all(slice).await;
                res?;
                buf = slice.into_inner();
                fsm
            }
            Ok(rc_zip::fsm::FsmResult::Done(_buffer)) => return Ok(buf),
            Err(err) => {
                tracing::error!("ERR {:?}", err);
                return Err(std::io::Error::other(err));
            }
        }
    }
}

fn is_method_supported(method: Method) -> bool {
    match method {
        Method::Store => true,
        #[cfg(feature = "deflate")]
        Method::Deflate => true,
        #[cfg(feature = "zstd")]
        Method::Zstd => true,
        #[cfg(feature = "deflate64")]
        Method::Deflate64 => true,
        #[cfg(feature = "bzip2")]
        Method::Bzip2 => true,
        #[cfg(feature = "lzma")]
        Method::Lzma => true,
        _ => false,
    }
}

const FRAGMENT: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'<').add(b'>').add(b'`');

#[tracing::instrument(skip_all, level = "info", err)]
async fn serve_index(
    is_root: bool,
    entries: &[fstree::FsTreeNode],
    stream: &mut OwnedWriteHalf<TcpStream>,
    mut buf: Buf,
) -> std::io::Result<Buf> {
    const INDEX_PREAMBLE: &str = include_str!("index.html");
    const DIGIT_COUNT: usize = size_of::<usize>() * 2;
    const CHUNK_SIZE_LEN: usize = DIGIT_COUNT + 2;
    let buflen = buf.len();
    debug_assert!(
        buf.len() > INDEX_PREAMBLE.len() * 2,
        "buffer should have ample space"
    );

    stream.write_all("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n").await.0?;

    async fn flush(
        stream: &mut OwnedWriteHalf<TcpStream>,
        mut buf: Buf,
        len: usize,
    ) -> std::io::Result<Buf> {
        buf[CHUNK_SIZE_LEN + len] = b'\r';
        buf[CHUNK_SIZE_LEN + len + 1] = b'\n';

        const_hex::encode_to_slice(len.to_be_bytes(), &mut buf[0..DIGIT_COUNT]).unwrap();
        buf[DIGIT_COUNT] = b'\r';
        buf[DIGIT_COUNT + 1] = b'\n';

        let mut slice = IoBufMut::slice_mut(buf, 0..len + CHUNK_SIZE_LEN + 2);
        let res;
        (res, slice) = stream.write_all(slice).await;
        buf = slice.into_inner();
        res?;
        Ok(buf)
    }

    const_hex::encode_to_slice(0usize.to_be_bytes(), &mut buf[0..DIGIT_COUNT]).unwrap();
    buf[DIGIT_COUNT] = b'\r';
    buf[DIGIT_COUNT + 1] = b'\n';

    let mut cur = Cursor::new(&mut buf[CHUNK_SIZE_LEN..buflen - 2]);
    cur.write_all(INDEX_PREAMBLE.as_bytes())?;
    if !is_root {
        cur.write_all(b"<li class=top><a href=\"..\">..</a>\n")?;
    }

    for entry in entries {
        if let fstree::FsTreeNode::Dir { name, .. } = entry {
            let mut prepos = cur.position();
            while let Err(e) = cur.write_fmt(format_args!(
                "<li class=dir><a href=\"./{name_url}/\">{name_html}</a>\n",
                name_url = utf8_percent_encode(name, FRAGMENT),
                name_html = v_htmlescape::escape(name),
            )) {
                if prepos == 0 {
                    return Err(e);
                }
                buf = flush(stream, buf, usize::try_from(prepos).unwrap()).await?;
                cur = Cursor::new(&mut buf[CHUNK_SIZE_LEN..buflen - 2]);
                prepos = cur.position();
            }
        }
    }
    for entry in entries {
        if let fstree::FsTreeNode::File { name, .. } = entry {
            let mut prepos = cur.position();
            while let Err(e) = cur.write_fmt(format_args!(
                "<li><a href=\"./{name_url}\">{name_html}</a>\n",
                name_url = utf8_percent_encode(name, FRAGMENT),
                name_html = v_htmlescape::escape(name),
            )) {
                if prepos == 0 {
                    return Err(e);
                }
                buf = flush(stream, buf, usize::try_from(prepos).unwrap()).await?;
                cur = Cursor::new(&mut buf[CHUNK_SIZE_LEN..buflen - 2]);
                prepos = cur.position();
            }
        }
    }

    let len = cur.position();
    buf = flush(stream, buf, usize::try_from(len).unwrap()).await?;
    stream.write_all("0\r\n\r\n").await.0?;
    Ok(buf)
}
