mod borrowed_file;
mod fstree;
mod rc_zip_monoio;
mod request;
pub(crate) mod response;

use std::num::NonZero;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use miette::{IntoDiagnostic, Result, WrapErr};
use monoio::IoUringDriver;
use monoio::fs::File;
use monoio::io::{AsyncWriteRent, Splitable};
use monoio::net::{TcpListener, TcpStream};
use tracing::Instrument;

use crate::borrowed_file::{BorrowedFile, FdBorrowToken, FdOwner};
use crate::fstree::FsTreeNode;
use crate::request::parse_next_request;
use crate::response::respond;

type Buf = Box<[u8]>;

#[derive(Debug)]
enum Never {}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let Some(file_arg) = std::env::args().nth(1) else {
        println!("Usage: zipring ZIPFILE");
        return Ok(());
    };

    let filepath = PathBuf::from(file_arg);
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "50002".to_string())
        .parse::<u16>()
        .unwrap_or(50002);

    let n_threads = std::env::var("ZIPRING_THREADS")
        .ok()
        .and_then(|v| v.parse::<NonZero<usize>>().ok())
        .map(NonZero::get)
        .unwrap_or_else(|| {
            // Benchmarks show 8 threads is the sweet spot for this I/O-bound server.
            // Beyond 8, high-concurrency latency improves <8% per added thread.
            // Each thread creates an io_uring runtime, so keeping the count
            // reasonable also avoids memlock exhaustion on constrained hosts.
            std::thread::available_parallelism()
                .map(NonZero::get)
                .unwrap_or(1)
                .min(8)
        });

    let (file, tree) = read_zip_tree(&filepath)?;
    let tree: &'static FsTreeNode = Box::leak(Box::new(tree));
    let file_holder = Box::leak(Box::new(FdOwner::from(file)));
    let mut threads: Vec<_> = (0..n_threads)
        .map(|i| {
            let token = file_holder.token();
            std::thread::spawn(move || -> Result<Never> {
                let mut rt = monoio::RuntimeBuilder::<IoUringDriver>::new()
                    .enable_all()
                    .build()
                    .into_diagnostic()
                    .wrap_err("should be able to start runtime")?;
                rt.block_on(inner_main(i, token, port, tree))
            })
        })
        .collect();

    loop {
        if let Some(finished) = threads.extract_if(.., |t| t.is_finished()).next() {
            match finished.join() {
                Ok(Err(err)) => return Err(err),
                Err(panic) => {
                    let msg = panic
                        .downcast_ref::<String>()
                        .map(|s| s.as_str())
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown panic cause");
                    return Err(miette::miette!("Thread panicked: {msg}"));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_zip_tree(filepath: &PathBuf) -> Result<(File, FsTreeNode), miette::Error> {
    monoio::RuntimeBuilder::<IoUringDriver>::new()
        .enable_all()
        .build()
        .into_diagnostic()
        .wrap_err("should be able to start runtime")?
        .block_on(async {
            let file = monoio::fs::File::open(filepath)
                .await
                .into_diagnostic()
                .wrap_err_with(|| format!("could not open {}", filepath.display()))?;
            let zip = rc_zip_monoio::read_zip_from_file(&file)
                .await
                .into_diagnostic()
                .wrap_err("could not parse zip")?;

            let mut tree = FsTreeNode::root();
            for entry in zip.entries() {
                tree.insert(entry.clone());
            }
            tree.recursive_sort();
            Ok((file, tree))
        })
}

async fn inner_main(
    threadid: usize,
    file_token: FdBorrowToken<'static>,
    port: u16,
    tree: &'static FsTreeNode,
) -> Result<Never> {
    let file: Rc<BorrowedFile<'static>> = Rc::new(file_token.to_borrowed_file());
    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr)
        .into_diagnostic()
        .wrap_err_with(|| format!("could not bind to {addr}"))?;
    if threadid == 0 {
        tracing::info!(
            "Serving file at http://{}",
            listener.local_addr().expect("should have an adress")
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
                monoio::spawn(serve(stream, file.clone(), &tree).instrument(span.exit()));
            }
            Err(e) => {
                tracing::error!(?threadid, "accepting connection failed: {}", e);
            }
        }
        conid += 1;
    }
}

async fn serve(stream: TcpStream, file: Rc<BorrowedFile<'_>>, tree: &FsTreeNode) {
    let (mut stream_read, mut stream_write) = stream.into_split();

    let mut buf = vec![0u8; 1024].into_boxed_slice();
    loop {
        let Ok(request) = parse_next_request(&mut stream_read, buf).await else {
            break;
        };
        let keep_alive = request.keep_alive();
        let Ok(r_buf) = respond(request, &*file, tree, &mut stream_write).await else {
            break;
        };
        buf = r_buf;
        if !keep_alive {
            tracing::info!("closing connection on request");
            break;
        }
    }
    if let Ok(mut stream) = stream_read.reunite(stream_write) {
        if let Err(e) = stream.shutdown().await {
            tracing::error!("shutdown failed: {:?}", e);
        }
    } else {
        tracing::error!("reunite failed");
    }
    tracing::info!("finished serving connection");
}
