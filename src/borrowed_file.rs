//! Owner + cross-thread token + borrowed-file-reader trio.
//!
//! * [`FdOwner`] — owns the file descriptor, closes it on drop.
//! * [`FdBorrowToken`] — `Send + Copy` token to pass across threads.
//! * [`BorrowedFile`] — async `read_at` only, never closes the fd.
//!
//! Conceptually analogous to the `OwnedFd` / `BorrowedFd` / `RawFd` split in
//! `std::os::fd`, but adapted for monoio's `!Send + !Sync` runtime types.

use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

use monoio::BufResult;
use monoio::buf::IoBufMut;

/// Owns an open file descriptor and guarantees it stays open.
///
/// Construct via [`FdOwner::from`], which takes ownership of a
/// [`monoio::fs::File`] and prevents it from closing the fd. On [`Drop`], the
/// fd is closed via the OS.
///
/// Call [`FdOwner::token`] to obtain a [`FdBorrowToken`] that can be sent to
/// another thread, where a [`BorrowedFile`] can be constructed.
///
/// # Example
///
/// ```ignore
/// let owner = FdOwner::from(file);
/// let token = owner.token();
/// // send token to another thread...
/// let borrowed = token.to_borrowed_file();
/// ```
pub struct FdOwner {
    fd: RawFd,
}

impl FdOwner {
    /// Take ownership of a [`monoio::fs::File`] and keep its fd open.
    ///
    /// The original `File` is consumed (forgotten) — the fd is only closed
    /// when this `FdOwner` is dropped (typically at process exit).
    #[inline]
    pub fn from(file: monoio::fs::File) -> Self {
        let fd = file.as_raw_fd();
        std::mem::forget(file);
        Self { fd }
    }

    /// Obtain a thread-safe token that can be used to construct a
    /// [`BorrowedFile`] on any thread.
    #[inline]
    pub fn token<'a>(&'a self) -> FdBorrowToken<'a> {
        FdBorrowToken {
            fd: self.fd,
            _marker: std::marker::PhantomData,
        }
    }
}

impl Drop for FdOwner {
    fn drop(&mut self) {
        // Reconstruct a std File and let it drop, which closes the fd.
        // SAFETY: `self` owns this fd. The original monoio::fs::File was
        // forget()'d in Self::from(); no other code will close it.
        let _ = unsafe { std::fs::File::from_raw_fd(self.fd) };
    }
}

/// A `Send + Copy` token representing an open file descriptor.
///
/// Created by [`FdOwner::token`] and converted into a [`BorrowedFile`]
/// via [`FdBorrowToken::to_borrowed_file`] on the target thread.
#[derive(Copy, Clone)]
pub struct FdBorrowToken<'a> {
    fd: RawFd,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> FdBorrowToken<'a> {
    /// Construct a [`BorrowedFile`] from this token on the current thread's
    /// monoio runtime.
    ///
    /// The returned [`BorrowedFile`] reads from the fd without ever closing it.
    #[inline]
    pub fn to_borrowed_file(self) -> BorrowedFile<'a> {
        // SAFETY: FdOwner guarantees the fd stays open for the lifetime `'a`.
        // BorrowedFile's ManuallyDrop prevents any close.
        unsafe { BorrowedFile::from_raw_fd(self.fd) }
    }
}

/// A borrowed handle to an open file descriptor providing async positional
/// reads via monoio, without ever closing the underlying handle.
///
/// Unlike [`monoio::fs::File`], drop is a no-op -- this type does NOT own
/// the file description.
///
/// Constructed exclusively via [`FdBorrowToken::to_borrowed_file`].
///
/// Must be constructed inside the monoio runtime (io_uring instance) that
/// will use it, because [`monoio::fs::File`] is `!Send`.
#[derive(Debug)]
pub struct BorrowedFile<'a> {
    inner: ManuallyDrop<monoio::fs::File>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> BorrowedFile<'a> {
    /// Wrap a raw file descriptor into a `BorrowedFile`.
    ///
    /// # Safety
    ///
    /// - The raw fd MUST be a valid open file descriptor.
    /// - The raw fd MUST remain open for the entire lifetime of this object.
    /// - No code may `close()` the fd while this object exists.
    #[inline]
    unsafe fn from_raw_fd(raw_fd: RawFd) -> Self {
        // SAFETY: caller guarantees fd is valid and remains open.
        let std_file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
        // from_std calls std_file.into_raw_fd() — it consumes the std::fs::File
        // without closing, then stores the fd in a new SharedFd.
        let file = monoio::fs::File::from_std(std_file)
            .expect("from_std only fails when the underlying handle is a Windows handle");
        Self {
            inner: ManuallyDrop::new(file),
            _marker: std::marker::PhantomData,
        }
    }

    /// Read up to `buf.len()` bytes at `offset` from the file, returning the number of bytes read.
    ///
    /// Wraps [`monoio::fs::File::read_at`].
    #[inline]
    pub async fn read_at<T: IoBufMut>(&self, buf: T, offset: u64) -> BufResult<usize, T> {
        self.inner.read_at(buf, offset).await
    }
}
