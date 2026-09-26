use std::fmt;
use std::io::{Cursor, Write};
use std::ops::RangeBounds;

use compio::BufResult;
use compio::buf::{IntoInner, IoBuf, buf_try};
use compio::fs::File;
use compio::io::{AsyncReadAt, AsyncReadAtExt, AsyncWriteExt};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use rc_zip::{
    Entry,
    fsm::{EntryFsm, FsmResult},
    parse::Method as CompressionMethod,
};

use crate::{
    Buf,
    buf_result::{bail_traced, buf_try_traced},
    fstree::FsTreeNode,
    rc_zip_compio::{find_entry_compressed_data, is_method_supported},
    request::AcceptedEncodings,
    response::status::HttpStatus,
};

const HTTP_CHUNK_DIGIT_COUNT: usize = size_of::<usize>() * 2;
const HTTP_CHUNK_SIZE_LEN: usize = HTTP_CHUNK_DIGIT_COUNT + b"\r\n".len();

const HTTP_CHUNK_TERMINATOR: &[u8] = b"0\r\n\r\n";

/// Formats `args` into `buf` after its first `len` bytes and advances `len` past them.
///
/// If the text does not fit, `len` stays put and whatever was partially written
/// beyond it is to be disregarded.
fn append_fmt(buf: &mut [u8], len: &mut usize, args: fmt::Arguments<'_>) -> std::io::Result<()> {
    let mut cur = Cursor::new(&mut buf[*len..]);
    cur.write_fmt(args)?;
    *len += usize::try_from(cur.position()).expect("buffer should be adressable in usize");
    Ok(())
}

const GZIP_HEADER: [u8; 10] = [
    0x1f, 0x8b, // Magic number
    0x08, // Compression method = DEFLATE
    0x00, // No flags
    0x00, 0x00, 0x00, 0x00, // No MTIME
    0x00, // No extra flags
    0xFF, // Filesystem unknown
];
const GZIP_TRAILER_LEN: usize = 8;

/// Lays the response header out at the front of `buf`, returning its length.
///
/// Nothing is sent here: the body is staged behind the header so that the whole
/// response can leave in a single write.
pub(super) fn write_entry_header(
    buf: &mut [u8],
    entry: &Entry,
    compression: Option<&ContentCompression>,
) -> std::io::Result<usize> {
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

    Ok(usize::try_from(cur.position()).expect("response should be adressable with usize"))
}

/// Where a chunk's payload has to stop: short of its own trailing CRLF and of the
/// terminator that the final chunk carries.
const fn chunk_limit(buflen: usize) -> usize {
    buflen - 2 - HTTP_CHUNK_TERMINATOR.len()
}

/// An HTTP chunk being assembled in the buffer: `len` payload bytes staged at
/// `prefix + HTTP_CHUNK_SIZE_LEN`, behind whatever occupies `buf[..prefix]`.
#[derive(Clone, Copy)]
struct PendingChunk {
    prefix: usize,
    len: usize,
}

const URI_FRAGMENT_ENCODING_SET: &AsciiSet =
    &CONTROLS.add(b' ').add(b'"').add(b'<').add(b'>').add(b'`');

pub struct ResponseStream<'w, W: AsyncWriteExt> {
    stream: &'w mut W,
    buf: Buf,
}

impl<'w, W: AsyncWriteExt> ResponseStream<'w, W> {
    pub fn new(stream: &'w mut W, buf: Buf) -> Self {
        Self { stream, buf }
    }
    pub fn into_buf(self) -> Buf {
        self.buf
    }

    async fn write_buf(mut self, range: impl RangeBounds<usize>) -> BufResult<(), Self> {
        let BufResult(res, slice) = self.stream.write_all(self.buf.slice(range)).await;
        self.buf = slice.into_inner();
        BufResult(res, self)
    }

    async fn read_buf_at(
        mut self,
        file: &File,
        range: impl RangeBounds<usize>,
        offset: u64,
    ) -> BufResult<usize, Self> {
        let BufResult(res, slice) = file.read_at(self.buf.slice(range), offset).await;
        self.buf = slice.into_inner();
        BufResult(res, self)
    }

    async fn read_exact_buf_at(
        mut self,
        file: &File,
        range: impl RangeBounds<usize>,
        offset: u64,
    ) -> BufResult<(), Self> {
        let BufResult(res, slice) = file.read_exact_at(self.buf.slice(range), offset).await;
        self.buf = slice.into_inner();
        BufResult(res, self)
    }

    /// Frames `chunk` and writes the buffer from its very start, so that whatever occupies
    /// `buf[..chunk.prefix]` -- the response header, on the first chunk -- leaves in the
    /// same write.
    ///
    /// The final chunk carries the terminator along with it.
    async fn flush_chunk(mut self, chunk: PendingChunk, is_last: bool) -> BufResult<(), Self> {
        let PendingChunk { prefix, len } = chunk;
        let buf = &mut self.buf;
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

        self.write_buf(..end).await
    }

