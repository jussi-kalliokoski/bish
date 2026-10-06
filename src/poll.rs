// Waiting on several descriptors at once, so repl.rs's main loop can
// watch stdin, a job's pty master, a session socket and the resize
// self-pipe together instead of blocking on one `read` with everything
// else checked once per iteration in between.
//
// The syscall and its struct are in `platform`; what is here is the
// part that is bish's -- which descriptors count as "ready" (a closed
// peer does, see `PollSet`), the self-pipe trick, and the resize
// handler.
#![allow(dead_code)]

use std::io;
use std::os::unix::io::RawFd;

use crate::platform::{self, PollFd};

// Under this module's own names, which is where the rest of bish asks.
pub const POLLIN: i16 = platform::POLL_READABLE;
pub const POLLERR: i16 = platform::POLL_ERROR;
pub const POLLHUP: i16 = platform::POLL_HUNG_UP;

// True if `fd` has input available within timeout_ms. The single-fd
// case term::stdin_ready wraps.
pub fn poll_one(fd: RawFd, timeout_ms: i32) -> bool {
    let mut watched = [PollFd::watching(fd, POLLIN)];
    platform::wait_for_ready(&mut watched, timeout_ms).is_ok_and(|ready| ready > 0) && watched[0].reported() & POLLIN != 0
}

/// `poll_one`, counting a closed peer as ready.
///
/// A pipe whose writer has gone reports `POLLHUP`, not `POLLIN` -- so
/// anything waiting for "readable" alone waits for ever on a descriptor
/// whose next read would return 0. That is the same reason `PollSet`
/// below watches for it, and it is what a pipeline stage waiting for
/// end-of-input needs.
pub fn poll_readable_or_eof(fd: RawFd, timeout_ms: i32) -> bool {
    let mut watched = [PollFd::watching(fd, POLLIN)];
    platform::wait_for_ready(&mut watched, timeout_ms).is_ok_and(|ready| ready > 0) && watched[0].reported() & (POLLIN | POLLHUP | POLLERR) != 0
}

// A set of fds, each watched for POLLIN (readable) plus POLLHUP/POLLERR
// (so a closed peer -- a dead job's pty, a disconnected client socket --
// is reported as "ready" too: the next read on it returns 0/an error,
// which is how callers already detect EOF/disconnect, rather than this
// set silently never waking for it again).
pub struct PollSet {
    fds: Vec<PollFd>,
}

impl PollSet {
    pub fn new() -> PollSet {
        PollSet { fds: Vec::new() }
    }

    pub fn add(&mut self, fd: RawFd) {
        if !self.fds.iter().any(|p| p.fd() == fd) {
            self.fds.push(PollFd::watching(fd, POLLIN));
        }
    }

    pub fn remove(&mut self, fd: RawFd) {
        self.fds.retain(|p| p.fd() != fd);
    }

    // `timeout_ms`: None blocks indefinitely; Some(0) polls without
    // blocking at all. Returns the registered fds that are ready, in
    // registration order (poll(2) itself has no concept of readiness
    // order). An empty set returns immediately with nothing ready
    // rather than ever calling poll(2) with nfds=0 -- that call is
    // well-defined (it just sleeps for `timeout`), but "wait
    // indefinitely on nothing" would otherwise hang this forever, which
    // is never what an empty set's caller actually wants.
    pub fn wait(&mut self, timeout_ms: Option<i32>) -> io::Result<Vec<RawFd>> {
        if self.fds.is_empty() {
            return Ok(Vec::new());
        }
        for p in &mut self.fds {
            p.clear();
        }
        // A signal interrupting the wait is not an error and not a
        // readiness either: anything worth acting on arrives through one
        // of these descriptors (the self-pipe below), so the layer
        // reports it as nothing ready.
        platform::wait_for_ready(&mut self.fds, timeout_ms.unwrap_or(-1))?;
        Ok(self.fds.iter().filter(|p| p.reported() & (POLLIN | POLLHUP | POLLERR) != 0).map(|p| p.fd()).collect())
    }
}

impl Default for PollSet {
    fn default() -> PollSet {
        PollSet::new()
    }
}

// The standard "self-pipe trick": a pipe whose write end a signal
// handler can safely write to (write(2) is async-signal-safe; almost
// nothing else usable inside a handler is) to wake a blocked poll()
// call immediately. Register its read end with a PollSet the same way
// any other fd; the byte's value carries no meaning -- `drain` just
// empties whatever accumulated, so the next `wait` doesn't spuriously
// return immediately on a byte already consumed.
pub struct SelfPipe {
    read_fd: RawFd,
    write_fd: RawFd,
}

impl SelfPipe {
    pub fn new() -> io::Result<SelfPipe> {
        let (read_fd, write_fd) = platform::wake_pipe()?;
        Ok(SelfPipe { read_fd, write_fd })
    }

    pub fn read_fd(&self) -> RawFd {
        self.read_fd
    }

