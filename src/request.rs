use monoio::{
    io::{AsyncReadRent as _, OwnedReadHalf},
    net::TcpStream,
};

use crate::Buf;

pub struct GetRequest {
    pub path: String,
    pub if_none_match: Option<u32>,
}

pub async fn run_receiver(
    requests_channel: async_channel::Sender<GetRequest>,
    mut stream_read: OwnedReadHalf<TcpStream>,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 1024].into_boxed_slice();
    loop {
        let request;
        (request, buf) = parse_next_request(&mut stream_read, buf).await;
        if let Some(request) = request {
            requests_channel.send(request).await.unwrap();
        };
    }
}

async fn parse_next_request(
    stream: &mut OwnedReadHalf<TcpStream>,
    buf: Buf,
) -> (Option<GetRequest>, Buf) {
    let (res, buf) = stream.read(buf).await;
    let Ok(len) = res else { return (None, buf) };
    if len == 0 {
        return (None, buf);
    }

    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let _body_offset = req.parse(&buf);

    if req.method != Some("GET") {
        return (None, buf);
    }

    let Some(path) = req.path else {
        return (None, buf);
    };
    let Ok(path) = percent_encoding::percent_decode(path.as_bytes()).decode_utf8() else {
        return (None, buf);
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
        Some(GetRequest {
            path,
            if_none_match,
        }),
        buf,
    )
}
