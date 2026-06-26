use std::io::{Cursor, Write};

use monoio::{
    buf::IoBufMut,
    fs::File,
    io::{AsyncWriteRent, AsyncWriteRentExt},
};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use rc_zip::{Entry, fsm::EntryFsm, parse::Method as CompressionMethod};

use crate::{
    Buf,
    fstree::FsTreeNode,
    rc_zip_monoio::{find_entry_compressed_data, is_method_supported},
    request::AcceptedEncodings,
    response::status::HttpStatus,
};

const HTTP_CHUNK_DIGIT_COUNT: usize = size_of::<usize>() * 2;
const HTTP_CHUNK_SIZE_LEN: usize = HTTP_CHUNK_DIGIT_COUNT + b"\r\n".len();

async fn flush_chunk<'w, W: AsyncWriteRent>(
    stream: &'w mut W,
    mut buf: Buf,
    len: usize,
) -> std::io::Result<Buf> {
    buf[HTTP_CHUNK_SIZE_LEN + len] = b'\r';
    buf[HTTP_CHUNK_SIZE_LEN + len + 1] = b'\n';

    const_hex::encode_to_slice(len.to_be_bytes(), &mut buf[0..HTTP_CHUNK_DIGIT_COUNT])
        .expect("chunk length should be encodable in DIGIT_COUNT hex digits");
    buf[HTTP_CHUNK_DIGIT_COUNT] = b'\r';
    buf[HTTP_CHUNK_DIGIT_COUNT + 1] = b'\n';

    let mut slice = IoBufMut::slice_mut(buf, 0..len + HTTP_CHUNK_SIZE_LEN + 2);
    let res;
    (res, slice) = stream.write_all(slice).await;
    res?;
    Ok(slice.into_inner())
}

const URI_FRAGMENT_ENCODING_SET: &AsciiSet =
    &CONTROLS.add(b' ').add(b'"').add(b'<').add(b'>').add(b'`');

pub struct ResponseStream<'w, W: AsyncWriteRent> {
    stream: &'w mut W,
    buf: Buf,
}

impl<'w, W: AsyncWriteRent> ResponseStream<'w, W> {
    pub fn new(stream: &'w mut W, buf: Buf) -> Self {
        Self { stream, buf }
    }
    pub fn into_buf(self) -> Buf {
        self.buf
    }

    pub(super) async fn send_entry_header(
        mut self,
        entry: &Entry,
        compression: Option<&ContentCompression>,
    ) -> std::io::Result<Self> {
        let mut buf = self.buf;
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
        let etag = encode_crc32(entry.crc32);
        cur.write_all(&etag)?;
        cur.write_all(b"\"\r\n")?;

        cur.write_all(b"Cache-control: max-age=180, public\r\n\r\n")?;
        // Send header
        let n = usize::try_from(cur.position()).expect("response should be adressable with usize");
        buf = cur.into_inner();

        let mut slice = IoBufMut::slice_mut(buf, 0..n);
        let res;
        (res, slice) = self.stream.write_all(slice).await;
        buf = slice.into_inner();
        res?;
        self.buf = buf;
        Ok(self)
    }
    async fn send_compressed_entry(mut self, file: &File, entry: &Entry) -> std::io::Result<Self> {
        let mut buf = self.buf;

        let mut len =
            usize::try_from(entry.compressed_size).expect("entry size should fit into usize");
        let mut gzip_trailer = [0u8; 8];
        if entry.method == CompressionMethod::Deflate {
            // Write gzip header for deflate
            self.stream
                .write_all(&[0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF])
                .await
                .0?;
            (gzip_trailer[0..4]).copy_from_slice(&entry.crc32.to_le_bytes());
            (gzip_trailer[4..8]).copy_from_slice(&entry.uncompressed_size.to_le_bytes()[0..4]);
        }

        let mut offset;
        (offset, buf) = find_entry_compressed_data(file, entry, Some(buf)).await?;
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
            let (res, slice) = self.stream.write_all(slice).await;
            res?;
            buf = slice.into_inner();
        }

