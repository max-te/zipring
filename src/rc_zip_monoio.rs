//! This file is based on <https://github.com/bearcove/rc-zip/pull/92> by @fasterthanlime,
//! vendored since they have no intentions of maintaining it.
//!
//! A library for reading zip files asynchronously using monoio I/O traits,
//! based on top of [rc-zip](https://crates.io/crates/rc-zip).
//!
//! See also:
//!
//!   * [rc-zip-sync](https://crates.io/crates/rc-zip-sync) for using std I/O traits
//!   * [rc-zip-tokio](https://crates.io/crates/rc-zip-tokio) for using tokio traits

use crate::borrowed_file::BorrowedFile;
use compio::BufResult;
use compio::buf::{IntoInner, IoBuf, IoBufMut};
use compio::fs::File;
use compio::io::AsyncReadAt;
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
    file: &BorrowedFile<'_>,
    entry: &Entry,
    buf: B,
) -> Result<(u64, B), Error> {
    let mut buf = buf;
    let offset = entry.header_offset;
    // https://en.wikipedia.org/wiki/ZIP_(file_format)#Local_file_header
    let mut cursor = 0;
    while cursor < 30 {
        let BufResult(res, slice) = file.read_at(buf.slice(cursor..30), offset).await;
        buf = slice.into_inner();
        let n = res?;
        if n == 0 {
            return Err(Error::IO(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "file ends within local header",
            )));
        }
        cursor += n;
    }
    let header = buf.as_init();

    // name_len and extra_len fields are at position 26 and 28 of the header
    let name_len = u16::from_le_bytes([header[26], header[27]]);
    let extra_len = u16::from_le_bytes([header[28], header[29]]);
    tracing::debug!(name: "find_entry_compressed_data", ?name_len, ?extra_len);

    Ok((
        offset + 30 + u64::from(name_len) + u64::from(extra_len),
        buf,
    ))
}
