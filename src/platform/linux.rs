//! What Linux does that POSIX doesn't: what the two OSes do too
//! differently for one body in `unix.rs`. See `darwin.rs` for each pair's
//! other half and `mod.rs` for the test keeping them matched.

use super::sys;
use std::os::fd::{FromRawFd, OwnedFd};

unsafe extern "C" {
    fn pipe2(fds: *mut i32, flags: i32) -> i32;
}

/// A pipe with both ends close-on-exec from the start: one call, set
/// before the fds are reachable, which macOS can't do (`darwin.rs`).
pub(crate) fn pipe_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    if unsafe { pipe2(fds.as_mut_ptr(), sys::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned success, so both are fresh descriptors
    // owned by nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

// `memfd_create(2)`'s flag, private: it belongs with the call only one OS
// has, not in a table needing a Darwin counterpart.
const MFD_CLOEXEC: u32 = 1;

unsafe extern "C" {
    fn memfd_create(name: *const u8, flags: u32) -> i32;
}

/// A real descriptor backed by anonymous memory: no name, no directory
/// entry, nothing left if bish dies holding it. `None` before Linux 3.17,
/// when the caller uses a named temp file. Close-on-exec: it's `dup2`'d
/// onto a child's stdout where wanted and mustn't leak elsewhere.
pub(crate) fn anonymous_file() -> Option<std::fs::File> {
    let fd = unsafe { memfd_create(c"bish-capture".as_ptr() as *const u8, MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: a fresh descriptor, owned by nothing else.
    Some(unsafe { std::fs::File::from_raw_fd(fd) })
}

// `getsockopt(SOL_SOCKET, SO_PEERCRED)` and its struct, private: Darwin
// has no such option.
const SOL_SOCKET: i32 = 1;
const SO_PEERCRED: i32 = 17;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Ucred {
    pid: i32,
    uid: u32,
    gid: u32,
}

unsafe extern "C" {
    fn getsockopt(socket: i32, level: i32, option: i32, value: *mut u8, length: *mut u32) -> i32;
}

/// The real user id at the other end of an accepted UNIX-domain socket,
/// checked before trusting a byte: defence in depth beside the socket
/// directory's 0700, refusing other local users if a shared or
/// misconfigured runtime directory lets them connect.
pub(crate) fn peer_user(socket: std::os::unix::io::RawFd) -> std::io::Result<u32> {
    let mut credentials = Ucred::default();
    let mut length = std::mem::size_of::<Ucred>() as u32;
    let asked = unsafe { getsockopt(socket, SOL_SOCKET, SO_PEERCRED, &mut credentials as *mut Ucred as *mut u8, &mut length) };
    match asked {
        0 => Ok(credentials.uid),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// What must stay open beside a new pty master: nothing on Linux, whose
/// master answers at once. See darwin.rs.
pub(crate) fn pty_keepalive(_slave_path: &str) -> std::io::Result<Option<std::fs::File>> {
    Ok(None)
}
