mod fstree;
mod rc_zip_monoio;
mod response;

use std::path::PathBuf;

use monoio::fs::File;
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};

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
    loop {
        let incoming = listener.accept().await;
        match incoming {
            Ok((stream, addr)) => {
                println!("accepted a connection from {}", addr);
                monoio::spawn(serve(stream, &file, &tree));
            }
            Err(e) => {
                println!("accepted connection failed: {}", e);
                return;
            }
        }
    }
}

async fn serve(mut stream: TcpStream, file: &File, tree: &FsTreeNode) -> std::io::Result<()> {
    let mut buf = vec![0u8; 8 * 1024].into_boxed_slice();
    let mut res;
    loop {
        (res, buf) = stream.read(buf).await;
        if res? == 0 {
            return Ok(());
        }
        if !buf.starts_with(b"GET ") {
            return Ok(());
        }
        let path = &buf[b"GET /".len()..];
        let Some(br) = path.iter().position(|&c| c == b' ') else {
            return Ok(());
        };
        let path = &path[..br];
        let Ok(path) = percent_encoding::percent_decode(path).decode_utf8() else {
            return Ok(());
        };
        tracing::debug!(?path, "GET request");

        let Some(node) = tree.find(&path) else {
            tracing::warn!(?path, "entry not found");
            let (res, _) = stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await;
            res?;
            continue;
        };
        buf = serve_node(&mut stream, file, buf, node).await?;
    }
}
