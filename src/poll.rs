// Waiting on several descriptors at once, so repl.rs's loop watches
// stdin, a job's pty master, a session socket and the resize self-pipe
// together instead of blocking on one. The syscall is in `platform`;
// here is what's bish's: what counts as ready (a closed peer does, see
// `PollSet`), the self-pipe, and the resize handler.
#![allow(dead_code)]

use std::io;
use std::os::fd::RawFd;

use crate::platform::{self, PollFd};

// Under this module's names, where the rest of bish asks.
pub const POLLIN: i16 = platform::POLL_READABLE;
pub const POLLERR: i16 = platform::POLL_ERROR;
pub const POLLHUP: i16 = platform::POLL_HUNG_UP;

// Whether `fd` has input within `timeout_ms`; `term::stdin_ready` wraps
// it.
pub fn poll_one(fd: RawFd, timeout_ms: i32) -> bool {
    let mut watched = [PollFd::watching(fd, POLLIN)];
    platform::wait_for_ready(&mut watched, timeout_ms).is_ok_and(|ready| ready > 0) && watched[0].reported() & POLLIN != 0
}

/// `poll_one`, counting a closed peer as ready: a pipe whose writer went
/// reports `POLLHUP`, not `POLLIN`, so waiting for readable alone would
/// wait forever for a read that returns 0. A pipeline stage waiting for
/// end of input needs this.
pub fn poll_readable_or_eof(fd: RawFd, timeout_ms: i32) -> bool {
    let mut watched = [PollFd::watching(fd, POLLIN)];
    platform::wait_for_ready(&mut watched, timeout_ms).is_ok_and(|ready| ready > 0) && watched[0].reported() & (POLLIN | POLLHUP | POLLERR) != 0
}

// A set of fds, each watched for `POLLIN` plus `POLLHUP`/`POLLERR`, so a
// closed peer (a dead job's pty, a disconnected client) counts as ready:
// the next read returns 0 or an error, which is how callers detect it,
// rather than never waking.
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

    // `timeout_ms`: `None` blocks indefinitely, `Some(0)` doesn't block.
    // Returns the ready fds in registration order. An empty set returns at
    // once rather than calling poll(2) with nfds=0, which with no timeout
    // would sleep forever.
    pub fn wait(&mut self, timeout_ms: Option<i32>) -> io::Result<Vec<RawFd>> {
        if self.fds.is_empty() {
            return Ok(Vec::new());
        }
        for p in &mut self.fds {
            p.clear();
        }
        // A signal interrupting the wait is neither error nor readiness:
        // anything worth acting on comes through a descriptor (the
        // self-pipe).
        platform::wait_for_ready(&mut self.fds, timeout_ms.unwrap_or(-1))?;
        Ok(self.fds.iter().filter(|p| p.reported() & (POLLIN | POLLHUP | POLLERR) != 0).map(|p| p.fd()).collect())
    }
}

impl Default for PollSet {
    fn default() -> PollSet {
        PollSet::new()
    }
}

// The self-pipe trick: a pipe a signal handler can safely write to
// (write(2) is async-signal-safe; little else is) to wake a blocked
// poll(). Register its read end like any fd; the byte means nothing, and
// `drain` empties what accumulated so the next `wait` doesn't return at
// once.
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

    // The fd a signal handler stores (in a static set at startup) and passes
    // to `wake_from_signal_handler`: a raw fd, not a `&self` method, makes
    // "called from a handler" explicit at the call site. The struct lives for
    // the whole process once installed.
    pub fn write_fd(&self) -> RawFd {
        self.write_fd
    }

    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        // Non-blocking, so "nothing left" is an error, not a wait.
        while platform::read_bytes(self.read_fd, &mut buf).is_ok_and(|n| n > 0) {}
    }
}

impl Drop for SelfPipe {
    fn drop(&mut self) {
        platform::close_fd(self.read_fd);
        platform::close_fd(self.write_fd);
    }
}

// Writes one wake-up byte to `fd` (a `SelfPipe`'s write end):
// async-signal-safe, the only part meant for a real signal handler. A
// full pipe means a wake-up is already queued, so nothing is lost.
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

// Installs a SIGWINCH handler that writes to `write_fd` directly, for a
// process with no periodic check to notice a resize between waits
// (session.rs's client loop blocks indefinitely). repl::run uses
// `exec::install_winch_handler`/`take_winch`, a flag, instead; not
// reused, so this module needn't depend on exec.rs. One installation per
// process.
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
