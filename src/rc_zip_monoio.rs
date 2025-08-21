//! A library for reading zip files asynchronously using monoio I/O traits,
//! based on top of [rc-zip](https://crates.io/crates/rc-zip).
//!
//! See also:
//!
//!   * [rc-zip-sync](https://crates.io/crates/rc-zip-sync) for using std I/O traits
//!   * [rc-zip-tokio](https://crates.io/crates/rc-zip-tokio) for using tokio traits

use monoio::{buf::IoBufMut, fs::File};
use rc_zip::{
    error::Error,
    fsm::{ArchiveFsm, FsmResult},
    parse::{Archive, Entry, LocalFileHeader},
};
use winnow::{Parser, Partial};

pub async fn read_zip_from_file(file: &File) -> Result<Archive, Error> {
    let meta = file.metadata().await?;
    let size = meta.len();
    let mut buf = vec![0u8; 256 * 1024].into_boxed_slice();

    let mut fsm = ArchiveFsm::new(size);
    loop {
        if let Some(offset) = fsm.wants_read() {
            let dst = fsm.space();
            let max_read = dst.len().min(buf.len());
            let slice = IoBufMut::slice_mut(buf, 0..max_read);

            let (res, slice) = file.read_at(slice, offset).await;
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

pub async fn find_entry_compressed_data(
    file: &File,
    entry: &Entry,
    buf: Option<Box<[u8]>>,
) -> Result<(u64, Box<[u8]>), Error> {
    let mut buf = buf.unwrap_or_else(|| vec![0u8; 1024].into_boxed_slice());
    let offset = entry.header_offset;
    let (res, slice) = file
        .read_at(IoBufMut::slice_mut(buf, 0..1024), offset)
        .await;
    buf = slice.into_inner();
    let _n = res?;

    let mut i = Partial::new(buf.as_ref());
    let header = LocalFileHeader::parser.parse_next(&mut i).unwrap();
    tracing::debug!(name: "find_entry_compressed_data", ?header);

    Ok((
        offset + 30 + header.name.len() as u64 + header.extra.len() as u64,
        buf,
    ))
}
