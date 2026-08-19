use monoio::buf::{IoBufMut, IoVecBufMut};
use monoio::io::AsyncReadRent;
use monoio::{BufResult, IoUringDriver};

use super::*;
use crate::response::status::HttpStatus;
use std::assert_matches;

/// A test reader that yields a fixed byte sequence.
struct TestReader {
    data: Vec<u8>,
    pos: usize,
}

impl TestReader {
    fn new(data: Vec<u8>) -> Self {
        Self { data, pos: 0 }
    }
}

impl AsyncReadRent for TestReader {
    async fn read<T: IoBufMut>(&mut self, mut buf: T) -> BufResult<usize, T> {
        let remaining = self.data.len() - self.pos;
        let amt = std::cmp::min(remaining, buf.bytes_total());
        unsafe {
            buf.write_ptr()
                .copy_from_nonoverlapping(self.data.as_ptr().add(self.pos), amt);
            buf.set_init(amt);
        }
        self.pos += amt;
        (Ok(amt), buf)
    }

    async fn readv<T: IoVecBufMut>(&mut self, _buf: T) -> BufResult<usize, T> {
        unimplemented!()
    }
}

struct ErrorReader;

impl AsyncReadRent for ErrorReader {
    async fn read<T: IoBufMut>(&mut self, buf: T) -> BufResult<usize, T> {
        (Err(std::io::ErrorKind::ConnectionReset.into()), buf)
    }

    async fn readv<T: IoVecBufMut>(&mut self, buf: T) -> BufResult<usize, T> {
        (Err(std::io::ErrorKind::ConnectionReset.into()), buf)
    }
}

fn make_buf(size: usize) -> Buf {
    vec![0u8; size].into_boxed_slice()
}

fn run(future: impl Future) {
    monoio::RuntimeBuilder::<IoUringDriver>::new()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
}

#[test]
fn test_parse_simple_get() {
    run(async {
        let data = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec();
        let mut reader = TestReader::new(data);
        let result = parse_next_request(&mut reader, make_buf(1024))
            .await
            .unwrap();
        assert_matches!(
            result,
            Request::Get {
                path,
                headers: Headers {
                    if_none_match: None,
                    accepted_encodings: AcceptedEncodings {
                        gzip: false,
                        zstd: false
                    },
                    close: false
                },
            } if &*path == b"/"
        );
    });
}

#[test]
fn test_parse_get_with_etag() {
    run(async {
        let data = b"GET /style.css HTTP/1.1\r\nIf-None-Match: \"deadbeef\"\r\n\r\n".to_vec();
        let mut reader = TestReader::new(data);
        let result = parse_next_request(&mut reader, make_buf(1024))
            .await
            .unwrap();
        assert_matches!(
            result,
            Request::Get {
                headers: Headers {
                    if_none_match: Some(0xdeadbeef),
                    ..
                },
                ..
            }
        );
    });
}

#[test]
fn test_parse_get_with_invalid_etag() {
    run(async {
        // Value is 10 bytes but hex part is not valid hex
        let data = b"GET / HTTP/1.1\r\nIf-None-Match: \"zzzzzzzz\"\r\n\r\n".to_vec();
        let mut reader = TestReader::new(data);
        let result = parse_next_request(&mut reader, make_buf(1024))
            .await
            .unwrap();
        assert_matches!(
            result,
            Request::Get {
                headers: Headers {
                    if_none_match: None,
                    ..
                },
                ..
            }
        );
    });
}

#[test]
fn test_parse_get_with_accept_encoding() {
    run(async {
        let data = b"GET / HTTP/1.1\r\nAccept-Encoding: gzip, zstd\r\n\r\n".to_vec();
        let mut reader = TestReader::new(data);
        let result = parse_next_request(&mut reader, make_buf(1024))
            .await
            .unwrap();
        assert_matches!(
            result,
            Request::Get {
                headers: Headers {
                    accepted_encodings: AcceptedEncodings {
                        gzip: true,
                        zstd: true,
                    },
                    ..
                },
                ..
            }
        );
    });
}

