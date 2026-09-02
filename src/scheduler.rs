// Nothing drives this yet: the pipeline stages that will be are the
// next commit. Everything here is exercised by its own tests, over real
// pipes; the allow goes when the shell starts calling it.
#![allow(dead_code)]

// Running several coroutines until they are all finished, switching
// between them when one cannot make progress.
//
// `coroutine` knows how to stop one execution and start another, and
// nothing about why. This is the why: a pipeline stage that has filled
// the pipe it writes, or emptied the one it reads, must hand the thread
// to whichever stage can unblock it. That is the whole of the
// scheduling policy -- there is no fairness to tune and no preemption,
// because the only reason a stage ever stops is that it is waiting for
// a descriptor, and the only thing that can help is another stage.
//
// The loop is therefore: run everything that can run, and when nothing
// can, sleep in `poll(2)` until the kernel says one of them can. That
// last part is what makes this a scheduler rather than a spin: with two
// stages both blocked, the process should be idle, not burning a core.

use std::cell::Cell;
use std::os::fd::RawFd;

use crate::coroutine::{Coroutine, State};

/// Why a coroutine stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Park {
    /// Gave up the thread voluntarily; can be resumed at any time.
    Ready,
    /// Waiting for `fd` to have something to read.
    Readable(RawFd),
    /// Waiting for `fd` to have room to write.
    Writable(RawFd),
}

thread_local! {
    /// Filled in by the `park_*` calls just before yielding, read by
    /// the scheduler just after the yield comes back.
    ///
    /// A side channel rather than a return value because the yield can
    /// happen arbitrarily deep inside an evaluator that has no idea it
    /// is running as a coroutine -- which is the property that lets the
    /// shell's own I/O paths park without every caller in between
    /// having to thread a reason back out.
    static REASON: Cell<Park> = const { Cell::new(Park::Ready) };
}

/// Gives up the thread, to be resumed whenever the scheduler comes
/// round again.
pub fn park_ready() {
    REASON.with(|r| r.set(Park::Ready));
    crate::coroutine::yield_now();
}

/// Gives up the thread until `fd` can be read without blocking.
pub fn park_readable(fd: RawFd) {
    REASON.with(|r| r.set(Park::Readable(fd)));
    crate::coroutine::yield_now();
}

/// Gives up the thread until `fd` can be written without blocking.
pub fn park_writable(fd: RawFd) {
    REASON.with(|r| r.set(Park::Writable(fd)));
    crate::coroutine::yield_now();
}

struct Task {
    co: Box<Coroutine>,
    park: Park,
}

/// A set of coroutines run to completion together.
#[derive(Default)]
pub struct Scheduler {
    tasks: Vec<Task>,
}

impl Scheduler {
    pub fn new() -> Scheduler {
        Scheduler { tasks: Vec::new() }
    }

    /// Adds a coroutine. Nothing runs until `run`.
    pub fn add(&mut self, body: impl FnOnce() + 'static) -> std::io::Result<()> {
        self.tasks.push(Task { co: Coroutine::new(body)?, park: Park::Ready });
        Ok(())
    }

    /// Runs every coroutine until all of them have finished.
    ///
    /// Returns whether any of them panicked -- which the caller has to
    /// decide about, since a panicking pipeline stage is not something
    /// this can sensibly recover from on its own.
    pub fn run(&mut self) -> bool {
        while self.tasks.iter().any(|t| t.co.state() != State::Done) {
            let mut ran_something = false;
            for index in 0..self.tasks.len() {
                if self.tasks[index].co.state() == State::Done {
                    continue;
                }
                if !self.can_run(index) {
                    continue;
                }
                // Cleared before the resume so that a coroutine which
                // finishes, rather than parking, does not leave the
                // previous reason behind for the scheduler to believe.
                REASON.with(|r| r.set(Park::Ready));
                self.tasks[index].co.resume();
                self.tasks[index].park = REASON.with(|r| r.get());
                ran_something = true;
            }
            if ran_something {
                continue;
            }
            // Everything still running is waiting for a descriptor.
            // Sleeping here rather than spinning is the difference
            // between an idle process and a busy one.
            if !self.wait_for_any() {
                // Nothing to wait for and nothing ran: no descriptor
                // will ever become ready, so the remaining coroutines
                // cannot proceed. Letting them run once more turns a
                // hang into whatever they do about a closed pipe.
                for task in self.tasks.iter_mut().filter(|t| t.co.state() != State::Done) {
                    task.park = Park::Ready;
                }
            }
        }
        self.tasks.iter().any(|t| t.co.panicked())
    }

    fn can_run(&self, index: usize) -> bool {
        match self.tasks[index].park {
            Park::Ready => true,
            Park::Readable(fd) => crate::poll::poll_one(fd, 0),
            Park::Writable(fd) => poll_writable(fd, 0),
        }
    }

    /// Blocks until one of the parked descriptors is ready. `false` if
    /// there was nothing to wait for.
    fn wait_for_any(&self) -> bool {
        let mut any = false;
        for task in self.tasks.iter().filter(|t| t.co.state() != State::Done) {
            match task.park {
                Park::Ready => return true,
                Park::Readable(fd) | Park::Writable(fd) => {
                    let _ = fd;
                    any = true;
                }
            }
        }
        if !any {
            return false;
        }
        // One at a time is enough here: a pipeline is a handful of
        // stages, and whichever is waited for first either becomes
        // ready or the timeout expires and the loop tries the rest.
        // A single `poll` over all of them would be better with many
        // more tasks than a pipeline ever has.
        for task in self.tasks.iter().filter(|t| t.co.state() != State::Done) {
            let ready = match task.park {
                Park::Ready => true,
                Park::Readable(fd) => crate::poll::poll_one(fd, POLL_SLICE_MS),
                Park::Writable(fd) => poll_writable(fd, POLL_SLICE_MS),
            };
            if ready {
                return true;
            }
        }
        true
    }
}

