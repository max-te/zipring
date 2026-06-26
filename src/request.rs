use monoio::{
    buf::{IoBufMut, SliceMut},
    io::{AsyncReadRent, OwnedReadHalf},
    net::TcpStream,
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
        if_none_match: Option<u32>,
        accepted_encodings: AcceptedEncodings,
        close: bool,
    },
    Bad {
        status: HttpStatus,
        buf: Buf,
    },
}

pub async fn parse_next_request(
    stream: &mut OwnedReadHalf<TcpStream>,
    buf: Buf,
) -> Result<Request, Buf> {
    let (res, mut buf) = stream.read(buf).await;
    let Ok(len) = res else { return Err(buf) };
    if len == 0 {
        tracing::debug!("read 0 bytes");
        return Err(buf);
    }

    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let body_offset = match req.parse(&buf) {
        Ok(body_offset) => body_offset,
        Err(httparse::Error::TooManyHeaders) => {
            return Ok(Request::Bad {
                status: HttpStatus::HeaderTooLong,
                buf,
            });
        }
        _ => {
            tracing::debug!("could not parse request");
            return Err(buf);
        }
    };
    if body_offset.is_partial() {
        tracing::debug!("partial request");
        if req.path.is_none() {
            return Ok(Request::Bad {
                status: HttpStatus::UriTooLong,
                buf,
            });
        }
        return Ok(Request::Bad {
            status: HttpStatus::BadRequest,
            buf,
        });
    }

    if req.method != Some("GET") {
        tracing::debug!("unsupported method");
        return Ok(Request::Bad {
            status: HttpStatus::MethodNotAllowed,
            buf,
        });
    }

    let path = req
        .path
        .expect("path should be set when parsing is complete");
    let path_offset = unsafe {
        // SAFETY: path is a slice of buf
        path.as_ptr().byte_offset_from_unsigned(buf.as_ptr())
    };
    let path_end = path_offset + path.len();

    let mut if_none_match = None;
    let mut accepted_encodings = AcceptedEncodings::default();
    let mut close = false;
    for h in headers {
        if h.name.eq_ignore_ascii_case("if-none-match") && h.value.len() == 10 {
            let hex_part = &h.value[1..9];
            if let Ok(crc32_bytes) = const_hex::decode_to_array::<&[u8], 4>(hex_part) {
                let crc32 = u32::from_be_bytes(crc32_bytes);
                if_none_match = Some(crc32);
                break;
            }
        }
        if h.name.eq_ignore_ascii_case("accept-encoding") {
            accepted_encodings = AcceptedEncodings::from_header(&h);
        }
        if h.name.eq_ignore_ascii_case("connection") && h.value.eq_ignore_ascii_case(b"close") {
            close = true;
        }
    }

    let (pre, post) = buf.split_at_mut(path_end);

    let path = &pre[path_offset..path_end];
    tracing::debug!(?path, "GET request");

    let mut decoded_path_cur = 0;
    for (i, chunk) in percent_encoding::percent_decode(path).enumerate() {
        if i == post.len() {
            break;
        }
        post[i] = chunk;
        decoded_path_cur = i + 1;
    }
    if decoded_path_cur == post.len() {
        tracing::error!("path to long for buffer");
        return Ok(Request::Bad {
            status: HttpStatus::UriTooLong,
            buf,
        });
    }
    let path = IoBufMut::slice_mut(buf, path_end..(path_end + decoded_path_cur));

    Ok(Request::Get {
        path,
        if_none_match,
        accepted_encodings,
        close,
    })
}
