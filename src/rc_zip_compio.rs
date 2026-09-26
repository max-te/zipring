//! This file is based on <https://github.com/bearcove/rc-zip/pull/92> by @fasterthanlime,
//! vendored since they have no intentions of maintaining it.
//!
//! A library for reading zip files asynchronously using compio I/O traits,
//! based on top of [rc-zip](https://crates.io/crates/rc-zip).
//!
//! See also:
//!
//!   * [rc-zip-sync](https://crates.io/crates/rc-zip-sync) for using std I/O traits
//!   * [rc-zip-tokio](https://crates.io/crates/rc-zip-tokio) for using tokio traits

use compio::BufResult;
use compio::buf::{IntoInner, IoBuf, IoBufMut, buf_try};
use compio::fs::File;
use compio::io::{AsyncReadAt, AsyncReadAtExt};
use rc_zip::parse::Method;
use rc_zip::{
    error::Error,
    fsm::{ArchiveFsm, FsmResult},
    parse::{Archive, Entry},
};

pub const fn is_method_supported(method: Method) -> bool {
    match method {
        Method::Store => true,
        #[cfg(feature = "deflate")]
        Method::Deflate => true,
        #[cfg(feature = "zstd")]
        Method::Zstd => true,
        #[cfg(feature = "deflate64")]
        Method::Deflate64 => true,
        #[cfg(feature = "bzip2")]
        Method::Bzip2 => true,
        #[cfg(feature = "lzma")]
        Method::Lzma => true,
        _ => false,
    }
}

pub async fn read_zip_from_file(file: &File) -> Result<Archive, Error> {
    let meta = file.metadata().await?;
    let size = meta.len();
    let mut buf = vec![0u8; 256 * 1024].into_boxed_slice();

    let mut fsm = ArchiveFsm::new(size);
    loop {
        if let Some(offset) = fsm.wants_read() {
            let dst = fsm.space();
            let max_read = dst.len().min(buf.len());
            let slice = buf.slice(0..max_read);

            let BufResult(res, slice) = file.read_at(slice, offset).await;
            let n = res?;
            (dst[..n]).copy_from_slice(&slice[..n]);

            fsm.fill(n);
            buf = slice.into_inner();
        }

        fsm = match fsm.process()? {
            FsmResult::Done(archive) => {
                break Ok(archive);
            }
            FsmResult::Continue(fsm) => fsm,
        }
    }
}

/// Locates the compressed data of `entry`, storing its local file header in `buf`,
/// as a scratch buffer. It must have at least 30 bytes of space.
/// Returns the offset to the compressed file stream in `file`.
pub async fn find_entry_compressed_data<B: IoBuf + IoBufMut>(
    file: &File,
    entry: &Entry,
    buf: B,
) -> BufResult<u64, B> {
    let offset = entry.header_offset;
    // https://en.wikipedia.org/wiki/ZIP_(file_format)#Local_file_header
    let ((), buf) = buf_try!(
        file.read_exact_at(buf.slice(..30), offset)
            .await
            .into_inner()
    );
    let header = buf.as_init();

    // name_len and extra_len fields are at position 26 and 28 of the header
    let name_len = u16::from_le_bytes([header[26], header[27]]);
    let extra_len = u16::from_le_bytes([header[28], header[29]]);
    tracing::debug!(name: "find_entry_compressed_data", ?name_len, ?extra_len);

    BufResult(
        Ok(offset + 30 + u64::from(name_len) + u64::from(extra_len)),
        buf,
    )
}

#[cfg(test)]
mod test {
    use super::*;

