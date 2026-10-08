//! What macOS does that POSIX does not -- and the POSIX dance it needs
//! where Linux has one call for the same thing.
//!
//! Every name here answers one in `linux.rs`, and `mod.rs`'s own test
//! fails the build if one of the two files grows something the other
//! does not have.

use super::sys;
use super::unix::fcntl;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

unsafe extern "C" {
    fn pipe(fds: *mut i32) -> i32;
}

/// A pipe whose two ends are close-on-exec.
///
/// macOS has no `pipe2`, so the flag goes on afterwards, one end at a
/// time. That gap is a real difference and not merely a slower way of
/// arriving: a `fork` between the two calls inherits ends that were
/// meant to stay with this process, and bish does spawn from threads
/// other than the one running the pipeline (a language server, a debug
/// adapter). The window is two `fcntl` calls wide, and closing it
/// properly needs a lock around every spawn rather than anything that
/// can be done here -- noted where it would bite rather than left to be
/// discovered.
pub(crate) fn pipe_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    if unsafe { pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Owned straight away, so an `fcntl` failure below closes them both
    // on the way out rather than leaking a pipe.
    // SAFETY: pipe returned success, so both are fresh descriptors owned
    // by nothing else.
    let ends = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for end in [&ends.0, &ends.1] {
        if unsafe { fcntl(end.as_raw_fd(), sys::F_SETFD, sys::FD_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(ends)
}

/// A file descriptor with no name.
///
/// macOS has no `memfd_create`, and `shm_open` -- the nearest thing --
/// is worse for this: its objects live in one global namespace with a
/// 31-character limit on the name, and on macOS they can only be
/// mapped, not read and written, which is exactly what a capture fd is
/// handed to a child to do. So: a temp file, unlinked the moment it
/// exists, which costs a directory entry and an unlink that Linux does
/// not pay. That is the price of a `$( )` here.
///
/// The mechanism is in `unix.rs` rather than here because every line of
/// it is POSIX and it can therefore be tested without a Mac.
pub(crate) fn anonymous_file() -> Option<std::fs::File> {
    super::unix::unlinked_temp_file("capture")
}

unsafe extern "C" {
    fn getpeereid(socket: i32, user: *mut u32, group: *mut u32) -> i32;
}

/// The real user id of whoever is on the other end of an accepted
/// UNIX-domain socket.
///
/// macOS has no `SO_PEERCRED` -- it has `getpeereid(2)`, which answers
/// the same question in one call and gives the group as well, which bish
/// does not need. (`LOCAL_PEERCRED` exists too and fills an `xucred`
/// whose layout has changed between releases; the dedicated call is both
/// simpler and stable.)
///
/// Same purpose as the Linux half: defence in depth on top of the socket
/// directory's own 0700, checked before a byte from the peer is trusted.
pub(crate) fn peer_user(socket: std::os::unix::io::RawFd) -> std::io::Result<u32> {
    let mut user = 0u32;
    let mut group = 0u32;
    match unsafe { getpeereid(socket, &mut user, &mut group) } {
        0 => Ok(user),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Whatever has to stay open beside a fresh pty's master for it to be
/// usable before a child has opened the slave.
///
/// Here, the slave itself. Every ioctl on a macOS master fails with
/// ENOTTY until the slave has been opened, and the slave's last close
/// resets the terminal, size included -- so the resize every caller
/// does between opening a pty and spawning onto it is refused, and the
/// child starts on a 0x0 terminal. Held until the child has its own
/// copy; close-on-exec (std's default) and `O_NOCTTY` so that it is
/// never anyone's controlling terminal or inheritance by accident.
pub(crate) fn pty_keepalive(slave_path: &str) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).write(true).custom_flags(sys::O_NOCTTY).open(slave_path).map(Some)
}
