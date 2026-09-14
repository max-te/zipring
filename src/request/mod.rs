use std::fmt::Debug;

use compio::BufResult;
use compio::buf::{IntoInner, IoBuf, Slice};
use compio::io::AsyncReadExt;

use crate::{Buf, response::status::HttpStatus};

#[derive(Debug, Clone, Copy, Default)]
pub struct AcceptedEncodings {
    pub gzip: bool,
    pub zstd: bool,
}

impl AcceptedEncodings {
    pub fn from_header(header: &httparse::Header) -> Self {
        let value = std::str::from_utf8(header.value).unwrap_or_default();
        let gzip = value.contains("gzip");
        let zstd = value.contains("zstd");

        Self { gzip, zstd }
    }
}

pub enum Request {
    Get { path: Slice<Buf>, headers: Headers },
    Bad { status: HttpStatus, buf: Buf },
}

impl Request {
    pub fn keep_alive(&self) -> bool {
        matches!(
            self,
            Request::Get {
                headers: Headers { close: false, .. },
                ..
            }
        )
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Headers {
    pub if_none_match: Option<u32>,
    pub accepted_encodings: AcceptedEncodings,
    pub close: bool,
}

impl Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Request::Get { path, headers } => f
                .debug_struct("Get")
                .field(
                    "path",
                    &str::from_utf8(path).expect("path should be valid utf-8"),
                )
                .field("if_none_match", &headers.if_none_match)
                .field("accepted_encodings", &headers.accepted_encodings)
                .field("close", &headers.close)
                .finish(),
            Request::Bad { status, buf } => f
                .debug_struct("Bad")
                .field("status", status)
                .field("buf", buf)
                .finish(),
        }
    }
}

/// Holds a connection's request bytes, which arrive on their own schedule: one request
/// may take several reads, and one read may deliver several requests.
///
/// The bytes between `consumed` and `filled` are what has arrived and not yet been
/// answered. They live here rather than in the response buffer so that a pipelined
/// request survives the response to the one before it.
pub struct RequestReader {
    buf: Buf,
    filled: usize,
    consumed: usize,
}

/// Ample for request headers, and the conventional bound: a request that does not fit
/// is refused rather than grown into.
pub const REQUEST_BUF_SIZE: usize = 8 * 1024;

enum Fill {
    Read,
    /// The peer closed, or the connection failed.
    Closed,
    /// The buffer holds an unterminated request and has no room left.
    Full,
}

impl RequestReader {
    pub fn new() -> Self {
        Self::with_capacity(REQUEST_BUF_SIZE)
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: vec![0u8; capacity].into_boxed_slice(),
            filled: 0,
            consumed: 0,
        }
    }

    /// Parse the next request, reading from `stream` only when the bytes already in
    /// hand do not hold a whole one.
    ///
    /// The decoded path is copied into `response_buf`, which the returned request
    /// carries onwards -- so nothing the caller holds refers to this reader's bytes,
    /// and the next request may sit here untouched while this one is answered.
    pub async fn next_request<R: AsyncReadExt>(
        &mut self,
        stream: &mut R,
        response_buf: Buf,
    ) -> Result<Request, Buf> {
        let mut scanned = self.consumed;
        let request_end = loop {
            if let Some(end) = find_headers_end(&self.buf[scanned..self.filled]) {
                break scanned + end;
            }
            // Resume just far enough back to catch a terminator split across two reads.
            scanned = self
                .filled
                .saturating_sub(HEADERS_END.len() - 1)
                .max(self.consumed);

            let consumed_before = self.consumed;
            match self.fill(stream).await {
                Fill::Read => {}
                Fill::Closed => return Err(response_buf),
                Fill::Full => {
                    tracing::error!("request fills the buffer without ending");
                    break self.filled;
                }
            }
            // A compaction inside fill() shifts what is left toward the front.
            scanned -= consumed_before - self.consumed;
        };

        let request = self.parse(request_end, response_buf);
        if self.consumed == self.filled {
            // Nothing pipelined behind it: start the next request at the front.
            self.consumed = 0;
            self.filled = 0;
        }
        request
    }

    /// Parse one request out of `self.buf[self.consumed..end]`, advancing past it.
    fn parse(&mut self, end: usize, response_buf: Buf) -> Result<Request, Buf> {
        let raw = &self.buf[self.consumed..end];
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let parsed = try_parse_http(raw, &mut headers);
        self.consumed = end;

        let httparse::Request {
            method,
            path,
            headers,
            ..
        } = match parsed {
            Ok(value) => value,
            Err(Some(status)) => return Ok(bad(status, response_buf)),
            Err(None) => return Err(response_buf),
        };

        let Some(path) = path else {
            tracing::error!("no path");
            return Ok(bad(HttpStatus::BadRequest, response_buf));
        };

        if method != Some("GET") {
            tracing::error!("unsupported method");
            return Ok(bad(HttpStatus::MethodNotAllowed, response_buf));
        }
        tracing::info!(?path, "GET request");

        let headers = extract_headers(headers);
        let path = match decode_path(path.as_bytes(), response_buf) {
            Ok(path) => path,
            Err(response_buf) => {
                tracing::error!("decode path failed");
                return Ok(bad(HttpStatus::UriTooLong, response_buf));
            }
        };

        Ok(Request::Get { path, headers })
    }

    /// Read once, making room first if the buffer has none left.
    async fn fill<R: AsyncReadExt>(&mut self, stream: &mut R) -> Fill {
        let capacity = self.buf.len();
        if self.filled == capacity {
            if self.consumed == 0 {
                return Fill::Full;
            }
            self.buf.copy_within(self.consumed..self.filled, 0);
            self.filled -= self.consumed;
            self.consumed = 0;
        }

        let buf = std::mem::take(&mut self.buf);
        let BufResult(res, slice) = stream.read(buf.slice(self.filled..capacity)).await;
        self.buf = slice.into_inner();

        match res {
            Ok(0) => {
                tracing::debug!("read 0 bytes");
                Fill::Closed
            }
            Ok(n) => {
                self.filled += n;
                Fill::Read
            }
            Err(_) => Fill::Closed,
        }
    }
}