    /// The archive served by the integration tests; every entry is deflated.
    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/resources/Universal_Declaration_of_Human_Rights.htmlz"
    );

    fn run<T>(future: impl Future<Output = T>) -> T {
        compio::runtime::Runtime::new().unwrap().block_on(future)
    }

    #[test]
    fn stored_entries_need_no_decoder() {
        assert!(is_method_supported(Method::Store));
    }

    /// Each optional method is supported exactly when its feature is compiled in.
    #[test]
    fn optional_methods_follow_their_feature() {
        for (method, feature) in [
            (Method::Deflate, cfg!(feature = "deflate")),
            (Method::Zstd, cfg!(feature = "zstd")),
            (Method::Deflate64, cfg!(feature = "deflate64")),
            (Method::Bzip2, cfg!(feature = "bzip2")),
            (Method::Lzma, cfg!(feature = "lzma")),
        ] {
            assert_eq!(
                is_method_supported(method),
                feature,
                "{method:?} should be supported iff its feature is enabled"
            );
        }
    }

    #[test]
    fn methods_without_a_decoder_are_unsupported() {
        for method in [Method::Xz, Method::Mp3, Method::Unrecognized(0xFFFF)] {
            assert!(!is_method_supported(method), "{method:?}");
        }
    }

    #[test]
    fn compressed_data_starts_after_the_local_header() {
        run(async {
            let file = File::open(FIXTURE).await.unwrap();
            let archive = read_zip_from_file(&file).await.unwrap();
            let entry = archive
                .entries()
                .find(|e| e.name == "index.html")
                .expect("fixture should contain index.html");
            assert_ne!(entry.header_offset, 0, "oracle needs a non-zero offset");

            let (offset, _buf) =
                find_entry_compressed_data(&file, entry, vec![0u8; 30].into_boxed_slice())
                    .await
                    .unwrap();

            // The offset is right precisely if the deflate stream found there
            // reproduces the entry's recorded size and checksum.
            let len = usize::try_from(entry.compressed_size).unwrap();
            let BufResult(res, compressed) = file.read_exact_at(vec![0u8; len], offset).await;
            res.unwrap();
            let mut inflated = Vec::new();
            std::io::Read::read_to_end(
                &mut flate2::read::DeflateDecoder::new(&compressed[..]),
                &mut inflated,
            )
            .expect("bytes at offset should be a deflate stream");

            assert_eq!(inflated.len() as u64, entry.uncompressed_size);
            let mut crc = flate2::Crc::new();
            crc.update(&inflated);
            assert_eq!(crc.sum(), entry.crc32);
        });
    }

    /// Local headers in the wild carry extra fields -- timestamps, zip64 sizes --
    /// and the compressed data begins only past them.
    #[test]
    fn the_local_extra_field_is_skipped() {
        const EXTRA: &[u8] = b"\x55\x54\x05\x00\x03\x01\x02\x03\x04";
        const CONTENT: &[u8] = b"the quick brown fox";

        let path = std::env::temp_dir().join(format!("zipring-extra-{}.zip", std::process::id()));
        std::fs::write(&path, stored_zip("padded.txt", EXTRA, CONTENT)).unwrap();

        run(async {
            let file = File::open(&path).await.unwrap();
            let archive = read_zip_from_file(&file).await.unwrap();
            let entry = archive.entries().next().unwrap();

            let (offset, _buf) =
                find_entry_compressed_data(&file, entry, vec![0u8; 30].into_boxed_slice())
                    .await
                    .unwrap();

            let BufResult(res, stored) = file.read_exact_at(vec![0u8; CONTENT.len()], offset).await;
            res.unwrap();
            assert_eq!(stored, CONTENT);
        });

        std::fs::remove_file(&path).unwrap();
    }

    /// Assemble a one-entry archive holding `content` uncompressed, with `extra`
    /// present in both the local and the central header.
    fn stored_zip(name: &str, extra: &[u8], content: &[u8]) -> Vec<u8> {
        let mut crc = flate2::Crc::new();
        crc.update(content);
        let crc = crc.sum().to_le_bytes();
        let size = u32::try_from(content.len()).unwrap().to_le_bytes();
        let name_len = u16::try_from(name.len()).unwrap().to_le_bytes();
        let extra_len = u16::try_from(extra.len()).unwrap().to_le_bytes();

        let mut zip = Vec::new();
        zip.extend(b"PK\x03\x04"); // local file header
        zip.extend([20, 0, 0, 0, 0, 0]); // version needed, flags, method Store
        zip.extend([0; 4]); // modification time and date
        zip.extend(crc);
        zip.extend(size); // compressed
        zip.extend(size); // uncompressed
        zip.extend(name_len);
        zip.extend(extra_len);
        zip.extend(name.as_bytes());
        zip.extend(extra);
        zip.extend(content);

        let central_offset = u32::try_from(zip.len()).unwrap();
        zip.extend(b"PK\x01\x02"); // central directory header
        zip.extend([20, 0, 20, 0, 0, 0, 0, 0]); // versions, flags, method Store
        zip.extend([0; 4]); // modification time and date
        zip.extend(crc);
        zip.extend(size);
        zip.extend(size);
        zip.extend(name_len);
        zip.extend(extra_len);
        zip.extend([0; 2]); // comment length
        zip.extend([0; 2]); // starting disk
        zip.extend([0; 2]); // internal attributes
        zip.extend((0o100_644u32 << 16).to_le_bytes()); // external attributes
        zip.extend([0; 4]); // local header offset
        zip.extend(name.as_bytes());
        zip.extend(extra);

        let central_size = u32::try_from(zip.len()).unwrap() - central_offset;
        zip.extend(b"PK\x05\x06"); // end of central directory
        zip.extend([0; 4]); // this disk, disk holding the central directory
        zip.extend(1u16.to_le_bytes()); // entries on this disk
        zip.extend(1u16.to_le_bytes()); // entries in total
        zip.extend(central_size.to_le_bytes());
        zip.extend(central_offset.to_le_bytes());
        zip.extend([0; 2]); // archive comment length
        zip
    }
}
