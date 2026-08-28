// Hand-rolled poll(2) FFI -- no libc crate, same extern "C"-against-glibc
// style as term.rs/pty.rs. The one place in this codebase that declares
// poll(2) itself; term::stdin_ready (a single-fd, fixed-to-stdin
// convenience wrapper that predates this module) now goes through
// poll_one below instead of keeping its own separate copy.
//
// Exists so repl.rs's main loop can wait on several fds at once (stdin,
// a job's pty master, a session socket, the SIGWINCH self-pipe below)
// instead of blocking on a single read(2) with everything else checked
// only "once per iteration" in between -- see repl.rs's own "a real
// event loop... is M9b's job" comment for the gap this exists to close.
#![allow(dead_code)]

use std::io;
use std::os::unix::io::RawFd;

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

pub const POLLIN: i16 = 0x0001;
pub const POLLERR: i16 = 0x0008;
pub const POLLHUP: i16 = 0x0010;

unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    fn pipe(fds: *mut i32) -> i32;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn close(fd: i32) -> i32;
}

// True if `fd` has input available within timeout_ms. The single-fd
// case term::stdin_ready wraps.
pub fn poll_one(fd: RawFd, timeout_ms: i32) -> bool {
    let mut pfd = PollFd { fd, events: POLLIN, revents: 0 };
    unsafe { poll(&mut pfd, 1, timeout_ms) > 0 && (pfd.revents & POLLIN) != 0 }
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
        if !self.fds.iter().any(|p| p.fd == fd) {
            self.fds.push(PollFd { fd, events: POLLIN, revents: 0 });
        }
    }

    pub fn remove(&mut self, fd: RawFd) {
        self.fds.retain(|p| p.fd != fd);
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
            p.revents = 0;
        }
        let timeout = timeout_ms.unwrap_or(-1);
        let n = unsafe { poll(self.fds.as_mut_ptr(), self.fds.len() as u64, timeout) };
        if n < 0 {
            let err = io::Error::last_os_error();
            // A signal (e.g. SIGWINCH itself, delivered while blocked
            // here) interrupting the call isn't a real error -- callers
            // that care about the signal already learn about it via the
            // self-pipe below, not via poll's own return value.
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(Vec::new());
            }
            return Err(err);
        }
        Ok(self.fds.iter().filter(|p| p.revents & (POLLIN | POLLHUP | POLLERR) != 0).map(|p| p.fd).collect())
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
        let mut fds = [0i32; 2];
        if unsafe { pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        crate::pty::set_nonblocking(fds[0]);
        crate::pty::set_nonblocking(fds[1]);
        Ok(SelfPipe { read_fd: fds[0], write_fd: fds[1] })
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
        loop {
            let n = unsafe { read(self.read_fd, buf.as_mut_ptr(), buf.len()) };
            if n <= 0 {
                break;
            }
        }
    }
}

impl Drop for SelfPipe {
    fn drop(&mut self) {
        unsafe {
            close(self.read_fd);
            close(self.write_fd);
        }
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
    unsafe {
        write(fd, [0u8].as_ptr(), 1);
    }
}

const SIGWINCH: i32 = 28;
static SIGWINCH_WAKE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

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
    unsafe {
        signal(SIGWINCH, sigwinch_wake_handler as *const () as usize);
    }
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
