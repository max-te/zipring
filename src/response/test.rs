use compio::BufResult;
use compio::buf::{IoBuf, IoVectoredBuf};
use compio::io::AsyncWrite;
use rc_zip::parse::{Entry, Method as ZipMethod, Mode, Version};

use super::stream::*;
use crate::Buf;
use crate::fstree::FsTreeNode;
use crate::request::AcceptedEncodings;
use crate::response::status::HttpStatus;

fn run<T>(future: impl Future<Output = T>) -> T {
    compio::runtime::Runtime::new().unwrap().block_on(future)
}

#[test]
fn test_serve_not_found() {
    run(async {
        let mut writer = TestWriter::new();
        ResponseStream::new(&mut writer, make_buf(1024))
            .serve_status(HttpStatus::NotFound)
            .await
            .unwrap();
        let out = String::from_utf8_lossy(&writer.written);
        assert_eq!(
            out.split("\r\n").collect::<Vec<_>>(),
            vec!["HTTP/1.1 404 Not Found", "Content-Length: 0", "", ""]
        );
    });
}

#[test]
fn test_serve_bad_request() {
    run(async {
        let mut writer = TestWriter::new();
        ResponseStream::new(&mut writer, make_buf(1024))
            .serve_status(HttpStatus::BadRequest)
            .await
            .unwrap();
        let out = String::from_utf8_lossy(&writer.written);
        assert_eq!(
            out.split("\r\n").collect::<Vec<_>>(),
            vec!["HTTP/1.1 400 Bad Request", "Content-Length: 0", "", ""]
        );
    });
}

#[test]
fn test_serve_not_modified() {
    run(async {
        let mut writer = TestWriter::new();
        ResponseStream::new(&mut writer, make_buf(1024))
            .serve_not_modified(0xDEAD_BEEF)
            .await
            .unwrap();
        let out = String::from_utf8_lossy(&writer.written);
        assert_eq!(
            out.split("\r\n").collect::<Vec<_>>(),
            vec!["HTTP/1.1 304 Not Modified", "ETag: \"deadbeef\"", "", ""]
        );
    });
}

#[test]
fn test_send_header_with_none_compression() {
    let entry = dummy_entry("style.css", 0xDEAD_BEEF, ZipMethod::Store, 4096, 4096);
    let out = header_of(&entry, None);
    assert_eq!(
        out.split("\r\n").collect::<Vec<_>>(),
        vec![
            "HTTP/1.1 200 OK",
            "Content-Type: text/css",
            "Content-Length: 4096",
            "ETag: \"deadbeef\"",
            "Cache-control: max-age=180, public",
            "",
            "",
        ]
    );
}

#[test]
fn test_send_header_with_gzip_compression() {
    let entry = dummy_entry("style.css", 0xDEAD_BEEF, ZipMethod::Deflate, 1024, 4096);
    let out = header_of(
        &entry,
        Some(&ContentCompression {
            extra_len: 18,
            encoding: "gzip",
        }),
    );
    assert_eq!(
        out.split("\r\n").collect::<Vec<_>>(),
        vec![
            "HTTP/1.1 200 OK",
            "Content-Type: text/css",
            "Content-Encoding: gzip",
            "Content-Length: 1042", // = 1024 + 18
            "ETag: \"deadbeef\"",
            "Cache-control: max-age=180, public",
            "",
            "",
        ]
    );
}

#[test]
fn test_send_header_with_zstd_compression() {
    let entry = dummy_entry("style.css", 0xDEAD_BEEF, ZipMethod::Deflate, 1024, 4096);
    let out = header_of(
        &entry,
        Some(&ContentCompression {
            extra_len: 0,
            encoding: "zstd",
        }),
    );
    assert_eq!(
        out.split("\r\n").collect::<Vec<_>>(),
        vec![
            "HTTP/1.1 200 OK",
            "Content-Type: text/css",
            "Content-Encoding: zstd",
            "Content-Length: 1024",
            "ETag: \"deadbeef\"",
            "Cache-control: max-age=180, public",
            "",
            "",
        ]
    );
}

