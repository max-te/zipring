mod borrowed_file;
mod fstree;
mod rc_zip_monoio;
mod request;
pub(crate) mod response;

use std::cell::RefCell;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZero;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use compio::driver::ProactorBuilder;
use compio::fs::File;
use compio::io::AsyncWrite;
use compio::net::{TcpSocket, TcpStream};
use miette::{IntoDiagnostic, Result, WrapErr};
use tracing::Instrument;

use crate::borrowed_file::{BorrowedFile, FdBorrowToken, FdOwner};
use crate::fstree::FsTreeNode;
use crate::request::RequestReader;
use crate::response::respond;

type Buf = Box<[u8]>;

/// Per-connection response buffer: holds the decoded path, then each
/// response chunk. Sized so that typical entries are served in a single write.
const CONNECTION_BUF_SIZE: usize = 64 * 1024;

type BufPool = Rc<RefCell<Vec<Buf>>>;

fn take_buf(pool: &BufPool) -> Buf {
    pool.borrow_mut()
        .pop()
        .unwrap_or_else(|| vec![0u8; CONNECTION_BUF_SIZE].into_boxed_slice())
}

fn return_buf(pool: &BufPool, buf: Buf) {
    assert_eq!(buf.len(), CONNECTION_BUF_SIZE);
    pool.borrow_mut().push(buf);
}

#[derive(Debug)]
enum Never {}

/// Optional `io_uring` setup flags, chosen by the `ZIPRING_URING_FLAGS` environment
/// variable so that a build can be measured against itself without recompiling.
#[derive(Clone, Copy, Default, Debug)]
struct UringFlags {
    single_issuer: bool,
    coop_taskrun: bool,
    defer_taskrun: bool,
}

impl UringFlags {
    /// Read the comma-separated flag list, rejecting names and combinations the
    /// kernel would only refuse later.
    fn from_env() -> Result<Self> {
        let mut flags = Self::default();
        let Ok(list) = std::env::var("ZIPRING_URING_FLAGS") else {
            return Ok(flags);
        };
        for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            match name {
                "single_issuer" => flags.single_issuer = true,
                "coop_taskrun" => flags.coop_taskrun = true,
                "defer_taskrun" => flags.defer_taskrun = true,
                other => {
                    return Err(miette::miette!(
                        "unknown io_uring flag {other:?}; expected single_issuer, coop_taskrun or defer_taskrun"
                    ));
                }
            }
        }
        if flags.defer_taskrun && !flags.single_issuer {
            return Err(miette::miette!(
                "defer_taskrun requires single_issuer, which the kernel enforces"
            ));
        }
        Ok(flags)
    }
}

/// Build a runtime for the calling thread, which then owns its ring.
fn build_runtime(flags: UringFlags) -> Result<compio::runtime::Runtime> {
    let mut proactor = ProactorBuilder::new();
    proactor
        .single_issuer(flags.single_issuer)
        .coop_taskrun(flags.coop_taskrun)
        .defer_taskrun(flags.defer_taskrun);
    compio::runtime::Runtime::builder()
        .with_proactor(proactor)
        .build()
        .into_diagnostic()
        .wrap_err("should be able to start runtime")
}

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
        .map_or_else(
            || {
                // Benchmarks show 8 threads is the sweet spot for this I/O-bound server.
                // Beyond 8, high-concurrency latency improves <8% per added thread.
                // Each thread creates an io_uring runtime, so keeping the count
                // reasonable also avoids memlock exhaustion on constrained hosts.
                std::thread::available_parallelism()
                    .map_or(1, NonZero::get)
                    .min(8)
            },
            NonZero::get,
        );

    let uring_flags = UringFlags::from_env()?;

    let (file, tree) = read_zip_tree(&filepath, uring_flags)?;
    let tree: &'static FsTreeNode = Box::leak(Box::new(tree));
    let file_holder = Box::leak(Box::new(FdOwner::from(file)));
    let mut threads: Vec<_> = (0..n_threads)
        .map(|i| {
            let token = file_holder.token();
            std::thread::spawn(move || -> Result<Never> {
                let rt = build_runtime(uring_flags)?;
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
                        .map(std::string::String::as_str)
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown panic cause");
                    return Err(miette::miette!("Thread panicked: {msg}"));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_zip_tree(
    filepath: &PathBuf,
    uring_flags: UringFlags,
) -> Result<(File, FsTreeNode), miette::Error> {
    build_runtime(uring_flags)?.block_on(async {
        let file = compio::fs::File::open(filepath)
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
    let buf_pool: BufPool = Rc::new(RefCell::new(Vec::new()));
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let socket = TcpSocket::new_v4()
        .await
        .into_diagnostic()
        .wrap_err("could not create socket")?;
    // Every thread binds the same address, so the kernel load-balances accepts across
    // the rings instead of one thread handing connections to the others.
    socket
        .set_reuseport(true)
        .and_then(|()| socket.set_reuseaddr(true))
        .and_then(|()| socket.set_keepalive(true))
        .into_diagnostic()
        .wrap_err("could not set socket options")?;
    socket
        .bind(addr)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("could not bind to {addr}"))?;
    let local_addr = socket
        .local_addr()
        .into_diagnostic()
        .wrap_err("bound socket should have an address")?;
    let listener = socket
        .listen(128)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("could not listen on {addr}"))?;
    if threadid == 0 {
        tracing::info!("Serving file at http://{}", local_addr);
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
                let handle = compio::runtime::spawn(
                    serve(stream, file.clone(), tree, buf_pool.clone()).instrument(span.exit()),
                );
                handle.detach();
            }
            Err(e) => {
                tracing::error!(?threadid, "accepting connection failed: {}", e);
            }
        }
        conid += 1;
    }
}

async fn serve(
    stream: TcpStream,
    file: Rc<BorrowedFile<'_>>,
    tree: &FsTreeNode,
    buf_pool: BufPool,
) {
    let (mut stream_read, mut stream_write) = stream.split();

    let mut reader = RequestReader::new();
    let mut buf = take_buf(&buf_pool);
    loop {
        let request = match reader.next_request(&mut stream_read, buf).await {
            Ok(request) => request,
            Err(buf) => {
                return_buf(&buf_pool, buf);
                break;
            }
        };
        let keep_alive = request.keep_alive();
        buf = match respond(request, &file, tree, &mut stream_write).await {
            Ok(buf) => buf,
            Err(e) => {
                tracing::error!("error responding: {:?}", e);
                break;
            }
        };
        if !keep_alive {
            tracing::info!("closing connection on request");
            return_buf(&buf_pool, buf);
            break;
        }
    }
    if let Err(e) = stream_write.shutdown().await {
        tracing::error!("shutdown failed: {:?}", e);
    }
    tracing::info!("finished serving connection");
}
