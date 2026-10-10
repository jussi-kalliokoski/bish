//! What macOS does that POSIX doesn't, and the POSIX steps it needs
//! where Linux has one call. Every name answers one in `linux.rs`, and
//! `mod.rs`'s test fails if the two differ.

use super::sys;
use super::unix::fcntl;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

unsafe extern "C" {
    fn pipe(fds: *mut i32) -> i32;
}

/// A pipe with both ends close-on-exec. macOS has no `pipe2`, so the flag
/// goes on afterwards, end by end, and a `fork` between the calls
/// inherits ends meant to stay here; bish spawns from other threads too
/// (a language server, a debug adapter). The window is two `fcntl`s wide;
/// closing it would need a lock around every spawn.
pub(crate) fn pipe_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    if unsafe { pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Owned at once, so an `fcntl` failure below closes both instead of
    // leaking. SAFETY: pipe succeeded, so both are fresh descriptors owned
    // by nothing else.
    let ends = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for end in [&ends.0, &ends.1] {
        if unsafe { fcntl(end.as_raw_fd(), sys::F_SETFD, sys::FD_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(ends)
}

/// A file descriptor with no name. macOS has no `memfd_create`, and
/// `shm_open` is worse: one global namespace with a 31-character name
/// limit, and objects that can only be mapped, not read and written as a
/// capture fd must be. So a temp file unlinked at once, costing a
/// directory entry and an unlink per `$( )`. The mechanism is POSIX, in
/// `unix.rs`, so it's tested without a Mac.
pub(crate) fn anonymous_file() -> Option<std::fs::File> {
    super::unix::unlinked_temp_file("capture")
}

unsafe extern "C" {
    fn getpeereid(socket: i32, user: *mut u32, group: *mut u32) -> i32;
}

/// The real user id at the other end of an accepted UNIX-domain socket.
/// macOS has no `SO_PEERCRED`; `getpeereid(2)` answers in one call.
/// (`LOCAL_PEERCRED` fills an `xucred` whose layout has changed between
/// releases.) Defence in depth beside the socket directory's 0700,
/// checked before trusting a byte.
pub(crate) fn peer_user(socket: std::os::unix::io::RawFd) -> std::io::Result<u32> {
    let mut user = 0u32;
    let mut group = 0u32;
    match unsafe { getpeereid(socket, &mut user, &mut group) } {
        0 => Ok(user),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// What must stay open beside a new pty master for it to be usable
/// before a child opens the slave: here, the slave. Every ioctl on a
/// macOS master fails with ENOTTY until the slave is opened, and the
/// slave's last close resets the terminal, size included, so the resize
/// done before spawning would be refused and the child start at 0x0.
/// Held until the child has its own copy; close-on-exec and `O_NOCTTY`,
/// so it's never a controlling terminal or inherited.
pub(crate) fn pty_keepalive(slave_path: &str) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).write(true).custom_flags(sys::O_NOCTTY).open(slave_path).map(Some)
}
