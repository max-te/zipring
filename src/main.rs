mod fstree;
mod rc_zip_monoio;
mod response;

use std::path::PathBuf;

use monoio::buf::{IoVecBufMut, VecBuf};
use monoio::fs::File;
use monoio::io::{AsyncReadRent, AsyncWriteRentExt, OwnedReadHalf, Splitable};
use monoio::net::{TcpListener, TcpStream};
use tracing::Instrument;

use crate::fstree::FsTreeNode;
use crate::response::serve_node;

#[monoio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let Some(file_arg) = std::env::args().nth(1) else {
        println!("Usage: zipring ZIPFILE");
        return;
    };
    let filepath = PathBuf::from(file_arg);
    let file = monoio::fs::File::open(&filepath).await.unwrap();
    let file: &'static File = Box::leak(Box::new(file));
    let zip = rc_zip_monoio::read_zip_from_file(&file).await.unwrap();

    let mut tree = FsTreeNode::root();
    for entry in zip.entries() {
        tree.insert(entry.clone());
    }
    tree.recursive_sort();
    let tree: &'static FsTreeNode = Box::leak(Box::new(tree));

    let listener = TcpListener::bind("127.0.0.1:50002").unwrap();
    println!(
        "Serving {} at http://{}",
        filepath.display(),
        listener.local_addr().unwrap()
    );
    let mut conid = 0usize;
    loop {
        let incoming = listener.accept().await;
        match incoming {
            Ok((stream, addr)) => {
                println!("accepted a connection from {}", addr);
                let span = tracing::info_span!("connection", id = conid);
                let _ = stream.set_nodelay(true);
                monoio::spawn(serve(stream, &file, &tree).instrument(span));
            }
            Err(e) => {
                println!("accepted connection failed: {}", e);
                return;
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
            let Some(path) = request else {
                break;
            };
            s.send(path).await.unwrap();
        }
    }
    .instrument(recv_span);

    let send_span = tracing::info_span!("sender");
    let sender = async {
        let mut buf = vec![0u8; 16 * 1024].into_boxed_slice();
        loop {
            let Ok(path) = r.recv().await else {
                break;
            };
            let respond_span = tracing::info_span!("response", path = path).entered();
            let Some(node) = tree.find(&path) else {
                tracing::warn!(?path, "entry not found");
                let (res, _) = stream_write
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .instrument(respond_span.exit())
                    .await;
                if let Err(_) = res {
                    break;
                }
                continue;
            };
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

async fn parse_next_request(
    stream: &mut OwnedReadHalf<TcpStream>,
    buf: Box<[u8]>,
) -> (Option<String>, Box<[u8]>) {
    let (res, buf) = stream.read(buf).await;
    let Ok(len) = res else { return (None, buf) };
    if len == 0 {
        return (None, buf);
    }

    if !buf.starts_with(b"GET ") {
        return (None, buf);
    }
    let path = &buf[b"GET /".len()..];
    let Some(br) = path.iter().position(|&c| c == b' ') else {
        return (None, buf);
    };
    let path = &path[..br];
    let Ok(path) = percent_encoding::percent_decode(path).decode_utf8() else {
        return (None, buf);
    };
    let path = path.to_string();
    tracing::debug!(?path, "GET request");

    (Some(path), buf)
}
