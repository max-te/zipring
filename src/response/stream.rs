use std::io::{Cursor, Write};

use crate::borrowed_file::BorrowedFile;
use monoio::{
    buf::IoBufMut,
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

const HTTP_CHUNK_TERMINATOR: &[u8] = b"0\r\n\r\n";

/// Frames the `len` payload bytes staged at `prefix + HTTP_CHUNK_SIZE_LEN` as an HTTP
/// chunk and writes the buffer from its very start, so that whatever occupies
/// `buf[..prefix]` -- the response header, on the first chunk -- leaves in the same write.
///
/// The final chunk carries the terminator along with it.
async fn flush_chunk<W: AsyncWriteRent>(
    stream: &mut W,
    mut buf: Buf,
    prefix: usize,
    len: usize,
    is_last: bool,
) -> std::io::Result<Buf> {
    let payload_end = prefix + HTTP_CHUNK_SIZE_LEN + len;
    buf[payload_end] = b'\r';
    buf[payload_end + 1] = b'\n';

    const_hex::encode_to_slice(
        len.to_be_bytes(),
        &mut buf[prefix..prefix + HTTP_CHUNK_DIGIT_COUNT],
    )
    .expect("chunk length should be encodable in DIGIT_COUNT hex digits");
    buf[prefix + HTTP_CHUNK_DIGIT_COUNT] = b'\r';
    buf[prefix + HTTP_CHUNK_DIGIT_COUNT + 1] = b'\n';

    let mut end = payload_end + 2;
    if is_last {
        buf[end..end + HTTP_CHUNK_TERMINATOR.len()].copy_from_slice(HTTP_CHUNK_TERMINATOR);
        end += HTTP_CHUNK_TERMINATOR.len();
    }

    let mut slice = IoBufMut::slice_mut(buf, 0..end);
    let res;
    (res, slice) = stream.write_all(slice).await;
    res?;
    Ok(slice.into_inner())
}

const GZIP_HEADER: [u8; 10] = [0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF];
const GZIP_TRAILER_LEN: usize = 8;

/// Lays the response header out at the front of `buf`, returning its length.
///
/// Nothing is sent here: the body is staged behind the header so that the whole
/// response can leave in a single write.
pub(super) fn write_entry_header(
    buf: Buf,
    entry: &Entry,
    compression: Option<&ContentCompression>,
) -> std::io::Result<(Buf, usize)> {
    let mime_type = mime_guess::from_path(&entry.name).first_or_octet_stream();
    tracing::debug!(?mime_type);
    let mut cur = Cursor::new(buf);
    Write::write_all(&mut cur, b"HTTP/1.1 200 OK\r\n")?;

    Write::write_all(&mut cur, b"Content-Type: ")?;
    Write::write_all(&mut cur, mime_type.essence_str().as_bytes())?;
    Write::write_all(&mut cur, b"\r\n")?;

    if let Some(ContentCompression { encoding, .. }) = compression {
        Write::write_all(&mut cur, b"Content-Encoding: ")?;
        Write::write_all(&mut cur, encoding.as_bytes())?;
        Write::write_all(&mut cur, b"\r\n")?;
    }

    Write::write_all(&mut cur, b"Content-Length: ")?;
    let content_length = match compression {
        Some(ContentCompression { extra_len, .. }) => entry.compressed_size + extra_len,
        None => entry.uncompressed_size,
    };
    let mut intbuf = itoa::Buffer::new();
    Write::write_all(&mut cur, intbuf.format(content_length).as_bytes())?;

    Write::write_all(&mut cur, b"\r\nETag: \"")?;
    let etag = encode_crc32(entry.crc32);
    Write::write_all(&mut cur, &etag)?;
    Write::write_all(&mut cur, b"\"\r\n")?;

    Write::write_all(&mut cur, b"Cache-control: max-age=180, public\r\n\r\n")?;

    let len = usize::try_from(cur.position()).expect("response should be adressable with usize");
    Ok((cur.into_inner(), len))
}

/// Where a chunk's payload has to stop: short of its own trailing CRLF and of the
/// terminator that the final chunk carries.
const fn chunk_limit(buflen: usize) -> usize {
    buflen - 2 - HTTP_CHUNK_TERMINATOR.len()
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

    async fn send_compressed_entry(
        mut self,
        file: &BorrowedFile<'_>,
        entry: &Entry,
        head_len: usize,
    ) -> std::io::Result<Self> {
        let mut buf = self.buf;
        let is_gzip = entry.method == CompressionMethod::Deflate;

        let mut prefix_len = head_len;
        let mut gzip_trailer = [0u8; GZIP_TRAILER_LEN];
        if is_gzip {
            buf[prefix_len..prefix_len + GZIP_HEADER.len()].copy_from_slice(&GZIP_HEADER);
            prefix_len += GZIP_HEADER.len();
            (gzip_trailer[0..4]).copy_from_slice(&entry.crc32.to_le_bytes());
            (gzip_trailer[4..8]).copy_from_slice(&entry.uncompressed_size.to_le_bytes()[0..4]);
        }

        let mut len =
            usize::try_from(entry.compressed_size).expect("entry size should fit into usize");
        let mut offset;
        (offset, buf) = find_entry_compressed_data(file, entry, buf, prefix_len).await?;
        tracing::debug!("found compressed data");

        loop {
            // Leaving room for the trailer lets a body that just fits still ship in one write.
            let room = buf.len() - prefix_len - GZIP_TRAILER_LEN;
            let bytes_to_read = len.min(room);
            let (res, slice) = file
                .read_at(
                    IoBufMut::slice_mut(buf, prefix_len..prefix_len + bytes_to_read),
                    offset,
                )
                .await;
            let n = res?;
            buf = slice.into_inner();
            if n == 0 && len > 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "entry ends before its compressed size",
                ));
            }
            offset += n as u64;
            len -= n;

            let mut end = prefix_len + n;
            if len == 0 && is_gzip {
                buf[end..end + GZIP_TRAILER_LEN].copy_from_slice(&gzip_trailer);
                end += GZIP_TRAILER_LEN;
            }
            let slice = IoBufMut::slice_mut(buf, 0..end);
            let (res, slice) = self.stream.write_all(slice).await;
            res?;
            buf = slice.into_inner();

            prefix_len = 0;
            if len == 0 {
                break;
            }
        }

        self.buf = buf;
        Ok(self)
    }
    async fn send_decompressed_entry(
        mut self,
        file: &BorrowedFile<'_>,
        entry: &Entry,
        head_len: usize,
    ) -> std::io::Result<Self> {
        let mut buf = self.buf;
        let mut res;
        let mut offset = entry.header_offset;
        let mut prefix_len = head_len;
        let mut fsm = EntryFsm::new(None, None);
        loop {
            if fsm.wants_read() {
                let dst = fsm.space();
                let max_read = dst.len().min(buf.len() - prefix_len);
                let mut slice = IoBufMut::slice_mut(buf, prefix_len..prefix_len + max_read);
                (res, slice) = file.read_at(slice, offset).await;
                let n = res?;
                (dst[..n]).copy_from_slice(&slice[..n]);
                fsm.fill(n);
                offset += n as u64;
                buf = slice.into_inner();
            }
            fsm = match fsm.process(&mut buf[prefix_len..]) {
                Ok(rc_zip::fsm::FsmResult::Continue((fsm, outcome))) => {
                    if outcome.bytes_written > 0 {
                        let mut slice =
                            IoBufMut::slice_mut(buf, 0..prefix_len + outcome.bytes_written);
                        (res, slice) = self.stream.write_all(slice).await;
                        res?;
                        buf = slice.into_inner();
                        prefix_len = 0;
                    }
                    fsm
                }
                Ok(rc_zip::fsm::FsmResult::Done(_buffer)) => break,
                Err(err) => {
                    tracing::error!("ERR {:?}", err);
                    return Err(std::io::Error::other(err));
                }
            }
        }
        if prefix_len > 0 {
            // An entry without any content still owes its header.
            let mut slice = IoBufMut::slice_mut(buf, 0..prefix_len);
            (res, slice) = self.stream.write_all(slice).await;
            res?;
            buf = slice.into_inner();
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
        const INDEX_HEADER: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n";
        let mut buf = self.buf;
        let buflen = buf.len();
        debug_assert!(
            buf.len() > INDEX_PREAMBLE.len() * 2,
            "buffer should have ample space"
        );

        // The header rides along with the first chunk.
        let mut prefix = INDEX_HEADER.len();
        buf[..prefix].copy_from_slice(INDEX_HEADER);

        let mut cur = Cursor::new(&mut buf[prefix + HTTP_CHUNK_SIZE_LEN..chunk_limit(buflen)]);
        Write::write_all(&mut cur, INDEX_PREAMBLE.as_bytes())?;
        if !is_root {
            Write::write_all(&mut cur, b"<li class=top><a href=\"..\">..</a>\n")?;
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
                        prefix,
                        usize::try_from(prepos)
                            .expect("response buffer should be adressable in usize"),
                        false,
                    )
                    .await?;
                    prefix = 0;
                    cur = Cursor::new(&mut buf[HTTP_CHUNK_SIZE_LEN..chunk_limit(buflen)]);
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
                        prefix,
                        usize::try_from(prepos)
                            .expect("response buffer should be adressable in usize"),
                        false,
                    )
                    .await?;
                    prefix = 0;
                    cur = Cursor::new(&mut buf[HTTP_CHUNK_SIZE_LEN..chunk_limit(buflen)]);
                    prepos = cur.position();
                }
            }
        }

        let len = cur.position();
        self.buf = flush_chunk(
            self.stream,
            buf,
            prefix,
            usize::try_from(len).expect("response buffer should be adressable in usize"),
            true,
        )
        .await?;
        Ok(self)
    }

    #[tracing::instrument(skip_all, level = "debug")]
    async fn serve_entry(
        mut self,
        file: &BorrowedFile<'_>,
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

        if compression.is_some() || is_method_supported(entry.method) {
            let (buf, head_len) = write_entry_header(self.buf, entry, compression.as_ref())?;
            self.buf = buf;
            if compression.is_some() {
                self.send_compressed_entry(file, entry, head_len).await
            } else {
                self.send_decompressed_entry(file, entry, head_len).await
            }
        } else {
            tracing::error!("Unsupported compression method {:?}", entry.method);
            self.send_status_empty("500 Unsupported Compression").await
        }
    }

    pub async fn serve_node(
        self,
        file: &BorrowedFile<'_>,
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
        const NOT_MODIFIED_TEMPLATE: &[u8] =
            b"HTTP/1.1 304 Not Modified\r\nETag: \"xxxxxxxx\"\r\n\r\n";
        const CRC_OFFSET: usize =
            position(NOT_MODIFIED_TEMPLATE, b'x').expect("template sould have x");

        let mut buf = self.buf;
        buf[0..NOT_MODIFIED_TEMPLATE.len()].copy_from_slice(NOT_MODIFIED_TEMPLATE);

        let etag = encode_crc32(crc32);
        buf[CRC_OFFSET..{ CRC_OFFSET + etag.len() }].copy_from_slice(&etag);

        let slice = IoBufMut::slice_mut(buf, 0..NOT_MODIFIED_TEMPLATE.len());
        let (res, slice) = self.stream.write_all(slice).await;
        self.buf = slice.into_inner();
        res?;
        Ok(self)
    }

    async fn send_status_empty(mut self, status: &'static str) -> std::io::Result<Self> {
        let mut cur = Cursor::new(self.buf);
        Write::write_all(&mut cur, b"HTTP/1.1 ")?;
        Write::write_all(&mut cur, status.as_bytes())?;
        Write::write_all(&mut cur, b"\r\nContent-Length: 0\r\n\r\n")?;
        let len = usize::try_from(cur.position()).expect("status should be adressable with usize");

        let mut slice = IoBufMut::slice_mut(cur.into_inner(), 0..len);
        let res;
        (res, slice) = self.stream.write_all(slice).await;
        self.buf = slice.into_inner();
        res?;
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
