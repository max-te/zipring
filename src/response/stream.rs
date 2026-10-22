use std::fmt;
use std::io::{Cursor, Write};
use std::ops::RangeBounds;

use compio::BufResult;
use compio::buf::{IntoInner, IoBuf, buf_try};
use compio::fs::File;
use compio::io::AsyncWriteExt;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use rc_zip::Entry;

use crate::{
    Buf,
    buf_result::{bail_traced, buf_try_traced},
    fstree::{FsTreeNode, Target},
    rc_zip_compio::is_method_supported,
    request::{AcceptedEncodings, Headers},
    response::{
        body::{EntryBody, InflaterPool},
        coding::NativeCoding,
        status::HttpStatus,
    },
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

/// Lays the response header out at the front of `buf`, returning its length.
/// Without a `native` coding, the entry is announced decoded.
///
/// Nothing is sent here: the body is staged behind the header so that the whole
/// response can leave in a single write.
pub(super) fn write_entry_header(
    buf: &mut [u8],
    entry: &Entry,
    native: Option<NativeCoding>,
) -> std::io::Result<usize> {
    let mime_type = mime_guess::from_path(&entry.name).first_or_octet_stream();
    tracing::debug!(?mime_type);
    let mut cur = Cursor::new(buf);
    Write::write_all(&mut cur, b"HTTP/1.1 200 OK\r\n")?;

    Write::write_all(&mut cur, b"Content-Type: ")?;
    Write::write_all(&mut cur, mime_type.essence_str().as_bytes())?;
    Write::write_all(&mut cur, b"\r\n")?;

    if let Some(name) = native.and_then(NativeCoding::name) {
        Write::write_all(&mut cur, b"Content-Encoding: ")?;
        Write::write_all(&mut cur, name.as_bytes())?;
        Write::write_all(&mut cur, b"\r\n")?;
    }

    Write::write_all(&mut cur, b"Content-Length: ")?;
    let content_length = match native {
        Some(native) => native.content_length(entry),
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

/// Keeps an entry name one path segment for browsers -- WHATWG's path set, plus
/// `\`, which they read as `/` -- safe in a quoted HTML attribute (`&`), and
/// intact through the server's own percent-decoding (`%`).
const PATH_SEGMENT_ENCODING_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'\\')
    .add(b'`')
    .add(b'{')
    .add(b'}');

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
                        name_url = utf8_percent_encode(name, PATH_SEGMENT_ENCODING_SET),
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
        inflaters: &InflaterPool,
    ) -> BufResult<(), Self> {
        let native = NativeCoding::of(entry.method)
            .filter(|native| native.is_accepted_by(accepted_encodings));

        if native.is_none() && !is_method_supported(entry.method) {
            tracing::error!("Unsupported compression method {:?}", entry.method);
            return self.serve_status(HttpStatus::UnsupportedCompression).await;
        }

        let res = write_entry_header(&mut self.buf, entry, native);
        let head_len;
        (head_len, self) = buf_try_traced!(res, self);
        self.send_body(file, EntryBody::new(entry, native, inflaters), head_len)
            .await
    }

    /// Sends `body` behind the `staged` bytes already at the front of the buffer,
    /// which leave with its first portion -- or alone, if it has none.
    #[tracing::instrument(skip_all)]
    async fn send_body(
        mut self,
        file: &File,
        mut body: EntryBody<'_>,
        mut staged: usize,
    ) -> BufResult<(), Self> {
        loop {
            let BufResult(res, buf) = body.fill(file, self.buf, staged).await;
            self.buf = buf;
            let n;
            (n, self) = buf_try_traced!(res, self);
            if n == 0 && staged == 0 {
                break;
            }
            ((), self) = buf_try_traced!(self.write_buf(..staged + n).await);
            if n == 0 {
                break;
            }
            staged = 0;
        }
        BufResult(Ok(()), self)
    }

    pub async fn serve_target(
        self,
        file: &File,
        target: Target<'_>,
        headers: Headers,
        inflaters: &InflaterPool,
    ) -> BufResult<(), Self> {
        match target {
            Target::Entry(entry) if headers.if_none_match == Some(entry.crc32) => {
                tracing::debug!("etag matches");
                self.serve_not_modified(entry.crc32).await
            }
            Target::Entry(entry) => {
                self.serve_entry(file, entry, headers.accepted_encodings, inflaters)
                    .await
            }
            Target::Listing { is_root, children } => self.serve_index(is_root, children).await,
            Target::Redirect { name } => self.serve_redirect_into_dir(name).await,
        }
    }

    /// Answers with a `Location` relative to the request, so that only the last
    /// segment needs spelling out.
    async fn serve_redirect_into_dir(mut self, name: &str) -> BufResult<(), Self> {
        let mut len = 0;
        let res = append_fmt(
            &mut self.buf,
            &mut len,
            format_args!(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: ./{name_url}/\r\nContent-Length: 0\r\n\r\n",
                name_url = utf8_percent_encode(name, PATH_SEGMENT_ENCODING_SET),
            ),
        );
        ((), self) = buf_try_traced!(res, self);
        self.write_buf(..len).await
    }

    pub(super) async fn serve_not_modified(mut self, crc32: u32) -> BufResult<(), Self> {
        const NOT_MODIFIED_TEMPLATE: &[u8] =
            b"HTTP/1.1 304 Not Modified\r\nETag: \"xxxxxxxx\"\r\n\r\n";
        const CRC_OFFSET: usize =
            position(NOT_MODIFIED_TEMPLATE, b'x').expect("template sould have x");

        self.buf[0..NOT_MODIFIED_TEMPLATE.len()].copy_from_slice(NOT_MODIFIED_TEMPLATE);

        let etag = encode_crc32(crc32);
        self.buf[CRC_OFFSET..{ CRC_OFFSET + etag.len() }].copy_from_slice(&etag);

        self.write_buf(..NOT_MODIFIED_TEMPLATE.len()).await
    }

    pub async fn serve_status(mut self, status: HttpStatus) -> BufResult<(), Self> {
        let mut len = 0;
        let res = append_fmt(
            &mut self.buf,
            &mut len,
            format_args!(
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n",
                status = status.as_str()
            ),
        );
        ((), self) = buf_try_traced!(res, self);
        self.write_buf(..len).await
    }
}

fn encode_crc32(crc32: u32) -> [u8; const { size_of::<u32>() * 2 }] {
    let mut etag = [0; _];
    const_hex::encode_to_slice(crc32.to_be_bytes(), &mut etag)
        .expect("u32 should always be encodable to 8 hex chars");
    etag
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
