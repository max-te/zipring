use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::mem::ManuallyDrop;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::AtomicU16;
use std::thread;
use std::time::Duration;

struct ServerHandle {
    process: ManuallyDrop<Child>,
    #[allow(unused)]
    port: u16,
}

impl ServerHandle {
    fn socket_address(&self) -> impl ToSocketAddrs {
        ("127.0.0.1", self.port)
    }

    fn uri(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn wait(&self) {
        for attempt in 0..600 {
            if TcpStream::connect(self.socket_address()).is_ok() {
                thread::sleep(Duration::from_millis(200));
                return;
            }
            if attempt % 50 == 0 && attempt > 0 {
                eprintln!(
                    "Waiting for server on port {}... ({} attempts)",
                    self.port, attempt
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "Server failed to start on port {} after 30 seconds",
            self.port
        );
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        let mut process = unsafe { ManuallyDrop::take(&mut self.process) };
        let _ = process.kill();
        let res = process.wait_with_output();
        match res {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                println!("===== Server stdout:\n{stdout}\n=====");
                println!("===== Server stderr:\n{stderr}\n=====");
            }
            Err(err) => eprintln!("Error waiting for server: {err}"),
        }
    }
}

fn start_server(zip_path: impl Into<PathBuf>) -> ServerHandle {
    let port = find_free_port();
    let exe = env!("CARGO_BIN_EXE_zipring");
    let server = ServerHandle {
        process: ManuallyDrop::new(
            std::process::Command::new(exe)
                .arg(zip_path.into())
                .env("PORT", port.to_string())
                // These tests run in parallel and none of them cares about thread
                // scaling. One io_uring per server rather than eight keeps the suite
                // clear of the memlock ceiling, which it was starting to hit.
                .env("ZIPRING_THREADS", "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("Failed to start server"),
        ),
        port,
    };
    server.wait();
    server
}

fn find_free_port() -> u16 {
    static NEXT_PORT_OFFSET: AtomicU16 = AtomicU16::new(20000);
    let mut port;
    if let Ok(slot) = std::env::var("NEXTEST_TEST_GLOBAL_SLOT") {
        let offset = slot.parse::<u16>().unwrap();
        port = NEXT_PORT_OFFSET.load(std::sync::atomic::Ordering::SeqCst) + offset;
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_err() {
                return port;
            }
            port = offset + NEXT_PORT_OFFSET.fetch_add(100, std::sync::atomic::Ordering::SeqCst);
        }
    } else {
        port = NEXT_PORT_OFFSET.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_err() {
                return port;
            }
            port = NEXT_PORT_OFFSET.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

fn make_request(
    server: &ServerHandle,
    path: &str,
    encodings: &str,
) -> Result<(String, Vec<u8>), std::io::Error> {
    let mut stream = TcpStream::connect(server.socket_address())?;
    stream.set_nodelay(true)?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAccept-Encoding: {encodings}\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;

    let mut reader = BufReader::new(&stream);

    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;

    let mut content_length = None;
    let mut chunked = false;
    let mut headers = String::new();
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
        writeln!(headers, "{line}").unwrap();
        if let Some(cl_header) = line.strip_prefix("Content-Length: ") {
            content_length = Some(cl_header.parse::<usize>().unwrap());
        }
        if line == "Transfer-Encoding: chunked" {
            chunked = true;
        }
    }

    let body = if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf)?;
        buf
    } else if chunked {
        let mut buf = Vec::new();
        loop {
            let mut chunk_size = String::new();
            reader.read_line(&mut chunk_size)?;
            let chunk_size = <usize>::from_str_radix(chunk_size.trim_end_matches("\r\n"), 16)
                .expect("Chunk size should be hex");
            if chunk_size == 0 {
                reader.read_exact(&mut [0; 2])?;
                break;
            }
            reader = {
                let mut chunk = reader.take(chunk_size as u64);
                chunk.read_to_end(&mut buf)?;
                chunk.into_inner()
            };
            reader.read_exact(&mut [0; 2])?;
        }
        buf
    } else {
        Vec::new()
    };
    assert!(
        reader.buffer().is_empty(),
        "Too much data in response stream"
    );

    Ok((status_line + &headers, body))
}

const TEST_FILE: &str = "tests/resources/Universal_Declaration_of_Human_Rights.htmlz";

#[test]
fn test_request_root() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    let (headers, _body) = make_request(&server, "/", "gzip").expect("Request failed");
    assert!(headers.contains("200 OK"), "Root should return 200");
    assert!(
        headers.contains("Content-Type: text/html"),
        "Root should be HTML"
    );
}

