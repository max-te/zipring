use monoio::{
    io::{AsyncReadRent, OwnedReadHalf},
    net::TcpStream,
};

use crate::Buf;

pub enum Request {
    Get {
        path: String,
        if_none_match: Option<u32>,
    },
    Bad {
        status: &'static str,
    },
}

const BAD_REQUEST: &str = "400 Bad Request";
const URI_TOO_LONG: &str = "414 URI Too Long";
const HEADER_TOO_LONG: &str = "431 Request Header Fields Too Large";
const METHOD_NOT_ALLOWED: &str = "405 Method Not Allowed";

pub async fn parse_next_request(
    stream: &mut OwnedReadHalf<TcpStream>,
    buf: Buf,
) -> (Option<Request>, Buf) {
    let (res, buf) = stream.read(buf).await;
    let Ok(len) = res else { return (None, buf) };
    if len == 0 {
        tracing::debug!("read 0 bytes");
        return (None, buf);
    }

    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let body_offset = match req.parse(&buf) {
        Ok(body_offset) => body_offset,
        Err(httparse::Error::TooManyHeaders) => {
            return (
                Some(Request::Bad {
                    status: HEADER_TOO_LONG,
                }),
                buf,
            );
        }
        _ => {
            tracing::debug!("could not parse request");
            return (None, buf);
        }
    };
    if body_offset.is_partial() {
        tracing::debug!("partial request");
        if req.path.is_none() {
            return (
                Some(Request::Bad {
                    status: URI_TOO_LONG,
                }),
                buf,
            );
        }
        return (
            Some(Request::Bad {
                status: BAD_REQUEST,
            }),
            buf,
        );
    }

    if req.method != Some("GET") {
        tracing::debug!("unsupported method");
        return (
            Some(Request::Bad {
                status: METHOD_NOT_ALLOWED,
            }),
            buf,
        );
    }

    let path = req
        .path
        .expect("path should be set when parsing is complete");
    let Ok(path) = percent_encoding::percent_decode(path.as_bytes()).decode_utf8() else {
        tracing::error!("path not decodable");
        return (
            Some(Request::Bad {
                status: BAD_REQUEST,
            }),
            buf,
        );
    };
    let path = path.to_string();

    let mut if_none_match = None;
    for h in headers {
        if h.name == "If-None-Match" && h.value.len() == 10 {
            let hex_part = &h.value[1..9];
            if let Ok(crc32_bytes) = const_hex::decode_to_array::<&[u8], 4>(hex_part) {
                let crc32 = u32::from_le_bytes(crc32_bytes);
                if_none_match = Some(crc32);
                break;
            }
        }
    }

    tracing::debug!(?path, "GET request");

    (
        Some(Request::Get {
            path,
            if_none_match,
        }),
        buf,
    )
}
