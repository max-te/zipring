use rc_zip::{Entry, parse::Method};

use crate::request::AcceptedEncodings;

const GZIP_HEADER: [u8; 10] = [
    0x1f, 0x8b, // Magic number
    0x08, // Compression method = DEFLATE
    0x00, // No flags
    0x00, 0x00, 0x00, 0x00, // No MTIME
    0x00, // No extra flags
    0xFF, // Filesystem unknown
];
const GZIP_TRAILER_LEN: usize = 8;

/// The content coding in which an entry's stored bytes can be sent as they are,
/// framed at most by a fixed prefix and a trailer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NativeCoding {
    /// A raw deflate stream, which gzip framing turns into a gzip member.
    Gzip,
    Zstd,
}

impl NativeCoding {
    pub const fn of(method: Method) -> Option<Self> {
        match method {
            Method::Deflate => Some(Self::Gzip),
            Method::Zstd => Some(Self::Zstd),
            _ => None,
        }
    }

    pub const fn is_accepted_by(self, accepted: AcceptedEncodings) -> bool {
        match self {
            Self::Gzip => accepted.gzip,
            Self::Zstd => accepted.zstd,
        }
    }

    /// The `Content-Encoding` it is announced as.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
        }
    }

    pub const fn prefix(self) -> &'static [u8] {
        match self {
            Self::Gzip => &GZIP_HEADER,
            Self::Zstd => &[],
        }
    }

    pub const fn trailer_len(self) -> usize {
        match self {
            Self::Gzip => GZIP_TRAILER_LEN,
            Self::Zstd => 0,
        }
    }

    /// Writes the trailer owed after `entry`'s data into `dst`, which is
    /// exactly [`Self::trailer_len`] long.
    pub fn write_trailer(self, entry: &Entry, dst: &mut [u8]) {
        match self {
            Self::Gzip => {
                dst[..4].copy_from_slice(&entry.crc32.to_le_bytes());
                // ISIZE is the uncompressed size modulo 2^32.
                dst[4..].copy_from_slice(&entry.uncompressed_size.to_le_bytes()[..4]);
            }
            Self::Zstd => {}
        }
    }

    pub const fn content_length(self, entry: &Entry) -> u64 {
        entry.compressed_size + self.prefix().len() as u64 + self.trailer_len() as u64
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_native_coding_of_each_method() {
        assert_eq!(NativeCoding::of(Method::Deflate), Some(NativeCoding::Gzip));
        assert_eq!(NativeCoding::of(Method::Zstd), Some(NativeCoding::Zstd));
        for method in [
            Method::Store,
            Method::Deflate64,
            Method::Bzip2,
            Method::Lzma,
            Method::Unrecognized(0xFFFF),
        ] {
            assert_eq!(NativeCoding::of(method), None, "{method:?}");
        }
    }

    #[test]
    fn test_native_coding_is_accepted_only_by_its_own_name() {
        let gzip = AcceptedEncodings {
            gzip: true,
            zstd: false,
        };
        assert!(NativeCoding::Gzip.is_accepted_by(gzip));
        assert!(!NativeCoding::Zstd.is_accepted_by(gzip));
    }
}
