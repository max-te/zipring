use monoio::BufResult;
use monoio::buf::{IoBuf, IoVecBuf};
use monoio::io::AsyncWriteRent;
use rc_zip::parse::{Entry, Method as ZipMethod, Mode, Version};

use super::stream::*;
use crate::Buf;
use crate::fstree::FsTreeNode;
use crate::response::status::HttpStatus;

#[monoio::test]
async fn test_serve_not_found() {
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
}

#[monoio::test]
async fn test_serve_bad_request() {
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
}

#[monoio::test]
async fn test_serve_not_modified() {
    let mut writer = TestWriter::new();
    ResponseStream::new(&mut writer, make_buf(1024))
        .serve_not_modified(0xDEADBEEF)
        .await
        .unwrap();
    let out = String::from_utf8_lossy(&writer.written);
    assert_eq!(
        out.split("\r\n").collect::<Vec<_>>(),
        vec!["HTTP/1.1 304 Not Modified", "ETag: \"efbeadde\"", "", ""]
    );
}

#[monoio::test]
async fn test_send_header_with_none_compression() {
    let mut writer = TestWriter::new();
    let entry = dummy_entry("style.css", 0xDEADBEEF, ZipMethod::Store, 4096, 4096);
    ResponseStream::new(&mut writer, make_buf(2048))
        .send_entry_header(&entry, None)
        .await
        .unwrap()
        .into_buf();
    let out = String::from_utf8(writer.written).unwrap();
    assert_eq!(
        out.split("\r\n").collect::<Vec<_>>(),
        vec![
            "HTTP/1.1 200 OK",
            "Content-Type: text/css",
            "Content-Length: 4096",
            "ETag: \"efbeadde\"",
            "Cache-control: max-age=180, public",
            "",
            "",
        ]
    )
}

#[monoio::test]
async fn test_send_header_with_gzip_compression() {
    let mut writer = TestWriter::new();
    let entry = dummy_entry("style.css", 0xDEADBEEF, ZipMethod::Deflate, 1024, 4096);
    ResponseStream::new(&mut writer, make_buf(2048))
        .send_entry_header(
            &entry,
            Some(&ContentCompression {
                extra_len: 18,
                encoding: "gzip",
            }),
        )
        .await
        .unwrap()
        .into_buf();
    let out = String::from_utf8(writer.written).unwrap();
    assert_eq!(
        out.split("\r\n").collect::<Vec<_>>(),
        vec![
            "HTTP/1.1 200 OK",
            "Content-Type: text/css",
            "Content-Encoding: gzip",
            "Content-Length: 1042", // = 1024 + 18
            "ETag: \"efbeadde\"",
            "Cache-control: max-age=180, public",
            "",
            "",
        ]
    )
}

#[monoio::test]
async fn test_send_header_with_zstd_compression() {
    let mut writer = TestWriter::new();
    let entry = dummy_entry("style.css", 0xDEADBEEF, ZipMethod::Deflate, 1024, 4096);
    ResponseStream::new(&mut writer, make_buf(2048))
        .send_entry_header(
            &entry,
            Some(&ContentCompression {
                extra_len: 0,
                encoding: "zstd",
            }),
        )
        .await
        .unwrap()
        .into_buf();
    let out = String::from_utf8(writer.written).unwrap();
    assert_eq!(
        out.split("\r\n").collect::<Vec<_>>(),
        vec![
            "HTTP/1.1 200 OK",
            "Content-Type: text/css",
            "Content-Encoding: zstd",
            "Content-Length: 1024",
            "ETag: \"efbeadde\"",
            "Cache-control: max-age=180, public",
            "",
            "",
        ]
    )
}

#[monoio::test]
async fn test_serve_index_root() {
    let mut writer = TestWriter::new();
    let entries = vec![node_dir("images"), node_file("index.html")];
    ResponseStream::new(&mut writer, make_buf(4096))
        .serve_index(true, &entries)
        .await
        .unwrap()
        .into_buf();
    let out = String::from_utf8_lossy(&writer.written);
    assert!(
        out.starts_with("HTTP/1.1 200 OK\r\nContent-Type: text/html;")
            && out.contains("Transfer-Encoding: chunked")
    );
    assert!(out.contains("<li class=dir><a href=\"./images/\">images</a>"));
    assert!(out.contains("<li><a href=\"./index.html\">index.html</a>"));
    assert!(!out.contains(".."), "root dir should not have parent link");

    assert!(out.ends_with("0\r\n\r\n"), "should end with final chunk");
}

/// A test writer that captures all bytes written to it.
/// Implements `AsyncWriteRent` so it can be used with `ResponseStream`.
struct TestWriter {
    written: Vec<u8>,
}

impl TestWriter {
    fn new() -> Self {
        Self {
            written: Vec::new(),
        }
    }
}

impl AsyncWriteRent for TestWriter {
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        let len = buf.bytes_init();

        let data = {
            let ptr = buf.read_ptr();
            // SAFETY: IoBuf contract guarantees `bytes_init()` bytes are initialized.
            // The buffer is `'static` so the pointer is valid for the duration of this call.
            unsafe { std::slice::from_raw_parts(ptr, len) }
        };
        self.written.extend_from_slice(data);
        (Ok(len), buf)
    }

    async fn writev<T: IoVecBuf>(&mut self, _buf_vec: T) -> BufResult<usize, T> {
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
        mode: Mode(0o100644),
        flags: 0,
    }
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