    // The value a signal handler should stash (typically in a static
    // AtomicI32, set once at startup) and pass to
    // wake_from_signal_handler. Exposing the raw fd rather than a
    // method that writes through `&self` keeps "this gets called from
    // inside a signal handler, not through the owning struct" an
    // explicit, visible contract at the call site -- and this struct is
    // meant to live for the whole process once installed, never dropped
    // out from under a handler that might still reference its fd.
    pub fn write_fd(&self) -> RawFd {
        self.write_fd
    }

    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        // Non-blocking, so "nothing left" arrives as an error rather
        // than as a wait.
        while platform::read_bytes(self.read_fd, &mut buf).is_ok_and(|n| n > 0) {}
    }
}

impl Drop for SelfPipe {
    fn drop(&mut self) {
        platform::close_fd(self.read_fd);
        platform::close_fd(self.write_fd);
    }
}

// Writes one wake-up byte to `fd` (a SelfPipe's write_fd) -- async-
// signal-safe, the only part of this module meant to be called from
// inside a real signal handler. A full pipe buffer (extremely unlikely
// for one byte written at a time, but possible under a storm of signals
// with nothing draining) is treated as "already woken, nothing more to
// do" rather than an error -- losing a redundant wake-up byte when
// one's already queued doesn't lose the wake-up itself.
pub fn wake_from_signal_handler(fd: RawFd) {
    platform::wake(fd);
}

static SIGWINCH_WAKE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn sigwinch_wake_handler(_sig: i32) {
    let fd = SIGWINCH_WAKE_FD.load(std::sync::atomic::Ordering::SeqCst);
    if fd >= 0 {
        wake_from_signal_handler(fd);
    }
}

// Installs a SIGWINCH handler that wakes `write_fd` (a SelfPipe's own
// write_fd) directly, instead of setting a flag polled once per
// iteration -- for a process with no other on_idle-style periodic check
// to notice a resize between `PollSet::wait` calls (session.rs's own
// client loop: it has nothing else to interleave, so it blocks
// genuinely indefinitely, unlike repl::run's own exec::
// install_winch_handler/take_winch, a different flag-based mechanism
// for a different caller -- deliberately not reused here to avoid this
// module depending on exec.rs for one signal number). Only one
// installation is meaningful at a time per process, which is all any
// current caller needs.
pub fn install_sigwinch_wake(write_fd: RawFd) {
    SIGWINCH_WAKE_FD.store(write_fd, std::sync::atomic::Ordering::SeqCst);
    platform::on_signal(platform::SIGWINCH, sigwinch_wake_handler);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_set_reports_a_pipe_readable_after_a_write() {
        let pipe = SelfPipe::new().expect("self pipe");
        let mut set = PollSet::new();
        set.add(pipe.read_fd());
        assert_eq!(set.wait(Some(0)).expect("wait"), Vec::<RawFd>::new(), "nothing written yet");
        wake_from_signal_handler(pipe.write_fd());
        let ready = set.wait(Some(1000)).expect("wait");
        assert_eq!(ready, vec![pipe.read_fd()]);
    }

    #[test]
    fn poll_set_wait_times_out_with_nothing_ready() {
        let pipe = SelfPipe::new().expect("self pipe");
        let mut set = PollSet::new();
        set.add(pipe.read_fd());
        let ready = set.wait(Some(50)).expect("wait");
        assert!(ready.is_empty());
    }

    #[test]
    fn poll_set_wait_on_an_empty_set_returns_immediately() {
        let mut set = PollSet::new();
        let start = std::time::Instant::now();
        let ready = set.wait(None).expect("wait");
        assert!(ready.is_empty());
        assert!(start.elapsed() < std::time::Duration::from_millis(500), "an empty PollSet must never block");
    }

    #[test]
    fn self_pipe_drain_empties_a_pending_byte() {
        let pipe = SelfPipe::new().expect("self pipe");
        wake_from_signal_handler(pipe.write_fd());
        pipe.drain();
        let mut set = PollSet::new();
        set.add(pipe.read_fd());
        assert!(set.wait(Some(0)).expect("wait").is_empty(), "a drained byte shouldn't leave the pipe ready");
    }

    #[test]
    fn poll_set_distinguishes_multiple_fds() {
        let pipe_a = SelfPipe::new().expect("pipe a");
        let pipe_b = SelfPipe::new().expect("pipe b");
        let mut set = PollSet::new();
        set.add(pipe_a.read_fd());
        set.add(pipe_b.read_fd());
        wake_from_signal_handler(pipe_b.write_fd());
        let ready = set.wait(Some(1000)).expect("wait");
        assert_eq!(ready, vec![pipe_b.read_fd()]);
    }

    #[test]
    fn poll_set_remove_stops_watching_an_fd() {
        let pipe = SelfPipe::new().expect("self pipe");
        let mut set = PollSet::new();
        set.add(pipe.read_fd());
        set.remove(pipe.read_fd());
        wake_from_signal_handler(pipe.write_fd());
        assert!(set.wait(Some(50)).expect("wait").is_empty(), "a removed fd must not be reported ready");
    }

    #[test]
    fn poll_one_matches_pollset_for_a_single_fd() {
        let pipe = SelfPipe::new().expect("self pipe");
        assert!(!poll_one(pipe.read_fd(), 0));
        wake_from_signal_handler(pipe.write_fd());
        assert!(poll_one(pipe.read_fd(), 1000));
    }
}
