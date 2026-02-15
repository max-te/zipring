use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

use divan::Bencher;
use divan::counter::BytesCount;

const SERVER_ADDR: &str = "127.0.0.1:50002";

struct ServerHandle {
    process: Child,
    addr: String,
}

impl ServerHandle {
    fn new(zip_path: &str) -> ServerHandle {
        let exe = env!("CARGO_BIN_EXE_zipring");
        let child = Command::new(exe)
            .arg(zip_path)
            .env("RUST_LOG", "warn")
            .spawn()
            .expect("Failed to start server");
        let server = ServerHandle {
            process: child,
            addr: SERVER_ADDR.to_owned(),
        };
        server.wait();
        server
    }

    fn wait(&self) {
        for _ in 0..200 {
            if TcpStream::connect(self.addr.as_str()).is_ok() {
                thread::sleep(Duration::from_millis(100));
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("Server failed to start");
    }

    fn connect(&self) -> TcpStream {
        TcpStream::connect(self.addr.as_str()).unwrap()
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn make_request(
    path: &str,
    stream: &mut TcpStream,
    encoding: &str,
) -> Result<Vec<u8>, std::io::Error> {
    stream.set_nodelay(true)?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: {}\r\n\r\n",
        path, encoding
    );
    stream.write_all(request.as_bytes())?;

    let mut reader = BufReader::new(stream);

    let mut content_length = None;
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }
        let line = line.trim_end_matches("\r\n");
        if line.is_empty() {
            break;
        }
        if let Some(cl_header) = line.strip_prefix("Content-Length: ") {
            content_length = Some(cl_header.parse::<usize>().unwrap());
        }
    }

    if let Some(len) = content_length {
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body)?;
        Ok(body)
    } else {
        Ok(Vec::new())
    }
}

const ZIP_PATH: &str = "tests/resources/Universal_Declaration_of_Human_Rights.htmlz";

#[divan::bench(sample_size = 10, sample_count = 1000)]
fn request_directory(b: Bencher) {
    let server = ServerHandle::new(ZIP_PATH);
    let mut stream = server.connect();
    let sample = make_request("/images", &mut stream, "identity").unwrap();

    b.counter(BytesCount::of_slice(&sample)).bench_local(|| {
        make_request("/images", &mut stream, "identity").unwrap();
    });
}

#[divan::bench(
    args = ["deflate", "identity", "gzip"],
    sample_size = 10,
)]
fn request_file(b: Bencher, encoding: &str) {
    let server = ServerHandle::new(ZIP_PATH);
    let mut stream = server.connect();
    let sample = make_request("/index.html", &mut stream, encoding).unwrap();

    b.counter(BytesCount::of_slice(&sample)).bench_local(|| {
        make_request("/index.html", &mut stream, encoding).unwrap();
    });
}

#[divan::bench(sample_size = 10, sample_count = 1000)]
fn request_small_file(b: Bencher) {
    let server = ServerHandle::new(ZIP_PATH);
    let mut stream = server.connect();
    let sample = make_request("/metadata.opf", &mut stream, "deflate").unwrap();

    b.counter(BytesCount::of_slice(&sample)).bench_local(|| {
        make_request("/metadata.opf", &mut stream, "deflate").unwrap();
    });
    drop(server);
}

#[divan::bench(args = ["/metadata.opf", "/index.html", "/images"], threads = [0,2,4,8], sample_size = 3, sample_count = 1000)]
fn connect_and_request_parallel(b: Bencher, path: &str) {
    let server = ServerHandle::new(ZIP_PATH);
    let sample = {
        let mut stream = server.connect();
        make_request(path, &mut stream, "deflate").unwrap()
    };
    b.counter(BytesCount::of_slice(&sample)).bench(|| {
        let mut stream = server.connect();
        make_request(path, &mut stream, "deflate").unwrap();
    });
}

fn main() {
    // Run registered benchmarks.
    divan::main();
}
