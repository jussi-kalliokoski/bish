//! What a wasm target does where Linux and macOS reach for the kernel.
//!
//! The counterpart to `linux.rs` / `darwin.rs`: the capabilities whose
//! body cannot be shared because the OSes do them differently. On wasm
//! (WASI preview 1) almost none of them exist at all, so these are
//! honest refusals rather than alternative implementations -- a pipe
//! between two processes, a peer's credentials over a socket, and an
//! anonymous capture file are all things the sandbox does not offer.
//! The rest of bish calls them through the same names and gets an error
//! it can report, rather than a link failure.

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

/// What has to stay open beside a pty master. There are no ptys, so
/// nothing -- same answer as Linux, for want of the question.
pub(crate) fn pty_keepalive(_slave_path: &str) -> std::io::Result<Option<std::fs::File>> {
    Ok(None)
}