#[test]
fn test_serve_index_root() {
    run(async {
        let mut writer = TestWriter::new();
        let entries = vec![node_dir("images"), node_file("index.html")];
        ResponseStream::new(&mut writer, make_buf(4096))
            .serve_index(true, &entries)
            .await
            .unwrap();
        let out = String::from_utf8_lossy(&writer.written);
        assert!(
            out.starts_with("HTTP/1.1 200 OK\r\nContent-Type: text/html;")
                && out.contains("Transfer-Encoding: chunked")
        );
        assert!(out.contains("<li class=dir><a href=\"./images/\">images</a>"));
        assert!(out.contains("<li><a href=\"./index.html\">index.html</a>"));
        assert!(!out.contains(".."), "root dir should not have parent link");

        assert!(out.ends_with("0\r\n\r\n"), "should end with final chunk");
    });
}

/// A test writer that captures all bytes written to it.
/// Implements `AsyncWrite` so it can be used with `ResponseStream`.
struct TestWriter {
    written: Vec<u8>,
    writes: usize,
}

impl TestWriter {
    fn new() -> Self {
        Self {
            written: Vec::new(),
            writes: 0,
        }
    }
}

impl AsyncWrite for TestWriter {
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        let data = buf.as_init();
        self.written.extend_from_slice(data);
        self.writes += 1;
        let len = data.len();
        BufResult(Ok(len), buf)
    }

    async fn write_vectored<T: IoVectoredBuf>(&mut self, _buf_vec: T) -> BufResult<usize, T> {
        unimplemented!()
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Create a zeroed-out version field for test entries.
fn dummy_version() -> Version {
    Version {
        host_system: rc_zip::parse::HostSystem::Unix,
        version: 0,
    }
}

/// Create a minimal `Entry` for testing response formatting.
fn dummy_entry(
    name: &str,
    crc32: u32,
    method: ZipMethod,
    compressed_size: u64,
    uncompressed_size: u64,
) -> Entry {
    Entry {
        name: name.to_owned(),
        method,
        comment: String::new(),
        modified: rc_zip::chrono::DateTime::from_timestamp(0, 0).unwrap(),
        created: None,
        accessed: None,
        header_offset: 0,
        reader_version: dummy_version(),
        uid: None,
        gid: None,
        crc32,
        compressed_size,
        uncompressed_size,
        mode: Mode(0o100_644),
        flags: 0,
    }
}

/// Render an entry's response header into a buffer and read it back as text.
fn header_of(entry: &Entry, compression: Option<&ContentCompression>) -> String {
    let mut buf = make_buf(2048);
    let len = write_entry_header(&mut buf, entry, compression).unwrap();
    String::from_utf8(buf[..len].to_vec()).unwrap()
}

/// Create a test buffer of the given size.
fn make_buf(size: usize) -> Buf {
    vec![0u8; size].into_boxed_slice()
}

fn node_dir(name: &str) -> FsTreeNode {
    FsTreeNode::Dir {
        name: name.to_string(),
        children: vec![],
        entry: None,
        is_root: false,
        index_html_index: None,
    }
}

fn node_file(name: &str) -> FsTreeNode {
    FsTreeNode::File {
        name: name.to_string(),
        entry: dummy_entry(name, 0xfefe, ZipMethod::Zstd, 10, 100),
    }
}

/// The archive the integration tests serve. `index.html` in it is large enough that
/// a small response buffer forces the entry loops through several iterations.
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/resources/Universal_Declaration_of_Human_Rights.htmlz"
);

const NOTHING_ACCEPTED: AcceptedEncodings = AcceptedEncodings {
    gzip: false,
    zstd: false,
};
const GZIP_ACCEPTED: AcceptedEncodings = AcceptedEncodings {
    gzip: true,
    zstd: false,
};
const ZSTD_ACCEPTED: AcceptedEncodings = AcceptedEncodings {
    gzip: false,
    zstd: true,
};

/// A response buffer far smaller than `index.html`, so that one entry spans many writes.
const SMALL_BUF: usize = 2048;