impl Default for RequestReader {
    fn default() -> Self {
        Self::new()
    }
}

fn try_parse_http<'h, 'b>(
    buf: &'b [u8],
    headers: &'h mut [httparse::Header<'b>; 64],
) -> Result<httparse::Request<'h, 'b>, Option<HttpStatus>> {
    let mut req = httparse::Request::new(headers);
    let body_offset = match req.parse(buf) {
        Ok(body_offset) => body_offset,
        Err(httparse::Error::TooManyHeaders) => {
            return Err(Some(HttpStatus::HeaderTooLong));
        }
        _ => {
            tracing::error!("could not parse request");
            return Err(Some(HttpStatus::BadRequest));
        }
    };
    if body_offset.is_partial() {
        // read_request only gives up once the buffer is full, so this is a request
        // too large to hold rather than one that has yet to arrive.
        tracing::error!("partial request");
        if req.path.is_none() {
            return Err(Some(HttpStatus::UriTooLong));
        }
        return Err(Some(HttpStatus::BadRequest));
    }
    Ok(req)
}

fn bad(status: HttpStatus, buf: Buf) -> Request {
    Request::Bad { status, buf }
}

const HEADERS_END: &[u8] = b"\r\n\r\n";

/// Where the headers end, as an index just past the terminator.
fn find_headers_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(HEADERS_END.len())
        .position(|window| window == HEADERS_END)
        .map(|start| start + HEADERS_END.len())
}

fn extract_headers(parsed_headers: &mut [httparse::Header<'_>]) -> Headers {
    let mut headers = Headers::default();
    for h in parsed_headers {
        if h.name.eq_ignore_ascii_case("if-none-match") && h.value.len() == 10 {
            let hex_part = &h.value[1..9];
            if let Ok(crc32_bytes) = const_hex::decode_to_array::<&[u8], 4>(hex_part) {
                let crc32 = u32::from_be_bytes(crc32_bytes);
                headers.if_none_match = Some(crc32);
            }
        } else if h.name.eq_ignore_ascii_case("accept-encoding") {
            headers.accepted_encodings = AcceptedEncodings::from_header(h);
        } else if h.name.eq_ignore_ascii_case("connection")
            && h.value.eq_ignore_ascii_case(b"close")
        {
            headers.close = true;
        }
    }
    headers
}

/// Percent-decode the raw path into the front of `buf`, which the response is built
/// over once the path has served its purpose.
fn decode_path(raw: &[u8], mut buf: Buf) -> Result<Slice<Buf>, Buf> {
    let mut len = 0;
    for byte in percent_encoding::percent_decode(raw) {
        if len == buf.len() {
            tracing::error!("path too long for buffer");
            return Err(buf);
        }
        buf[len] = byte;
        len += 1;
    }
    Ok(buf.slice(..len))
}

#[cfg(test)]
mod test;
