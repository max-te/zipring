use crate::fstree;
use crate::rc_zip_monoio::find_entry_compressed_data;
use monoio::buf::IoBufMut;
use monoio::fs::File;
use monoio::io::AsyncWriteRentExt;
use monoio::io::OwnedWriteHalf;
use monoio::net::TcpStream;
use rc_zip::fsm::EntryFsm;
use rc_zip::parse::Entry;
use rc_zip::parse::Method;
use std::io::Cursor;
use std::io::Write;

pub(crate) async fn serve_node(
    stream: &mut OwnedWriteHalf<TcpStream>,
    file: &File,
    buf: Box<[u8]>,
    node: &fstree::FsTreeNode,
) -> Result<Box<[u8]>, std::io::Error> {
    match node {
        fstree::FsTreeNode::Dir {
            is_root, children, ..
        } => serve_index(*is_root, children, stream).await.map(|_| buf),
        fstree::FsTreeNode::File { entry, .. } => serve_entry(stream, file, buf, entry).await,
    }
}

pub(crate) async fn serve_entry(
    stream: &mut OwnedWriteHalf<TcpStream>,
    file: &File,
    mut buf: Box<[u8]>,
    entry: &Entry,
) -> Result<Box<[u8]>, std::io::Error> {
    let mime_type = mime_guess::from_path(&entry.name).first_or_text_plain();
    tracing::debug!(?mime_type);
    let mut send_compressed = false;
    let mut compression_header_size = 0;

    // Assemble header
    let mut cur = Cursor::new(buf);
    cur.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: ")?;
    cur.write_all(mime_type.essence_str().as_bytes())?;
    cur.write_all(b"\r\n")?;

    tracing::debug!(?entry.method);
    match entry.method {
        Method::Deflate => {
            cur.write_all(b"Content-Encoding: gzip\r\n")?;
            send_compressed = true;
            compression_header_size = 18;
        }
        Method::Zstd => {
            cur.write_all(b"Content-Encoding: zstd\r\n")?;
            send_compressed = true;
        }
        _ => (),
    }

    cur.write_all(b"Content-Length: ")?;
    let mut intbuf = itoa::Buffer::new();
    cur.write_all(
        intbuf
            .format(if send_compressed {
                entry.compressed_size + compression_header_size
            } else {
                entry.uncompressed_size
            })
            .as_bytes(),
    )?;

    cur.write_all(b"\r\nETag: \"")?;
    let mut etag = [0u8; 8];
    const_hex::encode_to_slice(entry.crc32.to_le_bytes(), &mut etag)
        .expect("u32 should always be encodable to 8 hex chars");
    cur.write_all(&etag)?;

    cur.write_all(b"\"\r\n\r\n")?;

    // Send header
    let n = cur.position() as usize;
    buf = cur.into_inner();
    let mut slice = IoBufMut::slice_mut(buf, 0..n);
    let res;
    (res, slice) = stream.write_all(slice).await;
    buf = slice.into_inner();
    res?;

    // Send body

    buf = if send_compressed {
        send_compressed_entry(stream, file, buf, entry).await?
    } else {
        send_decompressed_entry(stream, file, buf, entry).await?
    };
    tracing::debug!("finished serving entry");
    Ok(buf)
}

pub(crate) async fn send_compressed_entry(
    stream: &mut OwnedWriteHalf<TcpStream>,
    file: &File,
    buf: Box<[u8]>,
    entry: &Entry,
) -> Result<Box<[u8]>, std::io::Error> {
    let mut len = entry.compressed_size as usize;
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

    (buf[..8]).copy_from_slice(&gzip_trailer);
    let slice = IoBufMut::slice_mut(buf, ..8);
    let (res, slice) = stream.write_all(slice).await;
    res?;
    Ok(slice.into_inner())
}

pub(crate) async fn send_decompressed_entry(
    stream: &mut OwnedWriteHalf<TcpStream>,
    file: &File,
    mut buf: Box<[u8]>,
    entry: &Entry,
) -> Result<Box<[u8]>, std::io::Error> {
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

pub(crate) static INDEX_PREAMBLE: &str = include_str!("index.html");

pub(crate) async fn serve_index(
    is_root: bool,
    entries: &[fstree::FsTreeNode],
    stream: &mut OwnedWriteHalf<TcpStream>,
) -> std::io::Result<()> {
    let mut listing = Vec::<u8>::with_capacity(32 * 1024);

    listing.write_all(INDEX_PREAMBLE.as_bytes())?;
    if !is_root {
        listing.write_all(b"<li class=top><a href=\"..\">..</a>\n")?;
    }
    for entry in entries {
        if let fstree::FsTreeNode::Dir { .. } = entry {
            listing.write_fmt(format_args!(
                "<li class=dir><a href=\"./{name}/\">{name}</a>\n",
                name = entry.name()
            ))?;
        }
    }
    for entry in entries {
        if let fstree::FsTreeNode::File { .. } = entry {
            listing.write_fmt(format_args!(
                "<li><a href=\"./{name}\">{name}</a>\n",
                name = entry.name()
            ))?;
        }
    }

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
        listing.len()
    )
    .into_bytes();

    stream.write_all(header).await.0?;
    stream.write_all(listing).await.0?;
    Ok(())
}