#[test]
fn test_serve_deflated_entry_as_gzip_over_many_writes() {
    let mut writer = TestWriter::new();
    let entry = run_with_fixture(&mut writer, SMALL_BUF, "index.html", GZIP_ACCEPTED, |r| r);
    let (head, body) = split_response(&writer.written);

    assert!(head.contains("Content-Encoding: gzip"), "{head}");
    assert!(
        head.contains(&format!("Content-Length: {}", entry.compressed_size + 18)),
        "{head}"
    );
    assert_eq!(
        body.len() as u64,
        entry.compressed_size + 18,
        "gzip framing adds a 10-byte header and an 8-byte trailer"
    );
    assert!(
        writer.writes > 1,
        "a {SMALL_BUF}-byte buffer cannot hold this entry in one write"
    );
    assert_entry_contents(&mut flate2::read::GzDecoder::new(&body[..]), &entry);
}

#[test]
fn test_serve_deflated_entry_inflated_over_many_writes() {
    let mut writer = TestWriter::new();
    let entry = run_with_fixture(
        &mut writer,
        SMALL_BUF,
        "index.html",
        NOTHING_ACCEPTED,
        |r| r,
    );
    let (head, body) = split_response(&writer.written);

    assert!(!head.contains("Content-Encoding"), "{head}");
    assert!(
        head.contains(&format!("Content-Length: {}", entry.uncompressed_size)),
        "{head}"
    );
    assert!(
        writer.writes > 1,
        "inflating this entry overflows the buffer"
    );
    assert_entry_contents(&mut &body[..], &entry);
}

/// An entry with no content still owes its header, which is only ever written
/// alongside the first body bytes.
#[test]
fn test_serve_empty_entry_sends_its_header() {
    let mut writer = TestWriter::new();
    run_with_fixture(&mut writer, 4096, "images/", NOTHING_ACCEPTED, |r| r);
    let (head, body) = split_response(&writer.written);

    assert!(head.contains("Content-Length: 0"), "{head}");
    assert!(body.is_empty());
}

/// A compressed entry whose stream runs past the end of the archive is refused
/// rather than served short.
#[test]
fn test_serve_entry_reaching_past_the_archive_fails() {
    let mut writer = TestWriter::new();
    let err = run(async {
        let file = compio::fs::File::open(FIXTURE).await.unwrap();
        let archive = crate::rc_zip_compio::read_zip_from_file(&file)
            .await
            .unwrap();
        let mut entry = fixture_entry(&archive, "index.html");
        entry.compressed_size = 1 << 20;
        let node = FsTreeNode::File {
            name: entry.name.clone(),
            entry,
        };
        ResponseStream::new(&mut writer, make_buf(SMALL_BUF))
            .serve_node(&file, &node, GZIP_ACCEPTED)
            .await
            .0
            .expect_err("should refuse to serve past the archive's end")
    });
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[test]
fn test_zstd_entry_is_passed_through_when_accepted() {
    let mut writer = TestWriter::new();
    let entry = run_with_fixture(
        &mut writer,
        4096,
        "index.html",
        ZSTD_ACCEPTED,
        |mut entry| {
            entry.method = ZipMethod::Zstd;
            entry.compressed_size = 512;
            entry
        },
    );
    let (head, body) = split_response(&writer.written);

    assert!(head.contains("Content-Encoding: zstd"), "{head}");
    assert!(head.contains("Content-Length: 512"), "{head}");
    assert_eq!(
        body.len() as u64,
        entry.compressed_size,
        "a zstd stream is forwarded verbatim, with no framing of our own"
    );
}

#[test]
fn test_zstd_entry_is_not_announced_when_unaccepted() {
    let mut writer = TestWriter::new();
    run(async {
        let file = compio::fs::File::open(FIXTURE).await.unwrap();
        let archive = crate::rc_zip_compio::read_zip_from_file(&file)
            .await
            .unwrap();
        let mut entry = fixture_entry(&archive, "index.html");
        entry.method = ZipMethod::Zstd;
        let node = FsTreeNode::File {
            name: entry.name.clone(),
            entry,
        };
        // However this ends -- 500 without a zstd decoder, a decode attempt with one --
        // the client must not be told the body is zstd.
        let _ = ResponseStream::new(&mut writer, make_buf(4096))
            .serve_node(&file, &node, NOTHING_ACCEPTED)
            .await;
    });
    let head = String::from_utf8_lossy(&writer.written);
    assert!(!head.contains("zstd"), "{head}");
}

/// A listing too long for one chunk is split, and every chunk must be framed
/// correctly for the client to reassemble the original document.
#[test]
fn test_serve_index_spanning_several_chunks() {
    const INDEX_PREAMBLE: &str = include_str!("index.html");
    // Both listing passes have to survive a chunk boundary, so each spans several.
    let names: Vec<String> = (0..30)
        .map(|i| format!("a-rather-long-name-{i:02}"))
        .collect();
    let entries: Vec<_> = names
        .iter()
        .map(|name| node_dir(name))
        .chain(names.iter().map(|name| node_file(name)))
        .collect();

    let mut writer = TestWriter::new();
    run(async {
        ResponseStream::new(&mut writer, make_buf(1024))
            .serve_index(false, &entries)
            .await
            .unwrap();
    });
    let (head, framed) = split_response(&writer.written);
    assert!(head.contains("Transfer-Encoding: chunked"), "{head}");

    let body = String::from_utf8(decode_chunked(&framed)).unwrap();
    assert!(writer.writes > 2, "60 entries should not fit in one chunk");
    assert!(body.starts_with(INDEX_PREAMBLE));
    assert!(body.contains("<li class=top><a href=\"..\">..</a>"));
    for name in &names {
        assert!(
            body.contains(&format!("<li class=dir><a href=\"./{name}/\">{name}</a>")),
            "{name}"
        );
        assert!(
            body.contains(&format!("<li><a href=\"./{name}\">{name}</a>")),
            "{name}"
        );
    }
}

/// Serve one fixture entry, letting `adjust` alter it first, and hand back the entry
/// as it was served.
fn run_with_fixture(
    writer: &mut TestWriter,
    buflen: usize,
    name: &str,
    accepted_encodings: AcceptedEncodings,
    adjust: impl FnOnce(Entry) -> Entry,
) -> Entry {
    run(async {
        let file = compio::fs::File::open(FIXTURE).await.unwrap();
        let archive = crate::rc_zip_compio::read_zip_from_file(&file)
            .await
            .unwrap();
        let entry = adjust(fixture_entry(&archive, name));
        let node = FsTreeNode::File {
            name: entry.name.clone(),
            entry: entry.clone(),
        };
        ResponseStream::new(writer, make_buf(buflen))
            .serve_node(&file, &node, accepted_encodings)
            .await
            .unwrap();
        entry
    })
}

fn fixture_entry(archive: &rc_zip::parse::Archive, name: &str) -> Entry {
    archive
        .entries()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("fixture should contain {name}"))
        .clone()
}

