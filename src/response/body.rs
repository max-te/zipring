#[cfg(feature = "deflate")]
use std::{io::ErrorKind, ops::Range};

use compio::BufResult;
use compio::buf::{IntoInner, IoBuf};
use compio::fs::File;
use compio::io::{AsyncReadAt, AsyncReadAtExt};
#[cfg(feature = "deflate")]
use rc_zip::parse::Method;
use rc_zip::{
    Entry,
    fsm::{EntryFsm, FsmResult},
};
#[cfg(feature = "deflate")]
use sirocco::{Format, inflate::Decoder};

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
pub(super) enum EntryBody<'e> {
    Native(NativeBody<'e>),
    #[cfg(feature = "deflate")]
    Inflated(InflatedBody<'e>),
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
            #[cfg(feature = "deflate")]
            None if entry.method == Method::Deflate => Self::Inflated(InflatedBody::new(entry)),
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
            #[cfg(feature = "deflate")]
            Self::Inflated(body) => body.fill(file, buf, at).await,
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

/// Decoder history; DEFLATE copies from at most 32 KiB back.
#[cfg(feature = "deflate")]
const WINDOW_LEN: usize = 64 * 1024;
#[cfg(feature = "deflate")]
const INPUT_LEN: usize = 16 * 1024;

/// A deflated entry, inflated and checked against its recorded size and CRC.
#[cfg(feature = "deflate")]
pub(super) struct InflatedBody<'e> {
    entry: &'e Entry,
    /// Boxed, because inline it would double every connection's future.
    decoder: Box<Decoder>,
    window: Box<[u8]>,
    /// How much of `window` the decoder's history takes up.
    history_len: usize,
    /// Decoded bytes in `window` that were not handed out yet.
    pending: Range<usize>,
    /// The decoder needs room in `window` before it can go on.
    window_full: bool,
    input: Box<[u8]>,
    /// Compressed bytes in `input` that were not decoded yet.
    unconsumed: Range<usize>,
    /// Where the unread compressed data lies, once the local header was read.
    data_offset: Option<u64>,
    unread: usize,
    crc: crc32fast::Hasher,
    done: bool,
}

#[cfg(feature = "deflate")]
impl<'e> InflatedBody<'e> {
    fn new(entry: &'e Entry) -> Self {
        Self {
            entry,
            decoder: Box::new(Decoder::new(Format::Deflate)),
            window: vec![0; WINDOW_LEN].into_boxed_slice(),
            history_len: 0,
            pending: 0..0,
            window_full: false,
            input: vec![0; INPUT_LEN].into_boxed_slice(),
            unconsumed: 0..0,
            data_offset: None,
            unread: usize::try_from(entry.compressed_size)
                .expect("entry size should fit into usize"),
            crc: crc32fast::Hasher::new(),
            done: false,
        }
    }

    async fn fill(&mut self, file: &File, mut buf: Buf, at: usize) -> BufResult<usize, Buf> {
        while self.pending.is_empty() && !self.done {
            if self.unconsumed.is_empty() && self.unread > 0 {
                ((), buf) = buf_try_traced!(self.read_input(file, buf, at).await);
            }
            ((), buf) = buf_try_traced!(self.decode(), buf);
        }
        let n = self.pending.len().min(buf.len() - at);
        buf[at..at + n].copy_from_slice(&self.window[self.pending.start..][..n]);
        self.pending.start += n;
        BufResult(Ok(n), buf)
    }

    /// Reads the next compressed bytes into `input`, by way of `buf[at..]`.
    async fn read_input(&mut self, file: &File, mut buf: Buf, at: usize) -> BufResult<(), Buf> {
        let offset = if let Some(offset) = self.data_offset {
            offset
        } else {
            let BufResult(res, scratch) =
                find_entry_compressed_data(file, self.entry, buf.slice(at..)).await;
            buf = scratch.into_inner();
            let offset;
            (offset, buf) = buf_try_traced!(res, buf);
            offset
        };
        let n = self.unread.min(INPUT_LEN).min(buf.len() - at);
        let BufResult(res, slice) = file.read_exact_at(buf.slice(at..at + n), offset).await;
        buf = slice.into_inner();
        ((), buf) = buf_try_traced!(res, buf);
        self.input[..n].copy_from_slice(&buf[at..at + n]);
        self.unconsumed = 0..n;
        self.data_offset = Some(offset + n as u64);
        self.unread -= n;
        BufResult(Ok(()), buf)
    }

    /// Decodes from `input` into `window`, leaving the new bytes pending.
    fn decode(&mut self) -> std::io::Result<()> {
        if self.window_full {
            // Only once pending bytes are out, because this moves the window.
            self.history_len -= self.decoder.make_room(&mut self.window);
            self.window_full = false;
        }
        let progress = self.decoder.decompress(
            &self.input[self.unconsumed.clone()],
            &mut self.window,
            self.unread == 0,
        )?;
        self.unconsumed.start += progress.consumed;
        self.crc.update(progress.output);
        let start = self.history_len;
        self.history_len += progress.output.len();
        self.pending = start..self.history_len;
        self.window_full = progress.full;
        self.done = progress.done;

        let expected_size = self.entry.uncompressed_size;
        if self.decoder.total_out() > expected_size
            || (self.done && self.decoder.total_out() < expected_size)
        {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("inflated entry is not the {expected_size} bytes recorded"),
            ));
        }
        if self.done && self.crc.clone().finalize() != self.entry.crc32 {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "inflated entry does not match its CRC-32",
            ));
        }
        Ok(())
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
