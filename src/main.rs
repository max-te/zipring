mod fstree;
mod rc_zip_monoio;
mod response;

use std::num::NonZero;
use std::path::PathBuf;

use monoio::IoUringDriver;
use monoio::buf::IoBufMut;
use monoio::fs::File;
use monoio::io::{AsyncReadRent, AsyncWriteRentExt, OwnedReadHalf, Splitable};
use monoio::net::{TcpListener, TcpStream};
use tracing::Instrument;

use crate::fstree::FsTreeNode;
use crate::response::serve_node;

fn main() {
    tracing_subscriber::fmt::init();
    let Some(file_arg) = std::env::args().nth(1) else {
        println!("Usage: zipring ZIPFILE");
        return;
    };

    let filepath = PathBuf::from(file_arg);
    let n_threads = std::thread::available_parallelism()
        .map(NonZero::get)
        .unwrap_or(4);

    let threads: Vec<_> = (0..n_threads)
        .map(|i| {
            let path = filepath.clone();
            std::thread::spawn(move || {
                let mut rt = monoio::RuntimeBuilder::<IoUringDriver>::new()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(inner_main(i, path))
            })
        })
        .collect();

    for t in threads {
        let _ = t.join();
    }
}

async fn inner_main(threadid: usize, filepath: PathBuf) {
    let file = monoio::fs::File::open(&filepath).await.unwrap();
    let file: &'static File = Box::leak(Box::new(file));
    let zip = rc_zip_monoio::read_zip_from_file(file).await.unwrap();

    let mut tree = FsTreeNode::root();
    for entry in zip.entries() {
        tree.insert(entry.clone());
    }
    tree.recursive_sort();
    let tree: &'static FsTreeNode = Box::leak(Box::new(tree));

    let listener = TcpListener::bind("127.0.0.1:50002").unwrap();
    if threadid == 0 {
        tracing::info!(
            "Serving {} at http://{}",
            filepath.display(),
            listener.local_addr().unwrap()
        );
    }
    let mut conid = 0usize;
    loop {
        let incoming = listener.accept().await;
        match incoming {
            Ok((stream, addr)) => {
                let span =
                    tracing::info_span!("connection", thread = threadid, conid = conid).entered();
                tracing::info!("accepted a connection from {}", addr);
                let _ = stream.set_nodelay(true);
                monoio::spawn(serve(stream, file, tree).instrument(span.exit()));
            }
            Err(e) => {
                tracing::error!(?threadid, "accepting connection failed: {}", e);
            }
        }
        conid += 1;
    }
}

async fn serve(stream: TcpStream, file: &File, tree: &FsTreeNode) -> std::io::Result<()> {
    let (s, r) = async_channel::bounded(5);
    let (mut stream_read, mut stream_write) = stream.into_split();

    let recv_span = tracing::info_span!("receiver");
    let receiver = async move {
        let mut buf = vec![0u8; 1024].into_boxed_slice();
        loop {
            let request;
            (request, buf) = parse_next_request(&mut stream_read, buf).await;
            if let Some(request) = request {
                s.send(request).await.unwrap();
            };
        }
    }
    .instrument(recv_span);

    let send_span = tracing::info_span!("sender");
    let sender = async {
        let mut buf = vec![0u8; 16 * 1024].into_boxed_slice();
        loop {
            let Ok(request) = r.recv().await else {
                break;
            };
            let respond_span = tracing::info_span!("response", path = request.path).entered();
            let Some(node) = tree.find(&request.path) else {
                tracing::warn!(?request.path, "entry not found");
                let (res, _) = stream_write
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .instrument(respond_span.exit())
                    .await;
                if res.is_err() {
                    break;
                }
                continue;
            };
            if let Some(crc32) = request.if_none_match
                && let Some(entry) = node.entry()
                && entry.crc32 == crc32
            {
                tracing::debug!(?request.path, "etag matches");
                const NOT_MODIFIED_TEMPLATE: &[u8] =
                    b"HTTP/1.1 304 Not Modified\r\nETag: \"xxxxxxxx\"\r\n\r\n";
                let mut response = vec![0u8; const { NOT_MODIFIED_TEMPLATE.len() }];
                response.copy_from_slice(NOT_MODIFIED_TEMPLATE);

                let etag = &mut response[34..{ 34 + 8 }];
                const_hex::encode_to_slice(entry.crc32.to_le_bytes(), etag).unwrap();
                let len = response.len();
                let slice = IoBufMut::slice_mut(response, 0..len);
                let (res, _) = stream_write
                    .write_all(slice)
                    .instrument(respond_span.exit())
                    .await;
                if res.is_err() {
                    break;
                }
                continue;
            }
            buf = match serve_node(&mut stream_write, file, buf, node)
                .instrument(respond_span.exit())
                .await
            {
                Ok(b) => b,
                Err(_) => break,
            };
        }
    }
    .instrument(send_span);

    monoio::join!(receiver, sender);
    Ok(())
}

struct GetRequest {
    path: String,
    if_none_match: Option<u32>,
}

async fn parse_next_request(
    stream: &mut OwnedReadHalf<TcpStream>,
    buf: Box<[u8]>,
) -> (Option<GetRequest>, Box<[u8]>) {
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
