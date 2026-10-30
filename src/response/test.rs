use compio::BufResult;
use compio::buf::{IoBuf, IoVectoredBuf};
use compio::fs::File;
use compio::io::AsyncWrite;
use rc_zip::parse::{Entry, Method as ZipMethod, Mode, Version};

use super::body::{EntryBody, InflaterPool};
use super::coding::NativeCoding;
use super::stream::*;
use crate::Buf;
use crate::fstree::{FsTreeNode, Target};
use crate::request::{AcceptedEncodings, Headers};
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
    let out = header_of(&entry, Some(NativeCoding::Gzip));
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
    let entry = dummy_entry("style.css", 0xDEAD_BEEF, ZipMethod::Zstd, 1024, 4096);
    let out = header_of(&entry, Some(NativeCoding::Zstd));
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
fn header_of(entry: &Entry, native: Option<NativeCoding>) -> String {
    let mut buf = make_buf(2048);
    let len = write_entry_header(&mut buf, entry, native).unwrap();
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
    let entry = run_with_fixture(
        &mut writer,
        SMALL_BUF,
        "index.html",
        accepting(GZIP_ACCEPTED),
        |r| r,
    );
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

#[cfg(feature = "deflate")]
#[test]
fn test_serve_deflated_entry_inflated_over_many_writes() {
    let mut writer = TestWriter::new();
    let entry = run_with_fixture(
        &mut writer,
        SMALL_BUF,
        "index.html",
        accepting(NOTHING_ACCEPTED),
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

/// Stored bytes are the content itself, whatever codings the client would take.
#[test]
fn test_serve_stored_entry_verbatim_over_many_writes() {
    let content: Vec<u8> = (0..=250).cycle().take(3 * SMALL_BUF).collect();
    let path = std::env::temp_dir().join(format!("zipring-stored-{}.zip", std::process::id()));
    std::fs::write(
        &path,
        crate::rc_zip_compio::test::stored_zip("data.bin", &[], &content),
    )
    .unwrap();

    let mut writer = TestWriter::new();
    run(async {
        let file = compio::fs::File::open(&path).await.unwrap();
        let archive = crate::rc_zip_compio::read_zip_from_file(&file)
            .await
            .unwrap();
        let entry = fixture_entry(&archive, "data.bin");
        ResponseStream::new(&mut writer, make_buf(SMALL_BUF))
            .serve_target(
                &file,
                Target::Entry(&entry),
                accepting(GZIP_ACCEPTED),
                &InflaterPool::default(),
            )
            .await
            .unwrap();
    });
    std::fs::remove_file(&path).unwrap();
    let (head, body) = split_response(&writer.written);

    assert!(!head.contains("Content-Encoding"), "{head}");
    assert!(
        head.contains(&format!("Content-Length: {}", content.len())),
        "{head}"
    );
    assert!(writer.writes > 1);
    assert_eq!(body, content);
}

/// The output spans several decoder windows, and the input several refills.
#[cfg(feature = "deflate")]
#[test]
fn test_serve_long_deflated_entry_inflated() {
    let content: Vec<u8> = (0..80_000u32)
        .flat_map(|i| format!("{} ", i.wrapping_mul(2_654_435_761) % 1000).into_bytes())
        .collect();
    let path = std::env::temp_dir().join(format!("zipring-deflated-{}.zip", std::process::id()));
    let zip = crate::rc_zip_compio::test::deflated_zip("long.txt", &content);
    assert!(zip.len() > 32 * 1024, "the stream should need many reads");
    assert!(
        content.len() > 4 * 64 * 1024,
        "the output should need many windows"
    );
    std::fs::write(&path, zip).unwrap();

    let mut writer = TestWriter::new();
    run(async {
        let file = compio::fs::File::open(&path).await.unwrap();
        let archive = crate::rc_zip_compio::read_zip_from_file(&file)
            .await
            .unwrap();
        let entry = fixture_entry(&archive, "long.txt");
        ResponseStream::new(&mut writer, make_buf(SMALL_BUF))
            .serve_target(
                &file,
                Target::Entry(&entry),
                accepting(NOTHING_ACCEPTED),
                &InflaterPool::default(),
            )
            .await
            .unwrap();
    });
    std::fs::remove_file(&path).unwrap();
    let (head, body) = split_response(&writer.written);

    assert!(!head.contains("Content-Encoding"), "{head}");
    assert!(
        head.contains(&format!("Content-Length: {}", content.len())),
        "{head}"
    );
    assert!(body == content, "inflated body differs from the content");
}

/// A method without a native coding goes through `rc_zip`'s own decoder, behind
/// the staged header and over many writes.
#[cfg(feature = "bzip2")]
#[test]
fn test_serve_bzip2_entry_decoded_over_many_writes() {
    const BZIP2_FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/resources/bzip2.zip");
    let mut writer = TestWriter::new();
    let entry = run(async {
        let file = File::open(BZIP2_FIXTURE).await.unwrap();
        let archive = crate::rc_zip_compio::read_zip_from_file(&file)
            .await
            .unwrap();
        let entry = fixture_entry(&archive, "x.txt");
        ResponseStream::new(&mut writer, make_buf(SMALL_BUF))
            .serve_target(
                &file,
                Target::Entry(&entry),
                accepting(NOTHING_ACCEPTED),
                &InflaterPool::default(),
            )
            .await
            .unwrap();
        entry
    });
    let (head, body) = split_response(&writer.written);

    assert!(
        head.contains(&format!("Content-Length: {}", entry.uncompressed_size)),
        "{head}"
    );
    assert!(writer.writes > 1);
    assert_entry_contents(&mut &body[..], &entry);
}

/// An entry with no content still owes its header, which is only ever written
/// alongside the first body bytes.
#[test]
fn test_serve_empty_entry_sends_its_header() {
    let mut writer = TestWriter::new();
    run_with_fixture(
        &mut writer,
        4096,
        "images/",
        accepting(NOTHING_ACCEPTED),
        |r| r,
    );
    let (head, body) = split_response(&writer.written);

    assert!(head.contains("Content-Length: 0"), "{head}");
    assert!(body.is_empty());
}

#[test]
fn test_serve_target_with_matching_etag_is_not_modified() {
    let mut writer = TestWriter::new();
    let headers = Headers {
        if_none_match: Some(0xDEAD_BEEF),
        ..accepting(GZIP_ACCEPTED)
    };
    run_with_fixture(&mut writer, 4096, "index.html", headers, |mut entry| {
        entry.crc32 = 0xDEAD_BEEF;
        entry
    });
    let (head, body) = split_response(&writer.written);

    assert!(head.starts_with("HTTP/1.1 304 Not Modified"), "{head}");
    assert!(head.contains("ETag: \"deadbeef\""), "{head}");
    assert!(body.is_empty());
}

#[test]
fn test_serve_target_with_stale_etag_is_served_whole() {
    let mut writer = TestWriter::new();
    let headers = Headers {
        if_none_match: Some(0xDEAD_BEEF),
        ..accepting(GZIP_ACCEPTED)
    };
    let entry = run_with_fixture(&mut writer, 4096, "metadata.opf", headers, |mut entry| {
        entry.crc32 = 0xFEED_FACE;
        entry
    });
    let (head, body) = split_response(&writer.written);

    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(head.contains("ETag: \"feedface\""), "{head}");
    assert_eq!(body.len() as u64, entry.compressed_size + 18);
}

#[test]
fn test_serve_target_redirects_into_the_dir() {
    let mut writer = TestWriter::new();
    run(async {
        let file = compio::fs::File::open(FIXTURE).await.unwrap();
        ResponseStream::new(&mut writer, make_buf(1024))
            .serve_target(
                &file,
                Target::Redirect { name: "a <b>" },
                Headers::default(),
                &InflaterPool::default(),
            )
            .await
            .unwrap();
    });
    let out = String::from_utf8_lossy(&writer.written);
    assert_eq!(
        out.split("\r\n").collect::<Vec<_>>(),
        vec![
            "HTTP/1.1 301 Moved Permanently",
            "Location: ./a%20%3Cb%3E/",
            "Content-Length: 0",
            "",
            ""
        ]
    );
}

/// Drains `body` through a buffer of `buflen` bytes, returning the size of each
/// portion and the bytes of all of them.
async fn drain(
    mut body: EntryBody<'_>,
    file: &File,
    buflen: usize,
) -> std::io::Result<(Vec<usize>, Vec<u8>)> {
    let mut buf = make_buf(buflen);
    let (mut portions, mut bytes) = (Vec::new(), Vec::new());
    loop {
        let BufResult(res, returned) = body.fill(file, buf, 0).await;
        buf = returned;
        match res? {
            0 => return Ok((portions, bytes)),
            n => {
                portions.push(n);
                bytes.extend_from_slice(&buf[..n]);
                assert!(bytes.len() < 1 << 24, "the body should end");
            }
        }
    }
}

/// Opens the fixture and hands `test` the named entry along with the file.
fn with_fixture_entry<T>(name: &str, test: impl AsyncFnOnce(&File, Entry) -> T) -> T {
    run(async {
        let file = File::open(FIXTURE).await.unwrap();
        let archive = crate::rc_zip_compio::read_zip_from_file(&file)
            .await
            .unwrap();
        test(&file, fixture_entry(&archive, name)).await
    })
}

#[test]
fn test_entry_body_gzip_in_small_portions() {
    let (entry, portions, bytes) = with_fixture_entry("index.html", async |file, entry| {
        let inflaters = InflaterPool::default();
        let body = EntryBody::new(&entry, Some(NativeCoding::Gzip), &inflaters);
        let (portions, bytes) = drain(body, file, SMALL_BUF).await.unwrap();
        (entry, portions, bytes)
    });
    assert!(portions.len() > 1, "{portions:?}");
    assert!(portions.iter().all(|&n| n <= SMALL_BUF), "{portions:?}");
    assert_eq!(
        bytes.len() as u64,
        NativeCoding::Gzip.content_length(&entry)
    );
    assert_entry_contents(&mut flate2::read::GzDecoder::new(&bytes[..]), &entry);
}

#[cfg(feature = "deflate")]
#[test]
fn test_entry_body_decoded_in_small_portions() {
    let (entry, portions, bytes) = with_fixture_entry("index.html", async |file, entry| {
        let inflaters = InflaterPool::default();
        let body = EntryBody::new(&entry, None, &inflaters);
        let (portions, bytes) = drain(body, file, SMALL_BUF).await.unwrap();
        (entry, portions, bytes)
    });
    assert!(portions.len() > 1, "{portions:?}");
    assert_entry_contents(&mut &bytes[..], &entry);
}

/// With room to spare, the whole gzip member leaves as one portion.
#[test]
fn test_entry_body_trailer_rides_with_the_last_data() {
    let (entry, portions) = with_fixture_entry("index.html", async |file, entry| {
        let whole = usize::try_from(NativeCoding::Gzip.content_length(&entry)).unwrap();
        let inflaters = InflaterPool::default();
        let body = EntryBody::new(&entry, Some(NativeCoding::Gzip), &inflaters);
        let (portions, _) = drain(body, file, whole).await.unwrap();
        (entry, portions)
    });
    assert_eq!(
        portions,
        [usize::try_from(NativeCoding::Gzip.content_length(&entry)).unwrap()]
    );
}

/// Data that ends too close to the end of the buffer leaves the trailer for a
/// portion of its own, rather than being cut short to make room for it.
#[test]
fn test_entry_body_trailer_spills_when_the_data_fills_the_buffer() {
    let (entry, portions, bytes) = with_fixture_entry("index.html", async |file, entry| {
        let whole = usize::try_from(NativeCoding::Gzip.content_length(&entry)).unwrap();
        let inflaters = InflaterPool::default();
        let body = EntryBody::new(&entry, Some(NativeCoding::Gzip), &inflaters);
        let (portions, bytes) = drain(body, file, whole - 4).await.unwrap();
        (entry, portions, bytes)
    });
    let trailer_len = NativeCoding::Gzip.trailer_len();
    let whole = usize::try_from(NativeCoding::Gzip.content_length(&entry)).unwrap();
    assert_eq!(portions, [whole - trailer_len, trailer_len]);
    assert_entry_contents(&mut flate2::read::GzDecoder::new(&bytes[..]), &entry);
}

/// What the caller staged in front of `at` is left alone.
#[test]
fn test_entry_body_fills_behind_what_is_staged() {
    const STAGED: usize = 100;
    let buf = with_fixture_entry("index.html", async |file, entry| {
        let mut buf = make_buf(SMALL_BUF);
        buf[..STAGED].fill(b'H');
        let inflaters = InflaterPool::default();
        let mut body = EntryBody::new(&entry, Some(NativeCoding::Gzip), &inflaters);
        let BufResult(res, buf) = body.fill(file, buf, STAGED).await;
        assert_eq!(res.unwrap(), SMALL_BUF - STAGED);
        buf
    });
    assert!(buf[..STAGED].iter().all(|&b| b == b'H'));
    assert_eq!(buf[STAGED..STAGED + 2], [0x1f, 0x8b], "gzip magic");
}

#[cfg(feature = "deflate")]
#[test]
fn test_entry_body_of_an_empty_entry_is_empty_when_decoded() {
    let portions = with_fixture_entry("images/", async |file, entry| {
        let inflaters = InflaterPool::default();
        let body = EntryBody::new(&entry, None, &inflaters);
        drain(body, file, SMALL_BUF).await.unwrap().0
    });
    assert!(portions.is_empty(), "{portions:?}");
}

/// Natively, even an empty entry has its framing: an empty deflate stream,
/// wrapped as a gzip member.
#[test]
fn test_entry_body_of_an_empty_entry_is_its_framing_when_native() {
    let (entry, bytes) = with_fixture_entry("images/", async |file, entry| {
        let inflaters = InflaterPool::default();
        let body = EntryBody::new(&entry, Some(NativeCoding::Gzip), &inflaters);
        let (_, bytes) = drain(body, file, SMALL_BUF).await.unwrap();
        (entry, bytes)
    });
    assert_eq!(
        bytes.len() as u64,
        NativeCoding::Gzip.content_length(&entry)
    );
    assert_entry_contents(&mut flate2::read::GzDecoder::new(&bytes[..]), &entry);
}

/// A compressed entry whose stream runs past the end of the archive is refused
/// rather than served short.
#[test]
fn test_entry_body_reaching_past_the_archive_fails() {
    let err = with_fixture_entry("index.html", async |file, mut entry| {
        entry.compressed_size = 1 << 20;
        let inflaters = InflaterPool::default();
        let body = EntryBody::new(&entry, Some(NativeCoding::Gzip), &inflaters);
        drain(body, file, SMALL_BUF)
            .await
            .expect_err("should refuse to read past the archive's end")
    });
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[cfg(feature = "deflate")]
#[test]
fn test_entry_body_inflated_against_the_wrong_crc_fails() {
    let err = with_fixture_entry("index.html", async |file, mut entry| {
        entry.crc32 ^= 1;
        let inflaters = InflaterPool::default();
        let body = EntryBody::new(&entry, None, &inflaters);
        drain(body, file, SMALL_BUF)
            .await
            .expect_err("should refuse data that fails its checksum")
    });
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

/// Bytes past the recorded size would overrun the announced Content-Length,
/// so none of them may leave.
#[cfg(feature = "deflate")]
#[test]
fn test_entry_body_inflating_past_the_recorded_size_fails() {
    const RECORDED: u64 = 1000;
    let (portions, err) = with_fixture_entry("index.html", async |file, mut entry| {
        entry.uncompressed_size = RECORDED;
        let inflaters = InflaterPool::default();
        let mut body = EntryBody::new(&entry, None, &inflaters);
        let mut buf = make_buf(SMALL_BUF);
        let mut portions = Vec::new();
        loop {
            let BufResult(res, returned) = body.fill(file, buf, 0).await;
            buf = returned;
            match res {
                Ok(0) => panic!("the oversized stream should be refused"),
                Ok(n) => portions.push(n),
                Err(err) => return (portions, err),
            }
        }
    });
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        portions.iter().sum::<usize>() as u64 <= RECORDED,
        "{portions:?}"
    );
}

/// A stream ending short of the recorded size would leave the client waiting
/// on the announced Content-Length.
#[cfg(feature = "deflate")]
#[test]
fn test_entry_body_inflating_short_of_the_recorded_size_fails() {
    let err = with_fixture_entry("index.html", async |file, mut entry| {
        entry.uncompressed_size += 1;
        let inflaters = InflaterPool::default();
        let body = EntryBody::new(&entry, None, &inflaters);
        drain(body, file, SMALL_BUF)
            .await
            .expect_err("should refuse a stream short of its recorded size")
    });
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

/// Input stops at the recorded compressed size, even mid-stream.
#[cfg(feature = "deflate")]
#[test]
fn test_entry_body_inflating_a_truncated_stream_fails() {
    with_fixture_entry("index.html", async |file, mut entry| {
        entry.compressed_size /= 2;
        let inflaters = InflaterPool::default();
        let body = EntryBody::new(&entry, None, &inflaters);
        drain(body, file, SMALL_BUF)
            .await
            .expect_err("should refuse a stream cut short");
    });
}

/// A body dropped halfway leaves its inflater fit for the next body.
#[cfg(feature = "deflate")]
#[test]
fn test_entry_body_inflates_after_one_was_dropped_early() {
    let (entry, bytes) = with_fixture_entry("index.html", async |file, entry| {
        let inflaters = InflaterPool::default();
        let mut abandoned = EntryBody::new(&entry, None, &inflaters);
        let BufResult(res, _) = abandoned.fill(file, make_buf(SMALL_BUF), 0).await;
        assert_eq!(
            res.unwrap(),
            SMALL_BUF,
            "the entry should be partly decoded"
        );
        drop(abandoned);
        assert_eq!(
            inflaters.idle_len(),
            1,
            "the inflater should go back to its pool"
        );

        let body = EntryBody::new(&entry, None, &inflaters);
        let (_, bytes) = drain(body, file, SMALL_BUF).await.unwrap();
        (entry, bytes)
    });
    assert_entry_contents(&mut &bytes[..], &entry);
}

#[test]
fn test_zstd_entry_is_passed_through_when_accepted() {
    let mut writer = TestWriter::new();
    let entry = run_with_fixture(
        &mut writer,
        4096,
        "index.html",
        accepting(ZSTD_ACCEPTED),
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
        // However this ends -- 500 without a zstd decoder, a decode attempt with one --
        // the client must not be told the body is zstd.
        let _ = ResponseStream::new(&mut writer, make_buf(4096))
            .serve_target(
                &file,
                Target::Entry(&entry),
                accepting(NOTHING_ACCEPTED),
                &InflaterPool::default(),
            )
            .await;
    });
    let head = String::from_utf8_lossy(&writer.written);
    assert!(!head.contains("zstd"), "{head}");
}

/// Each href must be read by browsers as one path segment inside a quoted
/// attribute, and must lead back to its name once the server decodes the path.
#[test]
fn test_serve_index_encodes_names_as_path_segments() {
    let names = [
        "a#b", "why?", "%41", "x&amp;y", "c\\d", "{e}", "sp ace", "写真",
    ];
    let entries: Vec<_> = names.iter().map(|name| node_file(name)).collect();

    let mut writer = TestWriter::new();
    run(async {
        ResponseStream::new(&mut writer, make_buf(4096))
            .serve_index(true, &entries)
            .await
            .unwrap();
    });
    let (_head, framed) = split_response(&writer.written);
    let body = String::from_utf8(decode_chunked(&framed)).unwrap();

    let hrefs: Vec<&str> = body
        .split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .collect();
    assert_eq!(hrefs.len(), names.len(), "{body}");
    for (href, name) in hrefs.into_iter().zip(names) {
        let segment = href.strip_prefix("./").expect("href should be relative");
        assert!(
            !segment.contains(['#', '?', '&', '\\', '/', ' ', '{', '}']),
            "{name:?} -> {href}"
        );
        assert_eq!(
            percent_encoding::percent_decode_str(segment).decode_utf8_lossy(),
            name,
            "{href}"
        );
    }
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

/// However full a chunk gets, its CRLF and the terminator must still fit.
/// Directory lines are 11 bytes longer than file lines, so that both parities occur.
#[test]
fn test_serve_index_fills_the_last_chunk_to_every_length() {
    for name_len in 1..470 {
        let name = "x".repeat(name_len);
        for entry in [node_file(&name), node_dir(&name)] {
            let entries = [entry];
            let mut writer = TestWriter::new();
            run(async {
                ResponseStream::new(&mut writer, make_buf(1024))
                    .serve_index(true, &entries)
                    .await
                    .unwrap();
            });
            let (_head, framed) = split_response(&writer.written);
            let body = String::from_utf8(decode_chunked(&framed)).unwrap();
            assert!(body.contains(&name), "{name_len}");
        }
    }
}

/// A name that no chunk can hold ends the response rather than looping.
#[test]
fn test_serve_index_refuses_a_name_too_long_for_any_chunk() {
    let entries = vec![node_file(&"x".repeat(2048))];
    let mut writer = TestWriter::new();
    let res = run(async {
        ResponseStream::new(&mut writer, make_buf(1024))
            .serve_index(true, &entries)
            .await
    });
    assert!(res.0.is_err());
}

/// Serve one fixture entry, letting `adjust` alter it first, and hand back the entry
/// as it was served.
fn run_with_fixture(
    writer: &mut TestWriter,
    buflen: usize,
    name: &str,
    headers: Headers,
    adjust: impl FnOnce(Entry) -> Entry,
) -> Entry {
    run(async {
        let file = compio::fs::File::open(FIXTURE).await.unwrap();
        let archive = crate::rc_zip_compio::read_zip_from_file(&file)
            .await
            .unwrap();
        let entry = adjust(fixture_entry(&archive, name));
        ResponseStream::new(writer, make_buf(buflen))
            .serve_target(
                &file,
                Target::Entry(&entry),
                headers,
                &InflaterPool::default(),
            )
            .await
            .unwrap();
        entry
    })
}

fn accepting(accepted_encodings: AcceptedEncodings) -> Headers {
    Headers {
        accepted_encodings,
        ..Headers::default()
    }
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
