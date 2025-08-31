mod fstree;
mod rc_zip_monoio;
mod request;
mod response;

use std::num::NonZero;
use std::path::PathBuf;

use monoio::IoUringDriver;
use monoio::fs::File;
use monoio::io::Splitable;
use monoio::net::{TcpListener, TcpStream};
use tracing::Instrument;

use crate::fstree::FsTreeNode;
use crate::request::run_receiver;
use crate::response::run_sender;

type Buf = Box<[u8]>;

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
    let (requests_channel_in, requests_channel_out) = async_channel::bounded(5);
    let (stream_read, stream_write) = stream.into_split();

    let recv_span = tracing::info_span!("receiver");
    let receiver = run_receiver(requests_channel_in, stream_read).instrument(recv_span);

    let send_span = tracing::info_span!("sender");
    let sender = run_sender(file, tree, requests_channel_out, stream_write).instrument(send_span);

    monoio::try_join!(receiver, sender)?;
    Ok(())
}