#[test]
fn test_request_split_across_segments() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    let mut stream = TcpStream::connect(server.socket_address()).expect("connect failed");
    stream.set_nodelay(true).expect("nodelay failed");

    // Two writes with the socket flushed between them, so the server has to read twice.
    stream
        .write_all(b"GET /index.html HTTP/1.1\r\nHost: local")
        .expect("first segment failed");
    std::thread::sleep(Duration::from_millis(50));
    stream
        .write_all(b"host\r\nConnection: close\r\n\r\n")
        .expect("second segment failed");

    let mut response = String::new();
    BufReader::new(&stream)
        .read_line(&mut response)
        .expect("response failed");
    assert!(
        response.contains("200 OK"),
        "a request split across segments should be served, got {response:?}"
    );
}

#[test]
fn test_pipelined_requests() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    let mut stream = TcpStream::connect(server.socket_address()).expect("connect failed");
    stream.set_nodelay(true).expect("nodelay failed");

    // Both requests in one write, so the server reads them together and must answer
    // the second from bytes it buffered while answering the first.
    stream
        .write_all(
            b"GET /metadata.opf HTTP/1.1\r\nHost: localhost\r\n\r\n\
              GET /style.css HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .expect("write failed");

    let mut reader = BufReader::new(&stream);
    for expected in ["application/oebps-package+xml", "text/css"] {
        let mut content_type = None;
        let mut body_length = 0;
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("header read failed");
            assert!(
                read != 0,
                "connection closed before the {expected} response"
            );
            let line = line.trim_end_matches("\r\n");
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Type: ") {
                content_type = Some(value.to_string());
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                body_length = value.parse::<u64>().expect("length should parse");
            }
        }
        assert_eq!(
            content_type.as_deref(),
            Some(expected),
            "pipelined responses should come back in order"
        );
        std::io::copy(&mut reader.by_ref().take(body_length), &mut std::io::sink())
            .expect("body read failed");
    }
}

#[test]
fn test_request_index_html() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    let (headers, body) = make_request(&server, "/index.html", "gzip").expect("Request failed");
    assert!(headers.contains("200 OK"), "index.html should return 200");
    assert!(
        headers.contains("Content-Type: text/html"),
        "index.html should be HTML"
    );
    assert!(!body.is_empty(), "index.html should have content");
    assert!(
        body.len() > 1000,
        "index.html should be larger than 1KB (actual: {} bytes)",
        body.len()
    );
}

#[test]
fn test_request_style_css() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    let (headers, body) = make_request(&server, "/style.css", "gzip").expect("Request failed");
    assert!(headers.contains("200 OK"), "style.css should return 200");
    assert!(
        headers.contains("Content-Type: text/css"),
        "style.css should be CSS"
    );
    assert!(!body.is_empty(), "style.css should have content");
}

#[test]
fn test_request_directory_images() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    let (headers, body) = make_request(&server, "/images", "gzip").expect("Request failed");
    let body = String::from_utf8(body).expect("Body should be UTF-8");
    assert!(
        headers.contains("200 OK"),
        "images directory should return 200"
    );
    assert!(
        headers.contains("Content-Type: text/html"),
        "images directory listing should be HTML"
    );
    assert!(
        !body.is_empty(),
        "images directory listing should have content"
    );
    assert!(
        body.contains("000000.png"),
        "images directory listing should contain 000000.png"
    );
}

#[test]
fn test_request_image_file() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    let (headers, body) =
        make_request(&server, "/images/000000.png", "gzip").expect("Request failed");
    assert!(headers.contains("200 OK"), "PNG should return 200");
    assert!(
        headers.contains("Content-Type: image/png"),
        "PNG should have image/png type"
    );
    assert!(!body.is_empty(), "PNG should have content");
}

#[test]
fn test_request_nonexistent_file() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    let (headers, _body) =
        make_request(&server, "/nonexistent.txt", "gzip").expect("Request failed");
    assert!(
        headers.contains("404 Not Found"),
        "Nonexistent file should return 404"
    );
}