    /// Appends `args` to `chunk`, flushing it first when it cannot hold `args` whole,
    /// and returns the chunk left pending.
    async fn append_fmt_chunked(
        mut self,
        mut chunk: PendingChunk,
        args: fmt::Arguments<'_>,
    ) -> BufResult<PendingChunk, Self> {
        let limit = chunk_limit(self.buf.len());
        loop {
            let payload = &mut self.buf[chunk.prefix + HTTP_CHUNK_SIZE_LEN..limit];
            match append_fmt(payload, &mut chunk.len, args) {
                Ok(()) => return BufResult(Ok(chunk), self),
                // Not even an empty chunk can hold `args`.
                Err(e) if chunk.len == 0 => bail_traced!(e, self),
                Err(_) => {
                    ((), self) = buf_try_traced!(self.flush_chunk(chunk, false).await);
                }
            }
            chunk = PendingChunk { prefix: 0, len: 0 };
        }
    }

    #[tracing::instrument(skip_all)]
    async fn send_compressed_entry(
        mut self,
        file: &File,
        entry: &Entry,
        head_len: usize,
    ) -> BufResult<(), Self> {
        let is_gzip = entry.method == CompressionMethod::Deflate;

        let mut prefix_len = head_len;
        let mut gzip_trailer = [0u8; GZIP_TRAILER_LEN];
        if is_gzip {
            self.buf[prefix_len..prefix_len + GZIP_HEADER.len()].copy_from_slice(&GZIP_HEADER);
            prefix_len += GZIP_HEADER.len();
            (gzip_trailer[0..4]).copy_from_slice(&entry.crc32.to_le_bytes());
            (gzip_trailer[4..8]).copy_from_slice(&entry.uncompressed_size.to_le_bytes()[0..4]);
        }

        let mut len =
            usize::try_from(entry.compressed_size).expect("entry size should fit into usize");
        let BufResult(res, scratch) =
            find_entry_compressed_data(file, entry, self.buf.slice(prefix_len..)).await;
        self.buf = scratch.into_inner();
        let mut offset;
        (offset, self) = buf_try_traced!(res, self);
        tracing::debug!("found compressed data");

        let mut is_last = false;
        while !is_last {
            // Leaving room for the trailer lets a body that just fits still ship in one write.
            let room = self.buf.len() - prefix_len - GZIP_TRAILER_LEN;
            let n = len.min(room);
            ((), self) = buf_try_traced!(
                self.read_exact_buf_at(file, prefix_len..prefix_len + n, offset)
                    .await
            );
            offset += n as u64;
            len -= n;

            is_last = len == 0;
            let mut end = prefix_len + n;
            if is_last && is_gzip {
                self.buf[end..end + GZIP_TRAILER_LEN].copy_from_slice(&gzip_trailer);
                end += GZIP_TRAILER_LEN;
            }
            ((), self) = buf_try_traced!(self.write_buf(..end).await);
            prefix_len = 0;
        }

        BufResult(Ok(()), self)
    }

    #[tracing::instrument(skip_all)]
    async fn send_decompressed_entry(
        mut self,
        file: &File,
        entry: &Entry,
        head_len: usize,
    ) -> BufResult<(), Self> {
        let mut offset = entry.header_offset;
        let mut prefix_len = head_len;
        let mut fsm = EntryFsm::new(None, None);
        loop {
            if fsm.wants_read() {
                let available_space = fsm.space().len().min(self.buf.len() - prefix_len);
                let bytes_read;
                (bytes_read, self) = buf_try_traced!(
                    self.read_buf_at(file, prefix_len..prefix_len + available_space, offset)
                        .await
                );
                fsm.space()[..bytes_read]
                    .copy_from_slice(&self.buf[prefix_len..prefix_len + bytes_read]);
                fsm.fill(bytes_read);
                offset += bytes_read as u64;
            }
            let outcome;
            (fsm, outcome) = match fsm.process(&mut self.buf[prefix_len..]) {
                Ok(FsmResult::Continue(continued)) => continued,
                Ok(FsmResult::Done(_buffer)) => break,
                Err(err) => bail_traced!(std::io::Error::other(err), self),
            };
            if outcome.bytes_written > 0 {
                ((), self) =
                    buf_try_traced!(self.write_buf(..prefix_len + outcome.bytes_written).await);
                prefix_len = 0;
            }
        }
        if prefix_len > 0 {
            // An entry without any content still owes its header.
            ((), self) = buf_try_traced!(self.write_buf(..prefix_len).await);
        }
        BufResult(Ok(()), self)
    }

