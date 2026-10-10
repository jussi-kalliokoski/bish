// `Scheduler::add` is used only by the tests.
#![allow(dead_code)]

// Runs several coroutines to completion, switching when one can't make
// progress.
//
// `coroutine` can stop one execution and start another; this decides
// when. A pipeline stage that filled the pipe it writes, or emptied the
// one it reads, hands the thread to whichever stage can unblock it. No
// fairness and no preemption: a stage only stops to wait for a
// descriptor, and only another stage can help.
//
// So: run whatever can run, and when nothing can, sleep in `poll(2)`
// until one can, so two blocked stages leave the process idle rather
// than spinning.

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
    /// Set by the `park_*` calls before yielding, read by the scheduler
    /// after. A side channel, as the yield can happen deep inside an
    /// evaluator that doesn't know it's a coroutine, so the shell's I/O
    /// paths park without every caller threading a reason out.
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

thread_local! {
    /// Whether a scheduler is already driving a coroutine on this thread.
    ///
    /// Both entry points below record fd 0 and fd 1 as the shell's own, to
    /// install per task and restore at the end. Nested inside another, one
    /// would record a task's pipes as the shell's, hand them to tasks with
    /// none of their own and restore them at the end, leaving the shell
    /// writing into a stage's pipe and hanging.
    ///
    /// It happens when a stage pumps: `Shell::pump_coroutines` reaches for
    /// the shared background scheduler, a different object, so borrowing
    /// doesn't exclude it. Nesting is never wanted: the outer scheduler is
    /// already giving this thread's coroutines turns.
    static DRIVING: Cell<bool> = const { Cell::new(false) };
}

/// `true` if this call took the guard, and so must give it back.
fn take_driving() -> bool {
    !DRIVING.replace(true)
}

fn release_driving() {
    DRIVING.set(false);
}

struct Task {
    co: Box<Coroutine>,
    park: Park,
    /// This task's fd 0 and fd 1, installed for each of its turns.
    ///
    /// A stage gets its pipes as its real stdin and stdout, so `exec
    /// {fd}<&0`, an external inheriting the rest of the input, or a builtin
    /// writing to fd 1 all work. The descriptors are swapped rather than the
    /// code using them: two `dup2`s per switch, and switches happen only when
    /// a stage blocks.
    stdin: Option<std::os::fd::OwnedFd>,
    stdout: Option<std::os::fd::OwnedFd>,
    /// This task's broken-pipe flag, swapped in around each resume; see
    /// `exec::swap_broken_pipe` for why it can't be the thread's.
    broken: Option<bool>,
    /// Set when nothing will read this task's output; it's never resumed
    /// again (see `cancel_running`).
    cancelled: bool,
    /// Whether `cancel_running` may stop this one. A `<( )` producer may:
    /// once its reader finished, more output goes nowhere. A `>( )` consumer
    /// may not: when the command ends it has just been handed its input and
    /// hasn't written its answer yet.
    cancellable: bool,
}