/// A requested path may be any bytes at all, multi-byte characters and invalid
/// UTF-8 among them, and must be answered rather than dropped.
#[test]
fn test_request_path_that_is_not_ascii() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    for path in ["/%C3%A9/x", "/%FF", "/%FF/x", "/images/%FF"] {
        let (headers, _body) =
            make_request(&server, path, "gzip").unwrap_or_else(|e| panic!("{path}: {e}"));
        assert!(headers.contains("404 Not Found"), "{path}: {headers}");
    }
}

#[test]
fn test_request_etag_caching() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    // First request to get the ETag
    let (headers, _body) = make_request(&server, "/metadata.opf", "gzip").expect("Request failed");
    assert!(headers.contains("200 OK"));

    let etag = headers
        .lines()
        .find(|line| line.contains("ETag:"))
        .map(|line| {
            line.trim_start_matches("ETag: ")
                .trim_matches('"')
                .to_string()
        });

    assert!(etag.is_some(), "Response should have ETag header");

    // Make a second request with If-None-Match
    let mut stream = TcpStream::connect(server.socket_address()).expect("Connection failed");
    stream.set_nodelay(true).unwrap();
    let request = format!(
        "GET /metadata.opf HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: \"{}\"\r\nConnection: close\r\n\r\n",
        etag.unwrap()
    );
    stream.write_all(request.as_bytes()).unwrap();

    let mut reader = BufReader::new(&stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();

    assert!(
        status_line.contains("304 Not Modified"),
        "ETag match should return 304 Not Modified"
    );
}

#[test]
fn test_request_multiple_sequential() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    // Make multiple requests with delays between them
    let (headers1, _body) =
        make_request(&server, "/index.html", "gzip").expect("First request failed");
    assert!(headers1.contains("200 OK"));
    thread::sleep(Duration::from_millis(50));

    let (headers2, _) = make_request(&server, "/style.css", "gzip").expect("Second request failed");
    assert!(headers2.contains("200 OK"));
    thread::sleep(Duration::from_millis(50));

    let (headers3, _) =
        make_request(&server, "/metadata.opf", "gzip").expect("Third request failed");
    assert!(headers3.contains("200 OK"));
}

#[test]
fn test_requests_ureq() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    let client = ureq::Agent::new_with_defaults();

    for _ in 0..10 {
        let mut response = client.get(server.uri()).call().expect("Request failed");
        assert_eq!(response.status(), 200);
        assert!(response.headers().get("Content-Encoding").is_some());
        let body = response.body_mut().read_to_vec();
        assert!(body.is_ok());
    }
}

#[test]
fn test_response_has_proper_headers() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    let (headers, _body) = make_request(&server, "/index.html", "gzip").expect("Request failed");

    // Check for required headers
    assert!(
        headers.contains("Content-Type:"),
        "Response should have Content-Type"
    );
    assert!(
        headers.contains("Content-Length:"),
        "Response should have Content-Length"
    );
    assert!(headers.contains("ETag:"), "Response should have ETag");
    assert!(
        headers.contains("Cache-control:"),
        "Response should have Cache-control"
    );
}

#[test]
fn test_content_encoding_gzip() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    // Deflate-compressed files in the zip should be served with Content-Encoding: gzip
    let (headers, body) = make_request(&server, "/index.html", "gzip").expect("Request failed");
    assert!(headers.contains("200 OK"));
    assert!(
        headers.contains("Content-Encoding: gzip"),
        "Deflate files should be served as gzip"
    );
    assert!(!body.is_empty(), "Gzipped content should have body");

    // Gzip format starts with magic bytes 0x1f 0x8b
    assert_eq!(
        body[0], 0x1f,
        "Gzipped content should start with gzip magic byte 0x1f"
    );
    assert_eq!(
        body[1], 0x8b,
        "Gzipped content should start with gzip magic byte 0x8b"
    );

    let mut decoder = flate2::read::GzDecoder::new(&body[..]);
    let mut s = String::new();
    decoder
        .read_to_string(&mut s)
        .expect("should be decodable gzip");

    assert!(s.ends_with("</html>"));
}

#[test]
fn test_content_encoding_decompressed() {
    let zip_path = PathBuf::from(TEST_FILE);
    let server = start_server(zip_path);

    let (headers, body) = make_request(&server, "/index.html", "brotli").expect("Request failed");
    assert!(headers.contains("200 OK"));
    assert!(
        !headers.contains("Content-Encoding:"),
        "Files should be served decompressed when client doesn't support compression"
    );
    assert!(!body.is_empty(), "content should have body");
    let body = String::from_utf8(body).expect("should be valid utf8");
    assert!(body.ends_with("</html>"));
}

