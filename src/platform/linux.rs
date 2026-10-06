//! What Linux does that POSIX does not.
//!
//! A capability belongs here rather than in `unix.rs` when the two OSes
//! do it differently enough that one body cannot serve both -- see
//! `darwin.rs` for the other half of every pair, and `mod.rs` for the
//! test that keeps them matched.

use super::sys;
use std::os::fd::{FromRawFd, OwnedFd};

unsafe extern "C" {
    fn pipe2(fds: *mut i32, flags: i32) -> i32;
}

/// A pipe whose two ends are close-on-exec from the moment they exist.
///
/// One call, and the flag is set before the fds are reachable from
/// anywhere -- which is the part macOS cannot reproduce; see the
/// counterpart in `darwin.rs`.
pub(crate) fn pipe_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    if unsafe { pipe2(fds.as_mut_ptr(), sys::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned success, so both are fresh descriptors
    // owned by nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}