    #[tracing::instrument(skip_all, level = "info")]
    pub(super) async fn serve_index(
        mut self,
        is_root: bool,
        entries: &[FsTreeNode],
    ) -> BufResult<(), Self> {
        const INDEX_PREAMBLE: &str = include_str!("index.html");
        const INDEX_HEADER: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nTransfer-Encoding: chunked\r\n\r\n";
        debug_assert!(
            self.buf.len() > INDEX_PREAMBLE.len() * 2,
            "buffer should have ample space"
        );

        // The header rides along with the first chunk.
        self.buf[..INDEX_HEADER.len()].copy_from_slice(INDEX_HEADER);
        let mut chunk = PendingChunk {
            prefix: INDEX_HEADER.len(),
            len: 0,
        };

        let top = if is_root {
            ""
        } else {
            "<li class=top><a href=\"..\">..</a>\n"
        };
        (chunk, self) = buf_try!(
            self.append_fmt_chunked(chunk, format_args!("{INDEX_PREAMBLE}{top}"))
                .await
        );

        let dirs = entries.iter().filter_map(|entry| match entry {
            FsTreeNode::Dir { name, .. } => Some((name, " class=dir", "/")),
            FsTreeNode::File { .. } => None,
        });
        let files = entries.iter().filter_map(|entry| match entry {
            FsTreeNode::File { name, .. } => Some((name, "", "")),
            FsTreeNode::Dir { .. } => None,
        });
        for (name, class, slash) in dirs.chain(files) {
            (chunk, self) = buf_try!(
                self.append_fmt_chunked(
                    chunk,
                    format_args!(
                        "<li{class}><a href=\"./{name_url}{slash}\">{name_html}</a>\n",
                        name_url = utf8_percent_encode(name, URI_FRAGMENT_ENCODING_SET),
                        name_html = v_htmlescape::escape(name),
                    ),
                )
                .await
            );
        }

        ((), self) = buf_try_traced!(self.flush_chunk(chunk, true).await);
        BufResult(Ok(()), self)
    }

    #[tracing::instrument(skip_all, level = "debug")]
    async fn serve_entry(
        mut self,
        file: &File,
        entry: &Entry,
        accepted_encodings: AcceptedEncodings,
    ) -> BufResult<(), Self> {
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

        if compression.is_none() && !is_method_supported(entry.method) {
            tracing::error!("Unsupported compression method {:?}", entry.method);
            return self.send_status_empty("500 Unsupported Compression").await;
        }

        let res = write_entry_header(&mut self.buf, entry, compression.as_ref());
        let head_len;
        (head_len, self) = buf_try_traced!(res, self);
        if compression.is_some() {
            self.send_compressed_entry(file, entry, head_len).await
        } else {
            self.send_decompressed_entry(file, entry, head_len).await
        }
    }

    pub async fn serve_node(
        self,
        file: &File,
        node: &FsTreeNode,
        accepted_encodings: AcceptedEncodings,
    ) -> BufResult<(), Self> {
        match node {
            FsTreeNode::File { entry, .. } => {
                self.serve_entry(file, entry, accepted_encodings).await
            }
            FsTreeNode::Dir {
                children,
                index_html_index: Some(idx),
                ..
            } if let FsTreeNode::File { entry, .. } = &children[*idx] => {
                self.serve_entry(file, entry, accepted_encodings).await
            }
            FsTreeNode::Dir {
                is_root, children, ..
            } => self.serve_index(*is_root, children).await,
        }
    }

    pub async fn serve_not_modified(mut self, crc32: u32) -> BufResult<(), Self> {
        const NOT_MODIFIED_TEMPLATE: &[u8] =
            b"HTTP/1.1 304 Not Modified\r\nETag: \"xxxxxxxx\"\r\n\r\n";
        const CRC_OFFSET: usize =
            position(NOT_MODIFIED_TEMPLATE, b'x').expect("template sould have x");

        self.buf[0..NOT_MODIFIED_TEMPLATE.len()].copy_from_slice(NOT_MODIFIED_TEMPLATE);

        let etag = encode_crc32(crc32);
        self.buf[CRC_OFFSET..{ CRC_OFFSET + etag.len() }].copy_from_slice(&etag);

        self.write_buf(..NOT_MODIFIED_TEMPLATE.len()).await
    }

    async fn send_status_empty(mut self, status: &'static str) -> BufResult<(), Self> {
        let mut len = 0;
        let res = append_fmt(
            &mut self.buf,
            &mut len,
            format_args!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n"),
        );
        ((), self) = buf_try_traced!(res, self);
        self.write_buf(..len).await
    }

    pub async fn serve_status(self, status: HttpStatus) -> BufResult<(), Self> {
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

pub(super) const fn position(haystack: &[u8], needle: u8) -> Option<usize> {
    let mut idx = 0;
    while idx < haystack.len() {
        if haystack[idx] == needle {
            return Some(idx);
        }
        idx += 1;
    }
    None
}