/// Read `source` to its end and check it against the entry's recorded size and checksum.
fn assert_entry_contents(source: &mut impl std::io::Read, entry: &Entry) {
    let mut content = Vec::new();
    source.read_to_end(&mut content).expect("should decode");
    assert_eq!(content.len() as u64, entry.uncompressed_size);
    let mut crc = flate2::Crc::new();
    crc.update(&content);
    assert_eq!(crc.sum(), entry.crc32);
}

fn split_response(response: &[u8]) -> (String, Vec<u8>) {
    let end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response should have a header");
    (
        String::from_utf8_lossy(&response[..end]).into_owned(),
        response[end + 4..].to_vec(),
    )
}

/// Reassemble a chunked body, insisting on well-formed framing throughout.
fn decode_chunked(framed: &[u8]) -> Vec<u8> {
    let mut rest = framed;
    let mut body = Vec::new();
    loop {
        let eol = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .expect("chunk should begin with its length");
        let len = usize::from_str_radix(str::from_utf8(&rest[..eol]).unwrap(), 16)
            .expect("chunk length should be hexadecimal");
        rest = &rest[eol + 2..];
        if len == 0 {
            assert_eq!(rest, b"\r\n", "terminator should close the body");
            return body;
        }
        body.extend_from_slice(&rest[..len]);
        assert_eq!(&rest[len..len + 2], b"\r\n", "chunk should end with CRLF");
        rest = &rest[len + 2..];
    }
}

#[test]
fn test_position_of_an_absent_byte() {
    assert_eq!(position(b"HTTP/1.1 304", b'x'), None);
    assert_eq!(position(b"", b'x'), None);
    assert_eq!(position(b"ETag: \"xxxxxxxx\"", b'x'), Some(7));
}
