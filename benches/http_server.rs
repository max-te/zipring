use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

const SERVER_ADDR: &str = "127.0.0.1:50002";

fn start_server(zip_path: PathBuf) -> Child {
    let exe = env!("CARGO_BIN_EXE_zipring");
    Command::new(exe)
        .arg(zip_path)
        .env("RUST_LOG", "warn")
        .spawn()
        .expect("Failed to start server")
}

fn stop_server(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
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

fn wait_for_server() {
    for _ in 0..200 {
        if TcpStream::connect(SERVER_ADDR).is_ok() {
            thread::sleep(Duration::from_millis(100));
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("Server failed to start");
}

fn criterion_benchmark(c: &mut criterion::Criterion) {
    let mut group = c.benchmark_group("http_server");

    let zip_path = PathBuf::from("tests/resources/Universal_Declaration_of_Human_Rights.htmlz");

    group.bench_function("request_directory", |b| {
        let server = start_server(zip_path.clone());
        wait_for_server();

        let mut stream = TcpStream::connect(SERVER_ADDR).unwrap();
        b.iter(|| {
            make_request("/images", &mut stream, "identity").unwrap();
        });

        stop_server(server);
    });

    group.bench_function("request_file_deflate", |b| {
        let server = start_server(zip_path.clone());
        wait_for_server();

        let mut stream = TcpStream::connect(SERVER_ADDR).unwrap();
        b.iter(|| {
            make_request("/index.html", &mut stream, "deflate").unwrap();
        });

        stop_server(server);
    });

    group.bench_function("request_file_identity", |b| {
        let server = start_server(zip_path.clone());
        wait_for_server();

        let mut stream = TcpStream::connect(SERVER_ADDR).unwrap();
        b.iter(|| {
            make_request("/index.html", &mut stream, "identity").unwrap();
        });

        stop_server(server);
    });

    group.bench_function("request_file_gzip", |b| {
        let server = start_server(zip_path.clone());
        wait_for_server();

        let mut stream = TcpStream::connect(SERVER_ADDR).unwrap();
        b.iter(|| {
            make_request("/index.html", &mut stream, "gzip").unwrap();
        });

        stop_server(server);
    });
    group.bench_function("request_small_file", |b| {
        let server = start_server(zip_path.clone());
        wait_for_server();

        let mut stream = TcpStream::connect(SERVER_ADDR).unwrap();
        b.iter(|| {
            make_request("/metadata.opf", &mut stream, "deflate").unwrap();
        });

        stop_server(server);
    });

    group.bench_function("connect_and_request_small_file", |b| {
        let server = start_server(zip_path.clone());
        wait_for_server();

        b.iter(|| {
            let mut stream = TcpStream::connect(SERVER_ADDR).unwrap();
            make_request("/metadata.opf", &mut stream, "deflate").unwrap();
        });

        stop_server(server);
    });

    group.finish();
}

criterion::criterion_group!(benches, criterion_benchmark);
criterion::criterion_main!(benches);
