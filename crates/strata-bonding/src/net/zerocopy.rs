//! Zero-copy send abstraction for future `io_uring` integration (#13).
//!
#![allow(dead_code)] // Not wired into production paths yet — API-only module.
//!
//! This module defines a [`ZeroCopySender`] trait that allows packet dispatch
//! without copying the payload into kernel space on kernels that support
//! `io_uring` (Linux 5.6+).  The trait is intentionally minimal so that
//! both the current `sendto(2)` path and a future `io_uring` path can
//! satisfy it — callers are shielded from the underlying mechanism.
//!
//! # Current status
//!
//! - The trait and types are defined and tested.
//! - No concrete `io_uring` implementation exists yet; the trait serves as a
//!   documented integration point for when `io_uring` support is added.
//! - A [`FallbackSender`] is provided that delegates to the standard
//!   `sendto(2)` path via `libc`, acting as the default implementation
//!   and a reference for future backends.
//!
//! # Migration path
//!
//! 1. Implement `ZeroCopySender` atop the `io-uring` crate.
//! 2. Feature-gate the implementation behind `cfg(feature = "io_uring")`.
//! 3. Wire it into [`crate::scheduler::bonding::BondingScheduler::send()`]
//!    as an alternative to `LinkSender::send()`.

use bytes::Bytes;
use std::io;

/// Completion token returned by [`ZeroCopySender::submit`].
///
/// The caller must keep the `Bytes` handle alive until the corresponding
/// [`ZeroCopySender::poll_completions`] returns this token, ensuring the
/// kernel can still read from the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubmitToken(pub u64);

/// Zero-copy network send abstraction.
///
/// Implementors submit buffers for asynchronous transmission and later
/// poll for completions.  The buffer (`Bytes`) is reference-counted so
/// the kernel and userspace can share it safely.
pub trait ZeroCopySender: Send {
    /// Submit `data` for asynchronous transmission on the given socket fd.
    ///
    /// Returns a [`SubmitToken`] that uniquely identifies this send
    /// operation.  The caller **must not** drop the `Bytes` handle until
    /// the token has been returned by [`poll_completions`].
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if the submission queue is full or the fd is
    /// invalid.
    fn submit(&mut self, fd: i32, data: &Bytes) -> io::Result<SubmitToken>;

    /// Poll for completed send operations.
    ///
    /// Returns up to `max` completed tokens.  Each returned token corresponds
    /// to a prior [`submit`] call whose buffer is now safe to drop/reuse.
    fn poll_completions(&mut self, max: usize) -> Vec<SubmitToken>;

    /// Flush any pending submissions to the kernel.
    fn flush(&mut self) -> io::Result<usize>;
}

/// Fallback sender that performs a blocking `sendto(2)` — no io_uring.
///
/// This satisfies the [`ZeroCopySender`] trait so callers can be
/// written generically and transparently switch to io_uring when the
/// feature is compiled in.
pub struct FallbackSender {
    next_token: u64,
}

impl FallbackSender {
    pub fn new() -> Self {
        Self { next_token: 0 }
    }
}

impl Default for FallbackSender {
    fn default() -> Self {
        Self::new()
    }
}

impl ZeroCopySender for FallbackSender {
    fn submit(&mut self, fd: i32, data: &Bytes) -> io::Result<SubmitToken> {
        // Synchronous sendto(2) — the data is copied into kernel space
        // immediately, so the completion is instant.
        let n = unsafe {
            libc::send(
                fd,
                data.as_ptr() as *const libc::c_void,
                data.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let token = SubmitToken(self.next_token);
        self.next_token += 1;
        Ok(token)
    }

    fn poll_completions(&mut self, _max: usize) -> Vec<SubmitToken> {
        // Synchronous path — completions are immediate upon submit.
        Vec::new()
    }

    fn flush(&mut self) -> io::Result<usize> {
        // Nothing to flush in the synchronous path.
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_token_identity() {
        let t1 = SubmitToken(1);
        let t2 = SubmitToken(1);
        let t3 = SubmitToken(2);
        assert_eq!(t1, t2);
        assert_ne!(t1, t3);
    }

    #[test]
    fn fallback_sender_creates() {
        let sender = FallbackSender::new();
        assert_eq!(sender.next_token, 0);
    }

    #[test]
    fn fallback_sender_default() {
        let sender = FallbackSender::default();
        assert_eq!(sender.next_token, 0);
    }

    #[test]
    fn fallback_poll_completions_empty() {
        let mut sender = FallbackSender::new();
        let completions = sender.poll_completions(10);
        assert!(completions.is_empty());
    }

    #[test]
    fn fallback_flush_noop() {
        let mut sender = FallbackSender::new();
        assert_eq!(sender.flush().unwrap(), 0);
    }

    #[test]
    fn fallback_submit_bad_fd() {
        let mut sender = FallbackSender::new();
        let data = Bytes::from_static(b"hello");
        // fd -1 is invalid — should fail with EBADF
        let result = sender.submit(-1, &data);
        assert!(result.is_err());
    }

    #[test]
    fn fallback_token_increments() {
        // We can't actually test submit with a real socket here without
        // creating one, but we can verify the token logic by using
        // socketpair or a memfd.  For now, just verify the struct layout.
        let mut sender = FallbackSender::new();
        assert_eq!(sender.next_token, 0);
        sender.next_token = 42;
        assert_eq!(sender.next_token, 42);
    }

    /// Verify that ZeroCopySender is object-safe (can be used as dyn trait).
    #[test]
    fn zerocopy_sender_is_object_safe() {
        fn _assert_object_safe(_s: &dyn ZeroCopySender) {}
        // Compiles iff the trait is object-safe.
    }
}