        if entry.method == CompressionMethod::Deflate {
            (buf[..8]).copy_from_slice(&gzip_trailer);
            let slice = IoBufMut::slice_mut(buf, ..8);
            let (res, slice) = self.stream.write_all(slice).await;
            res?;
            buf = slice.into_inner();
        }

        self.buf = buf;
        Ok(self)
    }
    async fn send_decompressed_entry(
        mut self,
        file: &File,
        entry: &Entry,
    ) -> std::io::Result<Self> {
        let mut buf = self.buf;
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
                    (res, slice) = self.stream.write_all(slice).await;
                    res?;
                    buf = slice.into_inner();
                    fsm
                }
                Ok(rc_zip::fsm::FsmResult::Done(_buffer)) => break,
                Err(err) => {
                    tracing::error!("ERR {:?}", err);
                    return Err(std::io::Error::other(err));
                }
            }
        }
        self.buf = buf;
        Ok(self)
    }

    #[tracing::instrument(skip_all, level = "info", err)]
    pub(super) async fn serve_index(
        mut self,
        is_root: bool,
        entries: &[FsTreeNode],
    ) -> std::io::Result<Self> {
        const INDEX_PREAMBLE: &str = include_str!("index.html");
        let mut buf = self.buf;
        let buflen = buf.len();
        debug_assert!(
            buf.len() > INDEX_PREAMBLE.len() * 2,
            "buffer should have ample space"
        );

        self.stream.write_all("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n").await.0?;

        const_hex::encode_to_slice(0usize.to_be_bytes(), &mut buf[0..HTTP_CHUNK_DIGIT_COUNT])
            .expect("0 should be hex-encodable");
        buf[HTTP_CHUNK_DIGIT_COUNT] = b'\r';
        buf[HTTP_CHUNK_DIGIT_COUNT + 1] = b'\n';

        let mut cur = Cursor::new(&mut buf[HTTP_CHUNK_SIZE_LEN..buflen - 2]);
        cur.write_all(INDEX_PREAMBLE.as_bytes())?;
        if !is_root {
            cur.write_all(b"<li class=top><a href=\"..\">..</a>\n")?;
        }

        // List directories first
        for entry in entries {
            if let FsTreeNode::Dir { name, .. } = entry {
                let mut prepos = cur.position();
                while let Err(e) = cur.write_fmt(format_args!(
                    "<li class=dir><a href=\"./{name_url}/\">{name_html}</a>\n",
                    name_url = utf8_percent_encode(name, URI_FRAGMENT_ENCODING_SET),
                    name_html = v_htmlescape::escape(name),
                )) {
                    if prepos == 0 {
                        return Err(e);
                    }
                    buf = flush_chunk(
                        self.stream,
                        buf,
                        usize::try_from(prepos)
                            .expect("response buffer should be adressable in usize"),
                    )
                    .await?;
                    cur = Cursor::new(&mut buf[HTTP_CHUNK_SIZE_LEN..buflen - 2]);
                    prepos = cur.position();
                }
            }
        }
        // Then list all files
        for entry in entries {
            if let FsTreeNode::File { name, .. } = entry {
                let mut prepos = cur.position();
                while let Err(e) = cur.write_fmt(format_args!(
                    "<li><a href=\"./{name_url}\">{name_html}</a>\n",
                    name_url = utf8_percent_encode(name, URI_FRAGMENT_ENCODING_SET),
                    name_html = v_htmlescape::escape(name),
                )) {
                    if prepos == 0 {
                        return Err(e);
                    }
                    buf = flush_chunk(
                        self.stream,
                        buf,
                        usize::try_from(prepos)
                            .expect("response buffer should be adressable in usize"),
                    )
                    .await?;
                    cur = Cursor::new(&mut buf[HTTP_CHUNK_SIZE_LEN..buflen - 2]);
                    prepos = cur.position();
                }
            }
        }

        let len = cur.position();
        self.buf = flush_chunk(self.stream, buf, usize::try_from(len).unwrap()).await?;
        self.stream.write_all("0\r\n\r\n").await.0?;
        Ok(self)
    }

    #[tracing::instrument(skip_all, level = "debug")]
    async fn serve_entry(
        self,
        file: &File,
        entry: &Entry,
        accepted_encodings: AcceptedEncodings,
    ) -> std::io::Result<Self> {
        let compression = match entry.method {
            CompressionMethod::Deflate if accepted_encodings.gzip => Some(ContentCompression {
                encoding: "gzip",
                extra_len: 18,
            }),
            CompressionMethod::Zstd if accepted_encodings.zstd => Some(ContentCompression {
                encoding: "zstd",
                extra_len: 0,
            }),
            _ => None,
        };

        if compression.is_some() {
            self.send_entry_header(entry, compression.as_ref())
                .await?
                .send_compressed_entry(file, entry)
                .await
        } else if is_method_supported(entry.method) {
            self.send_entry_header(entry, compression.as_ref())
                .await?
                .send_decompressed_entry(file, entry)
                .await
        } else {
            tracing::error!("Unsupported compression method {:?}", entry.method);
            self.send_status_empty("500 Unsupported Compression").await
        }
    }

    pub async fn serve_node(
        self,
        file: &File,
        node: &FsTreeNode,
        accepted_encodings: AcceptedEncodings,
    ) -> std::io::Result<Self> {
        match node {
            FsTreeNode::Dir {
                name: _,
                entry: _,
                is_root,
                children,
                index_html_index,
            } => {
                if let Some(idx) = index_html_index
                    && let FsTreeNode::File { entry, .. } = &children[*idx]
                {
                    self.serve_entry(file, entry, accepted_encodings).await
                } else {
                    self.serve_index(*is_root, children).await
                }
            }
            FsTreeNode::File { entry, .. } => {
                self.serve_entry(file, entry, accepted_encodings).await
            }
        }
    }

    pub async fn serve_not_modified(mut self, crc32: u32) -> std::io::Result<Self> {
        let mut buf = self.buf;
        const NOT_MODIFIED_TEMPLATE: &[u8] =
            b"HTTP/1.1 304 Not Modified\r\nETag: \"xxxxxxxx\"\r\n\r\n";
        const CRC_OFFSET: usize =
            position(NOT_MODIFIED_TEMPLATE, b'x').expect("template sould have x");

        buf[0..NOT_MODIFIED_TEMPLATE.len()].copy_from_slice(NOT_MODIFIED_TEMPLATE);

        let etag = encode_crc32(crc32);
        buf[CRC_OFFSET..{ CRC_OFFSET + etag.len() }].copy_from_slice(&etag);

        let slice = IoBufMut::slice_mut(buf, 0..NOT_MODIFIED_TEMPLATE.len());
        let (res, slice) = self.stream.write_all(slice).await;
        self.buf = slice.into_inner();
        res?;
        Ok(self)
    }

    async fn send_status_empty(self, status: &'static str) -> std::io::Result<Self> {
        self.stream.write_all(b"HTTP/1.1 ").await.0?;
        self.stream.write_all(status).await.0?;
        self.stream
            .write_all(b"\r\nContent-Length: 0\r\n\r\n")
            .await
            .0?;
        Ok(self)
    }

    pub async fn serve_status(self, status: HttpStatus) -> std::io::Result<Self> {
        self.send_status_empty(status.as_str()).await
    }
}

fn encode_crc32(crc32: u32) -> [u8; const { size_of::<u32>() * 2 }] {
    let mut etag = [0; _];
    const_hex::encode_to_slice(crc32.to_be_bytes(), &mut etag)
        .expect("u32 should always be encodable to 8 hex chars");
    etag
}

pub(super) struct ContentCompression {
    pub extra_len: u64,
    pub encoding: &'static str,
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
