mod buf_result;
mod fstree;
mod rc_zip_compio;
mod request;
pub(crate) mod response;

use std::cell::RefCell;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZero;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use compio::BufResult;
use compio::driver::ProactorBuilder;
use compio::fs::File;
use compio::io::AsyncWrite;
use compio::net::{TcpSocket, TcpStream};
use miette::{IntoDiagnostic, Result, WrapErr};
use tracing::Instrument;

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
    fn from_env() -> Result<Self> {
        match std::env::var("ZIPRING_URING_FLAGS") {
            Ok(list) => Self::parse(&list),
            Err(_) => Ok(Self::default()),
        }
    }

    /// Read the comma-separated flag list, rejecting names and combinations the
    /// kernel would only refuse later.
    fn parse(list: &str) -> Result<Self> {
        let mut flags = Self::default();
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

    let file = std::fs::File::open(&filepath)
        .into_diagnostic()
        .wrap_err_with(|| format!("could not open {}", filepath.display()))?;
    let tree: &'static FsTreeNode = Box::leak(Box::new(read_zip_tree(&file, uring_flags)?));
    let mut threads: Vec<_> = (0..n_threads)
        .map(|i| {
            let file = dup(&file)?;
            Ok(std::thread::spawn(move || -> Result<Never> {
                let rt = build_runtime(uring_flags)?;
                rt.block_on(inner_main(i, into_compio(file), port, tree))
            }))
        })
        .collect::<Result<_>>()?;

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

/// Duplicate the zip's fd so another thread can own a handle to it.
///
/// The dup shares one file description, so every thread reads the same bytes.
fn dup(file: &std::fs::File) -> Result<std::fs::File> {
    file.try_clone()
        .into_diagnostic()
        .wrap_err("could not duplicate zip file descriptor")
}

/// Turn an owned fd into a compio handle for the runtime that will read from it.
///
/// Each thread builds its own handle because compio's files are `!Send` -- their
/// shared-fd refcount is an `Rc` unless the `sync` feature is on -- so the fd
/// travels between threads as a [`std::fs::File`] instead.
fn into_compio(file: std::fs::File) -> File {
    // SAFETY: `file` owns this fd and gives it up here; the returned handle is the
    // sole owner and closes it on drop.
    unsafe { File::from_raw_fd(file.into_raw_fd()) }
}

fn read_zip_tree(file: &std::fs::File, uring_flags: UringFlags) -> Result<FsTreeNode> {
    let file = dup(file)?;
    build_runtime(uring_flags)?.block_on(async {
        let file = into_compio(file);
        let zip = rc_zip_compio::read_zip_from_file(&file)
            .await
            .into_diagnostic()
            .wrap_err("could not parse zip")?;

        let mut tree = FsTreeNode::root();
        for entry in zip.entries() {
            tree.insert(entry.clone());
        }
        tree.recursive_sort();
        Ok(tree)
    })
}

async fn inner_main(
    threadid: usize,
    file: File,
    port: u16,
    tree: &'static FsTreeNode,
) -> Result<Never> {
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

async fn serve(stream: TcpStream, file: File, tree: &FsTreeNode, buf_pool: BufPool) {
    let (mut stream_read, mut stream_write) = stream.split();

    let mut reader = RequestReader::new();
    let mut buf = take_buf(&buf_pool);
    loop {
        let request;
        BufResult(request, buf) = reader.next_request(&mut stream_read, buf).await;
        let Ok(Some(request)) = request else {
            return_buf(&buf_pool, buf);
            break;
        };
        let keep_alive = request.keep_alive();
        let res;
        BufResult(res, buf) = respond(request, buf, &file, tree, &mut stream_write).await;
        if let Err(e) = res {
            tracing::error!("error responding: {:?}", e);
            return_buf(&buf_pool, buf);
            break;
        }
        if let Err(e) = stream_write.flush().await {
            tracing::error!("error responding: {:?}", e);
            return_buf(&buf_pool, buf);
            break;
        }
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn an_absent_flag_list_leaves_every_flag_off() {
        let flags = UringFlags::parse("").unwrap();
        assert!(!flags.single_issuer && !flags.coop_taskrun && !flags.defer_taskrun);
    }

    #[test]
    fn a_single_flag_is_recognised() {
        let flags = UringFlags::parse("single_issuer").unwrap();
        assert!(flags.single_issuer);
        assert!(!flags.coop_taskrun && !flags.defer_taskrun);
    }

    /// Surrounding space and stray separators are tolerated, so that a list can be
    /// assembled by a shell script without care.
    #[test]
    fn the_full_list_is_recognised_despite_untidy_separators() {
        let flags = UringFlags::parse(" coop_taskrun ,, defer_taskrun , single_issuer,").unwrap();
        assert!(flags.single_issuer && flags.coop_taskrun && flags.defer_taskrun);
    }

    #[test]
    fn defer_taskrun_alone_is_rejected() {
        let err = UringFlags::parse("defer_taskrun").unwrap_err().to_string();
        assert!(err.contains("single_issuer"), "{err}");
    }

    #[test]
    fn an_unknown_name_is_rejected() {
        let err = UringFlags::parse("coop_taskrun,bogus")
            .unwrap_err()
            .to_string();
        assert!(err.contains("bogus"), "{err}");
    }

    /// The environment is read into the very flags that `parse` yields.
    #[test]
    fn from_env_reads_the_flag_list() {
        // SAFETY: no other test reads or writes this variable.
        unsafe { std::env::set_var("ZIPRING_URING_FLAGS", "coop_taskrun") };
        let flags = UringFlags::from_env().unwrap();
        unsafe { std::env::remove_var("ZIPRING_URING_FLAGS") };
        assert!(flags.coop_taskrun);
        assert!(!flags.single_issuer && !flags.defer_taskrun);
    }
}