#[test]
fn test_content_length_vs_compressed_size() {
    let server = start_server(TEST_FILE);

    // When serving compressed content, Content-Length should match the compressed size
    let (headers, body) = make_request(&server, "/index.html", "gzip").expect("Request failed");

    // Extract Content-Length from headers
    let content_length = headers
        .lines()
        .find(|line| line.starts_with("Content-Length:"))
        .and_then(|line| line.split(": ").nth(1))
        .and_then(|len| len.parse::<usize>().ok())
        .expect("Should have Content-Length header");

    // The (encoded) body should match the Content-Length
    assert_eq!(
        body.len(),
        content_length,
        "Received body size should match Content-Length header"
    );
}

#[test]
fn test_connection_close() {
    let server = start_server(TEST_FILE);
    let mut stream = TcpStream::connect(server.socket_address()).expect("Connection failed");
    stream.set_nodelay(true).unwrap();
    let request = "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).unwrap();

    let mut reader = BufReader::new(&stream);
    let mut body = Vec::new();
    reader.read_to_end(&mut body).unwrap();
    assert!(!body.is_empty(), "body should not be empty");
    let mut buf = Vec::new();
    // Zero-sized read indicates EOF
    assert_eq!(stream.read_to_end(&mut buf).unwrap(), 0);
}

/// Archives whose entry names are not ASCII, in both spellings the format allows:
/// one sets the UTF-8 flag, the other leaves it clear and stores shift-jis bytes,
/// which `rc_zip` transcodes. Both hold `日本語のファイル.txt`, `ソフトウェア設定.txt`
/// and, under a non-ascii directory, `写真/東京タワー.txt`.
const NON_ASCII_UTF8_FILE: &str = "tests/resources/non_ascii_utf8.zip";
const NON_ASCII_SHIFT_JIS_FILE: &str = "tests/resources/non_ascii_shift_jis.zip";

#[test]
fn test_serve_utf8_flagged_archive() {
    let server = start_server(PathBuf::from(NON_ASCII_UTF8_FILE));

    let (headers, body) = make_request(&server, "/", "gzip").expect("Request failed");
    let listing = String::from_utf8(body).expect("listing should be UTF-8");
    assert!(headers.contains("200 OK"), "{headers}");
    assert!(listing.contains(">日本語のファイル.txt</a>"), "{listing}");
    assert!(listing.contains(">写真</a>"), "{listing}");
    assert!(
        listing.contains("href=\"./%E5%86%99%E7%9C%9F/\""),
        "a non-ascii name should be percent-encoded in its href: {listing}"
    );

    let nested = "/%E5%86%99%E7%9C%9F/%E6%9D%B1%E4%BA%AC%E3%82%BF%E3%83%AF%E3%83%BC.txt";
    let (headers, body) = make_request(&server, nested, "gzip").expect("Request failed");
    assert!(headers.contains("200 OK"), "{headers}");
    assert_eq!(
        String::from_utf8(body).expect("content should be UTF-8"),
        "contents of 写真/東京タワー.txt\n"
    );
}

/// Which encoding `rc_zip` guesses for an archive that declares none is its own
/// business -- it currently recovers the Japanese names exactly. Ours is that the
/// archive is served at all, and that every name it lists is reachable by the href
/// given for it.
#[test]
fn test_serve_archive_without_an_encoding_flag() {
    let server = start_server(PathBuf::from(NON_ASCII_SHIFT_JIS_FILE));

    let (headers, body) = make_request(&server, "/", "gzip").expect("Request failed");
    let listing = String::from_utf8(body).expect("listing should be UTF-8");
    assert!(headers.contains("200 OK"), "{headers}");

    let hrefs = hrefs_of(&listing);
    assert_eq!(hrefs.len(), 3, "every entry should be listed: {listing}");

    for href in hrefs {
        let path = format!("/{}", href.trim_start_matches("./"));
        let (headers, body) = make_request(&server, &path, "gzip")
            .unwrap_or_else(|e| panic!("request for {path} failed: {e}"));
        assert!(headers.contains("200 OK"), "{path}: {headers}");
        if !path.ends_with('/') {
            let content = String::from_utf8(body).expect("content should be UTF-8");
            assert!(content.starts_with("contents of "), "{path}: {content}");
        }
    }
}

/// The href of every entry a listing offers, in the order they appear.
fn hrefs_of(listing: &str) -> Vec<&str> {
    listing
        .split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .collect()
}
