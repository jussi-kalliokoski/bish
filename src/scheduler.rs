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
    /// What this task's fd 0 and fd 1 are, installed for the duration
    /// of each of its turns.
    ///
    /// A pipeline stage that is its own process gets its pipes *as*
    /// stdin and stdout, and everything downstream of that -- `exec
    /// {fd}<&0`, an external command inheriting the rest of the input,
    /// a builtin writing to fd 1 -- works because they are the real
    /// descriptors. Handing a stage a side channel instead breaks all
    /// of it, so the descriptors are swapped rather than the code that
    /// uses them. Two `dup2`s per switch, and switches only happen when
    /// a stage blocks.
    stdin: Option<std::os::fd::OwnedFd>,
    stdout: Option<std::os::fd::OwnedFd>,
    /// This task's own broken-pipe watch, swapped in around each resume
    /// -- see `exec::swap_broken_pipe` for why it cannot just be the
    /// thread's.
    broken: Option<bool>,
    /// Set when nothing will read this task's output any more. It is
    /// never resumed again -- see `cancel_running`.
    cancelled: bool,
}

impl Task {
    /// The descriptor a parked task actually meant.
    ///
    /// A stage parks on the number it was using -- 0 for its input, 1
    /// for its output -- and those numbers mean whatever was last
    /// installed on them, which is some *other* stage's pipe as soon as
    /// one runs. Waiting on the number rather than on this task's own
    /// descriptor waits for the wrong thing: sometimes one that is
    /// always ready, which spins, and sometimes one that never will be,
    /// which hangs. Both were observed before this existed.
    fn own_fd(&self, parked: std::os::fd::RawFd) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        match parked {
            0 => self.stdin.as_ref().map(|f| f.as_raw_fd()).unwrap_or(parked),
            1 => self.stdout.as_ref().map(|f| f.as_raw_fd()).unwrap_or(parked),
            other => other,
        }
    }
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
        self.tasks.push(Task { co: Coroutine::new(body)?, park: Park::Ready, broken: None, cancelled: false, stdin: None, stdout: None });
        Ok(())
    }

    /// Adds a coroutine that runs with `stdin`/`stdout` as its own fd 0
    /// and fd 1 -- see `Task::stdin`. `None` leaves that descriptor as
    /// whatever the shell running the scheduler had.
    pub fn add_with_fds(
        &mut self,
        body: impl FnOnce() + 'static,
        stdin: Option<std::os::fd::OwnedFd>,
        stdout: Option<std::os::fd::OwnedFd>,
    ) -> std::io::Result<()> {
        self.tasks.push(Task { co: Coroutine::new(body)?, park: Park::Ready, broken: None, cancelled: false, stdin, stdout });
        Ok(())
    }

    /// Whether anything is still running.
    pub fn is_idle(&self) -> bool {
        self.tasks.iter().all(|t| t.co.state() == State::Done || t.cancelled)
    }

    /// Gives every runnable coroutine one turn and returns, rather than
    /// running to completion.
    ///
    /// This is what a construct that outlives its command needs: a
    /// `<( )` producer has to make progress while the shell is waiting
    /// for the command *consuming* it, and that shell is not inside
    /// `run`. It is called from the places the shell would otherwise
    /// sit idle in the kernel -- see `Shell::pump_coroutines` -- so a
    /// live coroutine costs nothing and gets time wherever there was
    /// none being used.
    ///
    /// Blocking here would be wrong in a way it is not in `run`: the
    /// caller has its own reason to come back.
    pub fn step(&mut self) {
        if self.is_idle() {
            return;
        }
        let saved = crate::exec::save_fd012_for_scheduler();
        for index in 0..self.tasks.len() {
            if self.tasks[index].co.state() == State::Done || self.tasks[index].cancelled || !self.can_run(index) {
                continue;
            }
            REASON.with(|r| r.set(Park::Ready));
            let outer = crate::exec::swap_broken_pipe(self.tasks[index].broken);
            install_fds(&self.tasks[index], saved);
            self.tasks[index].co.resume();
            self.tasks[index].broken = crate::exec::swap_broken_pipe(outer);
            self.tasks[index].park = REASON.with(|r| r.get());
            if self.tasks[index].co.state() == State::Done {
                self.tasks[index].stdin = None;
                self.tasks[index].stdout = None;
            }
        }
        crate::exec::restore_fd012_for_scheduler(saved);
    }

    /// Stops every task that is still running: nothing is going to read
    /// what it produces.
    ///
    /// What the shell does once the command consuming a `<( )` has
    /// finished. Stopping rather than closing the descriptor, because
    /// "no output" has no descriptor to name: leaving `stdout` as
    /// `None` means "the shell's own", so a producer that kept going
    /// wrote its remaining output *to the terminal* --
    /// `head -3 <(while true; do echo x; done)` printed x's for as long
    /// as it was pumped.
    ///
    /// A cancelled task is never resumed again and is dropped by
    /// `retire_finished`, which unmaps its stack. Whatever that stack
    /// still owned is leaked rather than dropped -- see `Coroutine`'s
    /// own `Drop` -- which is the price of stopping something in the
    /// middle, and is what a real shell gets for free by having the
    /// kernel kill a process.
    pub fn cancel_running(&mut self) {
        for task in self.tasks.iter_mut().filter(|t| t.co.state() != State::Done) {
            task.cancelled = true;
            task.stdin = None;
            task.stdout = None;
        }
    }

    /// Drops every finished task, so a long-lived shell does not
    /// accumulate them.
    pub fn retire_finished(&mut self) {
        self.tasks.retain(|t| t.co.state() != State::Done && !t.cancelled);
    }

    /// Runs every coroutine until all of them have finished.
    ///
    /// Returns whether any of them panicked -- which the caller has to
    /// decide about, since a panicking pipeline stage is not something
    /// this can sensibly recover from on its own.
    pub fn run(&mut self) -> bool {
        // Restored before returning, and while sleeping in `poll`: the
        // shell that started this still owns fd 0 and fd 1 whenever no
        // task is running on them.
        let saved = crate::exec::save_fd012_for_scheduler();
        let panicked = self.run_tasks(saved);
        crate::exec::restore_fd012_for_scheduler(saved);
        panicked
    }

    fn run_tasks(&mut self, shells_own: [i32; 3]) -> bool {
        while !self.is_idle() {
            let mut ran_something = false;
            for index in 0..self.tasks.len() {
                if self.tasks[index].co.state() == State::Done || self.tasks[index].cancelled {
                    continue;
                }
                if !self.can_run(index) {
                    continue;
                }
                // Cleared before the resume so that a coroutine which
                // finishes, rather than parking, does not leave the
                // previous reason behind for the scheduler to believe.
                REASON.with(|r| r.set(Park::Ready));
                let outer = crate::exec::swap_broken_pipe(self.tasks[index].broken);
                install_fds(&self.tasks[index], shells_own);
                self.tasks[index].co.resume();
                self.tasks[index].broken = crate::exec::swap_broken_pipe(outer);
                self.tasks[index].park = REASON.with(|r| r.get());
                if self.tasks[index].co.state() == State::Done {
                    // A finished stage must stop holding its pipes, or
                    // the stage downstream waits for an end-of-input
                    // that cannot arrive: this scheduler is one of the
                    // things keeping the write end open. `install_fds`
                    // for the next task takes fd 1 off it; these two
                    // are the other references.
                    self.tasks[index].stdin = None;
                    self.tasks[index].stdout = None;
                }
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
                for task in self.tasks.iter_mut().filter(|t| t.co.state() != State::Done && !t.cancelled) {
                    task.park = Park::Ready;
                }
            }
        }
        self.tasks.iter().any(|t| t.co.panicked())
    }

    fn can_run(&self, index: usize) -> bool {
        let task = &self.tasks[index];
        match task.park {
            Park::Ready => true,
            Park::Readable(fd) => crate::poll::poll_readable_or_eof(task.own_fd(fd), 0),
            Park::Writable(fd) => poll_writable(task.own_fd(fd), 0),
        }
    }

    /// Blocks until one of the parked descriptors is ready. `false` if
    /// there was nothing to wait for.
    fn wait_for_any(&self) -> bool {
        let mut any = false;
        for task in self.tasks.iter().filter(|t| t.co.state() != State::Done && !t.cancelled) {
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
        for task in self.tasks.iter().filter(|t| t.co.state() != State::Done && !t.cancelled) {
            let ready = match task.park {
                Park::Ready => true,
                Park::Readable(fd) => crate::poll::poll_readable_or_eof(task.own_fd(fd), POLL_SLICE_MS),
                Park::Writable(fd) => poll_writable(task.own_fd(fd), POLL_SLICE_MS),
            };
            if ready {
                return true;
            }
        }
        true
    }
}

/// Points fd 0 and fd 1 at this task's own pipes for the duration of
/// its turn.
fn install_fds(task: &Task, shells_own: [i32; 3]) {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }
    use std::os::fd::AsRawFd;
    // Both, every time, and `None` means the shell's own rather than
    // "leave it alone": whatever the previous task installed is still
    // there otherwise, so the last stage of `a | b` would write into
    // the pipe `a` was writing to instead of to the terminal.
    let stdin = task.stdin.as_ref().map(|f| f.as_raw_fd()).unwrap_or(shells_own[0]);
    let stdout = task.stdout.as_ref().map(|f| f.as_raw_fd()).unwrap_or(shells_own[1]);
    unsafe {
        dup2(stdin, 0);
        dup2(stdout, 1);
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
