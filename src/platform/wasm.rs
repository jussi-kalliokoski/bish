//! What wasm does where Linux and macOS use the kernel: the counterpart
//! to `linux.rs` and `darwin.rs`. On WASI preview 1 almost none of it
//! exists (cross-process pipes, peer credentials, anonymous capture
//! files), so these are refusals, which the rest of bish reports, rather
//! than link failures.

use std::os::fd::RawFd;

fn unsupported<T>(what: &str) -> std::io::Result<T> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, format!("{what} is not available on this platform")))
}

/// A close-on-exec pipe. There is no process to hand the other end to
/// here, so there is no pipe either.
pub(crate) fn pipe_cloexec() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    unsupported("a pipe")
}

/// A nameless file backed by anonymous memory. `None` asks the caller
/// to fall back to a named temp file, which is the right answer here:
/// wasm has no `memfd_create`, so the fallback is all there is.
pub(crate) fn anonymous_file() -> Option<std::fs::File> {
    None
}

/// The user on the other end of an accepted socket. There are no
/// UNIX-domain sockets to accept on, so this is never reached.
pub(crate) fn peer_user(_socket: RawFd) -> std::io::Result<u32> {
    unsupported("a socket peer's credentials")
}

/// What must stay open beside a pty master: nothing, since there are no
/// ptys.
pub(crate) fn pty_keepalive(_slave_path: &str) -> std::io::Result<Option<std::fs::File>> {
    Ok(None)
}
