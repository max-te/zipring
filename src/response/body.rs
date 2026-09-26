use compio::BufResult;
use compio::buf::{IntoInner, IoBuf};
use compio::fs::File;
use compio::io::{AsyncReadAt, AsyncReadAtExt};
use rc_zip::{
    Entry,
    fsm::{EntryFsm, FsmResult},
};

use crate::{
    Buf,
    buf_result::{bail_traced, buf_try_traced},
    rc_zip_compio::find_entry_compressed_data,
    response::coding::NativeCoding,
};

/// The bytes of an entry as they go on the wire, in its native coding or decoded,
/// handed out in whatever portions the caller has room for.
///
/// Once a fill has failed, the body is spent.
#[expect(
    clippy::large_enum_variant,
    reason = "one body per response; boxing the decoder would allocate per decoded response to save stack"
)]
pub(super) enum EntryBody<'e> {
    Native(NativeBody<'e>),
    Decoded(DecodedBody),
}

impl<'e> EntryBody<'e> {
    /// Without a `native` coding, `entry` is decoded.
    pub fn new(entry: &'e Entry, native: Option<NativeCoding>) -> Self {
        match native {
            Some(native) => Self::Native(NativeBody {
                entry,
                native,
                data_offset: None,
                remaining: usize::try_from(entry.compressed_size)
                    .expect("entry size should fit into usize"),
                prefix_pending: true,
                trailer_pending: true,
            }),
            None => Self::Decoded(DecodedBody {
                fsm: Some(EntryFsm::new(None, None)),
                offset: entry.header_offset,
            }),
        }
    }

    /// Fills `buf[at..]` with the next bytes of the body and returns how many.
    /// Yields 0 only once the body is complete.
    pub async fn fill(&mut self, file: &File, buf: Buf, at: usize) -> BufResult<usize, Buf> {
        match self {
            Self::Native(body) => body.fill(file, buf, at).await,
            Self::Decoded(body) => body.fill(file, buf, at).await,
        }
    }
}

pub(super) struct NativeBody<'e> {
    entry: &'e Entry,
    native: NativeCoding,
    /// Where the entry's data lies in the archive, once its local header was read.
    data_offset: Option<u64>,
    remaining: usize,
    prefix_pending: bool,
    trailer_pending: bool,
}

impl NativeBody<'_> {
    async fn fill(&mut self, file: &File, mut buf: Buf, at: usize) -> BufResult<usize, Buf> {
        let mut end = at;
        if self.prefix_pending {
            let prefix = self.native.prefix();
            buf[end..end + prefix.len()].copy_from_slice(prefix);
            end += prefix.len();
            self.prefix_pending = false;
        }

        if self.remaining > 0 {
            let offset = if let Some(offset) = self.data_offset {
                offset
            } else {
                // The local header is read into the room the data will take.
                let BufResult(res, scratch) =
                    find_entry_compressed_data(file, self.entry, buf.slice(end..)).await;
                buf = scratch.into_inner();
                let offset;
                (offset, buf) = buf_try_traced!(res, buf);
                offset
            };
            let n = self.remaining.min(buf.len() - end);
            let BufResult(res, slice) = file.read_exact_at(buf.slice(end..end + n), offset).await;
            buf = slice.into_inner();
            ((), buf) = buf_try_traced!(res, buf);
            self.data_offset = Some(offset + n as u64);
            self.remaining -= n;
            end += n;
        }

        let trailer_len = self.native.trailer_len();
        if self.remaining == 0 && self.trailer_pending && buf.len() - end >= trailer_len {
            self.native
                .write_trailer(self.entry, &mut buf[end..end + trailer_len]);
            end += trailer_len;
            self.trailer_pending = false;
        }
        BufResult(Ok(end - at), buf)
    }
}

pub(super) struct DecodedBody {
    /// Gone once decoding is complete.
    fsm: Option<EntryFsm>,
    offset: u64,
}

impl DecodedBody {
    async fn fill(&mut self, file: &File, mut buf: Buf, at: usize) -> BufResult<usize, Buf> {
        let Some(mut fsm) = self.fsm.take() else {
            return BufResult(Ok(0), buf);
        };
        // The decoder may take several reads before it has anything to show.
        loop {
            if fsm.wants_read() {
                let room = fsm.space().len().min(buf.len() - at);
                let BufResult(res, slice) =
                    file.read_at(buf.slice(at..at + room), self.offset).await;
                buf = slice.into_inner();
                let bytes_read;
                (bytes_read, buf) = buf_try_traced!(res, buf);
                fsm.space()[..bytes_read].copy_from_slice(&buf[at..at + bytes_read]);
                fsm.fill(bytes_read);
                self.offset += bytes_read as u64;
            }
            let outcome;
            (fsm, outcome) = match fsm.process(&mut buf[at..]) {
                Ok(FsmResult::Continue(continued)) => continued,
                Ok(FsmResult::Done(_buffer)) => return BufResult(Ok(0), buf),
                Err(err) => bail_traced!(std::io::Error::other(err), buf),
            };
            if outcome.bytes_written > 0 {
                self.fsm = Some(fsm);
                return BufResult(Ok(outcome.bytes_written), buf);
            }
        }
    }
}