impl Task {
    /// The descriptor a parked task meant. A stage parks on the number it
    /// used (0 for input, 1 for output), which means whatever was installed
    /// last, another stage's pipe once one runs. Waiting on the number
    /// could wait on one always ready (a spin) or one never ready (a hang).
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
        self.tasks.push(Task {
            co: Coroutine::new(body)?,
            park: Park::Ready,
            broken: None,
            cancelled: false,
            cancellable: true,
            stdin: None,
            stdout: None,
        });
        Ok(())
    }

    /// Adds a coroutine with `stdin`/`stdout` as its fd 0 and fd 1 (see
    /// `Task::stdin`). `None` leaves the shell's own.
    pub fn add_with_fds(
        &mut self,
        body: impl FnOnce() + 'static,
        stdin: Option<std::os::fd::OwnedFd>,
        stdout: Option<std::os::fd::OwnedFd>,
        cancellable: bool,
    ) -> std::io::Result<()> {
        self.tasks.push(Task { co: Coroutine::new(body)?, park: Park::Ready, broken: None, cancelled: false, cancellable, stdin, stdout });
        Ok(())
    }

    /// Whether anything is still running.
    pub fn is_idle(&self) -> bool {
        self.tasks.iter().all(|t| t.co.state() == State::Done || t.cancelled)
    }

    /// Gives each runnable coroutine one turn and returns, for constructs
    /// outliving their command: a `<( )` producer must progress while the
    /// shell waits for the command consuming it, outside `run`. Called where
    /// the shell would otherwise sit idle (see `Shell::pump_coroutines`).
    ///
    /// Never blocks, unlike `run`: the caller has its own reason to return.
    pub fn step(&mut self) {
        if self.is_idle() || !take_driving() {
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
                // See `run_tasks`: the copies on fd 0 and fd 1 count too.
                self.tasks[index].stdin = None;
                self.tasks[index].stdout = None;
                install_shell_fds(saved);
            }
        }
        crate::exec::restore_fd012_for_scheduler(saved);
        release_driving();
    }

    /// Stops every task still running: nothing will read its output. What
    /// the shell does once the command consuming a `<( )` finished. Stopped
    /// rather than its descriptor closed: `None` for `stdout` means the
    /// shell's own, so a producer kept going would write to the terminal
    /// (`head -3 <(while true; do echo x; done)` printed x's forever).
    ///
    /// A cancelled task is never resumed, and `retire_finished` drops it and
    /// unmaps its stack. What the stack owned is leaked (see `Coroutine`'s
    /// `Drop`): the price of stopping something midway, which a process gets
    /// free from the kernel.
    pub fn cancel_running(&mut self) {
        for task in self.tasks.iter_mut().filter(|t| t.co.state() != State::Done && t.cancellable) {
            task.cancelled = true;
            task.stdin = None;
            task.stdout = None;
        }
    }

    /// Drops finished tasks, so a long-lived shell doesn't accumulate them.
    pub fn retire_finished(&mut self) {
        self.tasks.retain(|t| t.co.state() != State::Done && !t.cancelled);
    }

    /// Runs every coroutine until all have finished. Returns whether any
    /// panicked, for the caller to decide about.
    pub fn run(&mut self) -> bool {
        if !take_driving() {
            return false;
        }
        // Restored before returning and while sleeping in `poll`: the shell
        // owns fd 0 and fd 1 whenever no task runs on them.
        let saved = crate::exec::save_fd012_for_scheduler();
        let panicked = self.run_tasks(saved);
        crate::exec::restore_fd012_for_scheduler(saved);
        release_driving();
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
                // Cleared before resuming, so a coroutine that finishes
                // rather than parks doesn't leave a stale reason.
                REASON.with(|r| r.set(Park::Ready));
                let outer = crate::exec::swap_broken_pipe(self.tasks[index].broken);
                install_fds(&self.tasks[index], shells_own);
                self.tasks[index].co.resume();
                self.tasks[index].broken = crate::exec::swap_broken_pipe(outer);
                self.tasks[index].park = REASON.with(|r| r.get());
                if self.tasks[index].co.state() == State::Done {
                    // A finished stage must let go of its pipes, or the
                    // stage on the other end waits on something kept alive
                    // here. Three references: the task's pair and the
                    // copies `install_fds` left on fd 0 and fd 1. Those are
                    // replaced only when another task is installed, after
                    // `can_run` asks, so a finished reader's fd 0 kept its
                    // pipe looking read and the producer was never woken.
                    self.tasks[index].stdin = None;
                    self.tasks[index].stdout = None;
                    install_shell_fds(shells_own);
                }
                ran_something = true;
            }
            if ran_something {
                continue;
            }
            // Everything is waiting for a descriptor, maybe the pane's own
            // pty filling, which only this thread empties (see
            // `exec::PIPELINE_DRAIN`).
            crate::exec::drain_pipeline_output();
            if !self.wait_for_any() {
                // Nothing to wait for and nothing ran: nothing can become
                // ready. One more run turns a hang into whatever they do
                // about a closed pipe.
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
        // One at a time is enough for a handful of stages: each wait ends
        // when ready or on the timeout, then the rest are tried.
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

/// Puts fd 0 and fd 1 back to the shell's own, releasing what a task had
/// installed.
fn install_shell_fds(shells_own: [i32; 3]) {
    crate::platform::duplicate_onto(shells_own[0], 0);
    crate::platform::duplicate_onto(shells_own[1], 1);
}

/// Points fd 0 and fd 1 at this task's pipes for its turn.
fn install_fds(task: &Task, shells_own: [i32; 3]) {
    use std::os::fd::AsRawFd;
    // Both, every time, and `None` means the shell's own, not "leave it":
    // otherwise the last stage of `a | b` would write into `a`'s pipe.
    let stdin = task.stdin.as_ref().map(|f| f.as_raw_fd()).unwrap_or(shells_own[0]);
    let stdout = task.stdout.as_ref().map(|f| f.as_raw_fd()).unwrap_or(shells_own[1]);
    crate::platform::duplicate_onto(stdin, 0);
    crate::platform::duplicate_onto(stdout, 1);
}

/// How long to wait on one descriptor before trying the next. Reached
/// only when every stage is blocked, and ends as soon as it's ready.
const POLL_SLICE_MS: i32 = 20;

/// Whether `fd` can be written within `timeout_ms`.
fn poll_writable(fd: RawFd, timeout_ms: i32) -> bool {
    use crate::platform::{POLL_ERROR, POLL_HUNG_UP, POLL_WRITABLE, PollFd};
    let mut watched = [PollFd::watching(fd, POLL_WRITABLE)];
    // A pipe whose reader is gone reports `POLLERR`, not `POLLOUT`, and its
    // next write fails with `EPIPE`, which is how a producer learns to stop.
    // Waiting for `POLLOUT` alone hung
    // `while true; do echo x; done | { read v; echo "$v"; }`.
    crate::platform::wait_for_ready(&mut watched, timeout_ms).is_ok_and(|ready| ready > 0)
        && watched[0].reported() & (POLL_WRITABLE | POLL_ERROR | POLL_HUNG_UP) != 0
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

    // One coroutine can't finish until another acts, and the scheduler
    // works that out from the descriptors.
    #[test]
    fn a_reader_waits_for_a_writer_through_a_real_pipe() {
        let (read_end, write_end) = crate::exec::make_pipe_for_test().unwrap();
        crate::pty::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&read_end));
        let got = Rc::new(std::cell::RefCell::new(String::new()));

        let mut s = Scheduler::new();
        // The reader goes first and blocks at once: the writer hasn't run.
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

    // A writer filling the pipe hands over to the reader rather than spin,
    // and together they move more than a pipe buffer.
    #[test]
    fn a_writer_blocked_on_a_full_pipe_hands_over_to_the_reader() {
        let (read_end, write_end) = crate::exec::make_pipe_for_test().unwrap();
        crate::pty::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&read_end));
        crate::pty::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&write_end));
        let read_fd = std::os::fd::AsRawFd::as_raw_fd(&read_end);
        let write_fd = std::os::fd::AsRawFd::as_raw_fd(&write_end);
        // More than any pipe buffer, so the writer must block on the reader.
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