#[test]
fn test_parse_get_with_connection_close() {
    run(async {
        let data = b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n".to_vec();
        let mut reader = TestReader::new(data);
        let result = parse_next_request(&mut reader, make_buf(1024))
            .await
            .unwrap();
        assert_matches!(
            result,
            Request::Get {
                headers: Headers { close: true, .. },
                ..
            }
        );
    });
}

#[test]
fn test_parse_post_not_allowed() {
    run(async {
        let data = b"POST / HTTP/1.1\r\nContent-Length: 0\r\n\r\n".to_vec();
        let mut reader = TestReader::new(data);
        let result = parse_next_request(&mut reader, make_buf(1024))
            .await
            .unwrap();
        assert_matches!(
            result,
            Request::Bad {
                status: HttpStatus::MethodNotAllowed,
                ..
            }
        );
    });
}

#[test]
fn test_parse_read_error() {
    run(async {
        let result = parse_next_request(&mut ErrorReader, make_buf(1024)).await;
        assert!(result.is_err(), "read errors should return Err(buf)");
    });
}

#[test]
fn test_parse_empty_read() {
    run(async {
        let data = Vec::new();
        let mut reader = TestReader::new(data);
        let result = parse_next_request(&mut reader, make_buf(1024)).await;
        assert!(result.is_err(), "empty read should return Err(buf)");
    });
}

#[test]
fn test_parse_partial_returns_bad_request() {
    run(async {
        // A complete request line with an incomplete first header
        let data = b"GET / HTTP/1.1\r\nX-".to_vec();
        let mut reader = TestReader::new(data.clone());
        let result = parse_next_request(&mut reader, make_buf(1024))
            .await
            .unwrap();
        assert_matches!(
            result,
            Request::Bad {
                status: HttpStatus::BadRequest,
                ..
            }
        );
    });
}
#[test]
fn test_parse_partial_with_path_returns_bad_request() {
    run(async {
        // A complete request line with no headers and no second \r\n
        let data = b"GET / HTTP/1.1\r\n".to_vec();
        let mut reader = TestReader::new(data.clone());
        let result = parse_next_request(&mut reader, make_buf(1024))
            .await
            .unwrap();
        assert_matches!(
            result,
            Request::Bad {
                status: HttpStatus::BadRequest,
                ..
            }
        );
    });
}

#[test]
fn test_decode_path() {
    run(async {
        let path = format!("/a+file%20path/..%2F");
        let request_line = format!("GET {} HTTP/1.1\r\n\r\n", path);
        let mut reader = TestReader::new(request_line.as_bytes().to_vec());
        let result = parse_next_request(&mut reader, make_buf(63)).await.unwrap();
        assert_matches!(
            result,
            Request::Get {
                path,
                headers: _,
            } if matches!(str::from_utf8(&path[..]), Ok("/a+file path/../"))
        );
    });
}

#[test]
fn test_parse_path_too_long_for_buffer() {
    run(async {
        // The decoded path is written into the tail of the buffer after the
        // raw path. If decoding consumes every remaining byte the request is
        // rejected as UriTooLong.  A 30-byte non-encoded path in a 63-byte
        // buffer leaves exactly 30 bytes of tail space — enough to trigger.
        let path = format!("/{}", "a".repeat(29)); // 30 bytes
        let request_line = format!("GET {} HTTP/1.1\r\n\r\n", path);
        let mut reader = TestReader::new(request_line.as_bytes().to_vec());
        let result = parse_next_request(&mut reader, make_buf(64)).await.unwrap();
        assert_matches!(
            result,
            Request::Bad {
                status: HttpStatus::UriTooLong,
                ..
            }
        );
    });
}

#[test]
fn test_parse_too_many_headers() {
    run(async {
        // 65 header lines exceeds the 64-header capacity
        let mut raw = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..65 {
            raw.extend_from_slice(format!("X-Dummy: {}\r\n", i).as_bytes());
        }
        raw.extend_from_slice(b"\r\n");
        let mut reader = TestReader::new(raw);
        let result = parse_next_request(&mut reader, make_buf(2048))
            .await
            .unwrap();
        assert_matches!(
            result,
            Request::Bad {
                status: HttpStatus::HeaderTooLong,
                ..
            }
        );
    });
}
