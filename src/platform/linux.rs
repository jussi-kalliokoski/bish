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

// `memfd_create(2)`'s own flag, and private: a constant that belongs to
// a call only one OS has belongs with that call, not in a table that
// would then need an invented Darwin counterpart.
const MFD_CLOEXEC: u32 = 1;

unsafe extern "C" {
    fn memfd_create(name: *const u8, flags: u32) -> i32;
}

/// A real file descriptor backed by anonymous memory: no name, no
/// directory entry, nothing to unlink, nothing left behind if bish dies
/// holding it.
///
/// `None` on a kernel without the call (before 3.17), which the caller
/// answers with a named temp file of its own.
///
/// Close-on-exec: the fd is `dup2`'d onto a child's stdout explicitly
/// where it is wanted, and must not leak into anything else spawned.
pub(crate) fn anonymous_file() -> Option<std::fs::File> {
    let fd = unsafe { memfd_create(c"bish-capture".as_ptr() as *const u8, MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: a fresh descriptor, owned by nothing else.
    Some(unsafe { std::fs::File::from_raw_fd(fd) })
}