/// How long to wait on one descriptor before trying the next.
///
/// Only reached when every stage is blocked, so this is a bound on how
/// long the process sleeps before rechecking, not a poll interval: the
/// wait ends as soon as the descriptor is ready.
const POLL_SLICE_MS: i32 = 20;

/// `poll_one`, for writability.
fn poll_writable(fd: RawFd, timeout_ms: i32) -> bool {
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    }
    const POLLOUT: i16 = 0x004;
    let mut p = PollFd { fd, events: POLLOUT, revents: 0 };
    unsafe { poll(&mut p as *mut PollFd, 1, timeout_ms) > 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::rc::Rc;

    #[test]
    fn every_task_runs_to_completion() {
        let done = Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut s = Scheduler::new();
        for name in ["a", "b", "c"] {
            let done = Rc::clone(&done);
            s.add(move || done.borrow_mut().push(name)).unwrap();
        }
        assert!(!s.run(), "nothing panicked");
        let mut got = done.borrow().clone();
        got.sort_unstable();
        assert_eq!(got, ["a", "b", "c"]);
    }

    #[test]
    fn a_voluntary_park_lets_the_others_move() {
        let log = Rc::new(std::cell::RefCell::new(String::new()));
        let mut s = Scheduler::new();
        for name in ['A', 'B'] {
            let log = Rc::clone(&log);
            s.add(move || {
                for i in 0..2 {
                    log.borrow_mut().push(name);
                    log.borrow_mut().push_str(&i.to_string());
                    park_ready();
                }
            })
            .unwrap();
        }
        s.run();
        assert_eq!(*log.borrow(), "A0B0A1B1");
    }

    // The case the whole thing exists for: one coroutine cannot finish
    // until another does something, and the scheduler has to work that
    // out from the descriptors rather than from being told.
    #[test]
    fn a_reader_waits_for_a_writer_through_a_real_pipe() {
        let (read_end, write_end) = crate::exec::make_pipe_for_test().unwrap();
        crate::pty::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&read_end));
        let got = Rc::new(std::cell::RefCell::new(String::new()));

        let mut s = Scheduler::new();
        // The reader goes first and blocks immediately: nothing has
        // been written yet, and the writer has not run at all.
        let out = Rc::clone(&got);
        let read_fd = std::os::fd::AsRawFd::as_raw_fd(&read_end);
        s.add(move || {
            let mut file = std::fs::File::from(read_end);
            let mut buf = [0u8; 64];
            let mut total = 0;
            while total < 5 {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n;
                        out.borrow_mut().push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => park_readable(read_fd),
                    Err(_) => break,
                }
            }
        })
        .unwrap();
        s.add(move || {
            let mut file = std::fs::File::from(write_end);
            for chunk in ["he", "ll", "o"] {
                park_ready();
                let _ = file.write_all(chunk.as_bytes());
            }
        })
        .unwrap();

        s.run();
        assert_eq!(*got.borrow(), "hello");
    }

    // A writer that fills the pipe must hand over to the reader rather
    // than spin, and the two together must move more than a pipe
    // buffer's worth -- which is the thing a single-threaded shell
    // could not do at all before this.
    #[test]
    fn a_writer_blocked_on_a_full_pipe_hands_over_to_the_reader() {
        let (read_end, write_end) = crate::exec::make_pipe_for_test().unwrap();
        crate::pty::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&read_end));
        crate::pty::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&write_end));
        let read_fd = std::os::fd::AsRawFd::as_raw_fd(&read_end);
        let write_fd = std::os::fd::AsRawFd::as_raw_fd(&write_end);
        // Comfortably more than any pipe buffer, so the writer is
        // guaranteed to block and to need the reader to continue.
        const TOTAL: usize = 1024 * 1024;
        let received = Rc::new(Cell::new(0usize));

        let mut s = Scheduler::new();
        let count = Rc::clone(&received);
        s.add(move || {
            let mut file = std::fs::File::from(read_end);
            let mut buf = vec![0u8; 4096];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => count.set(count.get() + n),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => park_readable(read_fd),
                    Err(_) => break,
                }
            }
        })
        .unwrap();
        s.add(move || {
            let mut file = std::fs::File::from(write_end);
            let payload = vec![b'x'; 4096];
            let mut written = 0;
            while written < TOTAL {
                match file.write(&payload) {
                    Ok(0) => break,
                    Ok(n) => written += n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => park_writable(write_fd),
                    Err(_) => break,
                }
            }
            // Dropped here, so the reader sees end-of-input and stops.
        })
        .unwrap();

        s.run();
        assert_eq!(received.get(), TOTAL, "everything written came out the other end");
    }

    #[test]
    fn a_panicking_task_is_reported_and_the_others_still_finish() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let finished = Rc::new(Cell::new(false));
        let f = Rc::clone(&finished);
        let mut s = Scheduler::new();
        s.add(|| panic!("stage failed")).unwrap();
        s.add(move || f.set(true)).unwrap();
        let panicked = s.run();
        std::panic::set_hook(previous);
        assert!(panicked);
        assert!(finished.get(), "the other task still ran to completion");
    }
}
