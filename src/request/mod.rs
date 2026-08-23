use std::fmt::Debug;

use monoio::{
    buf::{IoBufMut, SliceMut},
    io::AsyncReadRent,
};

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
    Get {
        path: SliceMut<Buf>,
        headers: Headers,
    },
    Bad {
        status: HttpStatus,
        buf: Buf,
    },
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
                    &str::from_utf8(&path).expect("path should be valid utf-8"),
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

pub async fn parse_next_request<R: AsyncReadRent>(
    stream: &mut R,
    buf: Buf,
) -> Result<Request, Buf> {
    let (len, buf) = read_request(stream, buf).await?;
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let httparse::Request {
        method,
        path,
        headers,
        ..
    } = match try_parse_http(&buf[..len], &mut headers) {
        Ok(value) => value,
        Err(Some(status)) => return Ok(bad(status, buf)),
        Err(None) => return Err(buf),
    };

    let Some(path) = path else {
        tracing::error!("no path");
        return Ok(bad(HttpStatus::BadRequest, buf));
    };

    if method != Some("GET") {
        tracing::error!("unsupported method");
        return Ok(bad(HttpStatus::MethodNotAllowed, buf));
    }
    tracing::info!(?path, "GET request");

    let headers = extract_headers(headers);
    // SAFETY: `path` is a slice of `buf`
    let path_range = unsafe { get_path_range(&buf, path) };
    let path = match decode_path(buf, path_range) {
        Ok(path) => path,
        Err(buf) => {
            tracing::error!("decode path failed");
            return Ok(bad(HttpStatus::UriTooLong, buf));
        }
    };

    Ok(Request::Get { path, headers })
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

/// Read until the request's headers are complete, returning how much of `buf` they fill.
///
/// A client is entitled to split its request across as many segments as it likes, so a
/// short read is not a malformed request -- only a closed connection is. A request that
/// fills the buffer without terminating is handed on regardless, for the parser to
/// reject with the status that fits.
async fn read_request(stream: &mut impl AsyncReadRent, mut buf: Buf) -> Result<(usize, Buf), Buf> {
    let capacity = buf.len();
    let mut filled = 0;
    loop {
        let mut slice = IoBufMut::slice_mut(buf, filled..capacity);
        let res;
        (res, slice) = stream.read(slice).await;
        buf = slice.into_inner();

        let Ok(n) = res else { return Err(buf) };
        if n == 0 {
            tracing::debug!("read 0 bytes");
            return Err(buf);
        }

        // Start far enough back that a terminator straddling two reads is still found.
        let search_from = filled.saturating_sub(HEADERS_END.len() - 1);
        filled += n;
        if buf[search_from..filled]
            .windows(HEADERS_END.len())
            .any(|window| window == HEADERS_END)
        {
            return Ok((filled, buf));
        }
        if filled == capacity {
            tracing::error!("request fills the buffer without ending");
            return Ok((filled, buf));
        }
    }
}

// SAFETY: `path` must be a slice within `buf`
unsafe fn get_path_range(buf: &Buf, path: &str) -> std::range::Range<usize> {
    let start = unsafe {
        // SAFETY: caller must ensure `path` is a slice within `buf`
        path.as_ptr().byte_offset_from_unsigned(buf.as_ptr())
    };
    let end = start + path.len();
    std::range::Range { start, end }
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
            headers.accepted_encodings = AcceptedEncodings::from_header(&h);
        } else if h.name.eq_ignore_ascii_case("connection")
            && h.value.eq_ignore_ascii_case(b"close")
        {
            headers.close = true;
        }
    }
    headers
}

fn decode_path(mut buf: Buf, path_range: std::range::Range<usize>) -> Result<SliceMut<Buf>, Buf> {
    let (pre, post) = buf.split_at_mut(path_range.end);
    let path = &pre[path_range];
    let mut decoded_path_cur = 0;
    for (i, chunk) in percent_encoding::percent_decode(path).enumerate() {
        if i == post.len() {
            // We'd return an error here, but we can't because the buffer is still borrowed
            break;
        }
        post[i] = chunk;
        decoded_path_cur = i + 1;
    }
    if decoded_path_cur >= post.len() {
        tracing::error!("path to long for buffer");
        return Err(buf);
    }
    let path = IoBufMut::slice_mut(buf, path_range.end..(path_range.end + decoded_path_cur));

    Ok(path)
}

#[cfg(test)]
mod test;
