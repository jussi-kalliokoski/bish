use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::rc::Rc;

use crate::arith;
use crate::bishedit::highlight;
use crate::bishedit::snippet::Abbr;
use crate::builtins;
use crate::compgen;
use crate::glob;
use crate::lexer::{Chunk, ReplaceAnchor, TransformKind, VarOp};
use crate::parser::{
    self, AndOr, ArrayLiteralItem, AssignMode, Combinator, ListItem, Pipeline, Program, Redirect, Sep, SimpleCommand, TimeStyle, Word,
};
use crate::pty;
use crate::vt100;

// Where a Shell's own output (builtin/diagnostic text -- never a spawned
// external process's, which always writes straight to its real inherited
// fds regardless) actually goes. `Real` -- writing directly to the
// process's real stdout/stderr -- is what every session uses before
// promotion, and after promotion for whichever session is momentarily
// mid-transition. `Grid` -- feeding bytes into that session's own VT100
// Screen (M8) -- is what every session uses once the M9 compositor is
// driving the terminal: repl.rs is the only thing that can see the real
// window list, so it's the one that flips a session's sink from Real to
// Grid (see repl.rs's apply_window_action) and later reads the Grid back
// out to render whichever window is currently focused.
#[derive(Clone)]
pub(crate) enum OutputSink {
    Real,
    Grid(Rc<RefCell<vt100::Screen>>),
    // repl.rs's command mode temporarily swaps a session's sink to this
    // while running one command, so it can show that command's own
    // combined stdout+stderr text as a dedicated overlay (see run_
    // command_mode) instead of it landing mixed into the pane's own
    // grid/scrollback. No onlcr translation here (unlike Grid) -- this
    // is plain text repl.rs renders itself by splitting on '\n', not fed
    // into a terminal emulator that needs CRLF.
    Capture(Rc<RefCell<String>>),
    // dispatch_builtin_or_external's own push_builtin_output_sink installs
    // this around a single builtin call so its sh_println!/sh_eprintln!
    // writes honor *that command's own* `>`/`>>`/`2>`/`&>`/`2>&1`/`1>&2`
    // redirects -- previously builtins ignored per-command redirects
    // entirely (see plan.md), which mattered for real activation scripts
    // (mise, nvm, ...) relying on `declare -p foo >/dev/null 2>&1`-style
    // guards being silent. Each stream says where it goes; a stream with
    // no redirect of its own says so by naming the enclosing sink's
    // matching side.
    Builtin { previous: Box<OutputSink>, stdout: SinkStream, stderr: SinkStream },
}

// Where one of a redirected builtin's two streams ends up.
//
// A dup names the descriptor *as the command found it*, which is the
// whole reason this is three cases and not a file plus a "follows the
// other one" flag: in
//
//     echo e >&2 2>/dev/null
//
// `>&2` copies fd 2 before the `2>` rebinds it, so the line still
// prints on the terminal. A flag can only point at this sink's own
// stderr field, which by then is /dev/null -- the output vanished.
// Naming the *enclosing* sink instead keeps the two apart.
#[derive(Clone)]
pub(crate) enum SinkStream {
    // The enclosing sink's stdout: no redirect of its own, or a `2>&1`
    // whose fd 1 this command never rebound.
    OuterOut,
    // The enclosing sink's stderr, likewise.
    OuterErr,
    // A file this command's own redirects opened. Two streams that end
    // up on one destination hold the same `Rc`, and so share a write
    // position -- `>file 2>&1` must not have them overwrite each other.
    File(Rc<RefCell<std::fs::File>>),
}

// Emulates the real terminal's ONLCR postprocessing (translating outgoing
// LF into CRLF) for text landing in a session's Grid. Every existing
// sh_println!-style call site was written assuming a directly-connected,
// default-cooked-mode terminal, where the OS/tty layer inserts the \r
// invisibly; vt100::Screen deliberately does NOT do this itself (real
// VT100 semantics: LF moves the cursor down only, CR is separate -- M9b's
// raw pty byte streams from curses apps need that distinction preserved,
// since those programs explicitly manage when to emit \r). This bridges
// the gap only at the Grid sink boundary, leaving both ends correct for
// what they actually receive.
fn onlcr(s: &str) -> String {
    if !s.contains('\n') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut prev = '\0';
    for c in s.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    out
}

impl OutputSink {
    fn write_out(&self, s: &str) {
        match self {
            // Deliberately not the print! macro (and not sh_print! either
            // -- this is the base case every sh_print!/sh_println!/etc
            // call bottoms out into via Shell::sink_out; routing it back
            // through the sink macro here would recurse forever). print!
            // panics on a write error, which happens for real the moment
            // output is piped into something that closes its end early
            // (`ulimit -a | head -3`, `jobs | head -1`, ...) -- every
            // builtin's multi-line output funnels through here now, so
            // this is the one place that needs to treat a broken pipe as
            // "stop writing, not fatal" instead of crashing the process.
            OutputSink::Real => {
                // Straight to the descriptor rather than through
                // `std::io::stdout()`. Its buffering hides the one
                // condition that matters here: a write into a pipeline
                // stage's own full pipe returns from the buffer with
                // `Ok`, and the `EAGAIN` surfaces later inside `flush`,
                // where the error was dropped and the bytes with it.
                // The sink flushes after every write anyway, so the
                // buffer was never buying anything.
                note_write(write_fd_parking(1, s.as_bytes()));
                // Flushed here rather than left to the line buffer,
                // because fd 1 is not this shell's alone: the next
                // thing to write to it is very often a child process,
                // and a half-line still sitting in `Stdout`'s buffer
                // comes out *after* what the child wrote.
                // `printf A; printf B | cat; printf C` printed `BAC`.
                //
                // A line-oriented write already costs one syscall, so
                // this only adds one where output stops mid-line.
                // Measured on 20,000 partial writes into /dev/null --
                // the case buffering would help most -- 2.156s against
                // 2.146s, which is noise: the loop is the interpreter's
                // time, not the kernel's. Flushing before every spawn
                // instead would keep the buffering, at the cost of
                // having to find every place a child can start.
            }
            OutputSink::Grid(screen) => screen.borrow_mut().feed(onlcr(s).as_bytes()),
            OutputSink::Capture(buf) => buf.borrow_mut().push_str(s),
            OutputSink::Builtin { previous, stdout, .. } => match stdout {
                SinkStream::OuterOut => previous.write_out(s),
                SinkStream::OuterErr => previous.write_err(s),
                SinkStream::File(f) => note_write(write_all_parking(&mut f.borrow_mut(), s.as_bytes())),
            },
        }
    }

    fn write_err(&self, s: &str) {
        match self {
            OutputSink::Real => {
                use std::io::Write;
                let _ = std::io::stderr().write_all(s.as_bytes());
            }
            OutputSink::Grid(screen) => screen.borrow_mut().feed(onlcr(s).as_bytes()),
            OutputSink::Capture(buf) => buf.borrow_mut().push_str(s),
            OutputSink::Builtin { previous, stderr, .. } => {
                use std::io::Write;
                match stderr {
                    SinkStream::OuterOut => previous.write_out(s),
                    SinkStream::OuterErr => previous.write_err(s),
                    SinkStream::File(f) => {
                        let _ = f.borrow_mut().write_all(s.as_bytes());
                    }
                }
            }
        }
    }
}

// A Shell's own idea of "what should an external command's stdin/stdout
// default to when nothing more specific redirects it" -- consulted at
// every `Stdio::inherit()` fallback site (spawn_stdin_stdio/
// spawn_stdout_stdio below) instead of calling `Stdio::inherit()`
// directly. Only meaningful for a Shell created by run_in_child_shell
// (a foreground subshell/command-substitution/proc-sub now running
// in-process instead of self-exec'ing -- see its own doc comment): once
// that construct's own external commands stop being real *grandchild*
// processes of a real re-exec'd child (which naturally inherited the
// right fds for free, since `.output()`/`.status()`'s own pipe/redirect
// was that child's real fd 1), something has to keep telling them where
// their default stdio actually goes. `None` (the ordinary case for every
// other Shell) means "really inherit the real process's own fds",
// exactly matching plain `Stdio::inherit()`.

// Shell::run_in_child_shell's own redirect/capture argument -- see its own
// doc comment. `Default` (every field `None`/`false`) means "this
// construct has no redirect of its own," inheriting the parent's current
// output/input destination exactly.
#[derive(Default)]
struct ChildStdio {
    stdout: Option<std::fs::File>,
    stdin: Option<std::fs::File>,
    stderr: Option<std::fs::File>,
    // `2>&1` / `1>&2`: which stream follows the other, and what it was
    // following at the point the dup appeared. A descriptor names what
    // it names right then, so the two are different destinations:
    // `{ echo e; } >&2 2>/dev/null` prints on the terminal, because
    // `>&2` copied fd 2 before the `2>` rebound it, while
    // `{ echo e; } 2>/dev/null >&2` prints nothing.
    out_follows_err: Option<Follows>,
    err_follows_out: Option<Follows>,
}

// What a `2>&1`/`1>&2` was pointing at when it appeared.
#[derive(Clone, Copy)]
enum Follows {
    // The file this same command had already opened for the other
    // stream.
    OwnFile,
    // The enclosing shell's descriptor for it -- the dup came first.
    Outer,
}

impl ChildStdio {
    // Does this construct redirect either output stream at all? When it
    // doesn't, the child's sink should be *exactly* the parent's rather
    // than a fresh wrapper around it.
    fn redirects_output(&self) -> bool {
        self.stdout.is_some() || self.stderr.is_some() || self.out_follows_err.is_some() || self.err_follows_out.is_some()
    }

    fn sink_stdout(&self) -> SinkStream {
        match self.out_follows_err {
            Some(Follows::Outer) => SinkStream::OuterErr,
            Some(Follows::OwnFile) => file_stream(&self.stderr).unwrap_or(SinkStream::OuterErr),
            None => file_stream(&self.stdout).unwrap_or(SinkStream::OuterOut),
        }
    }

    fn sink_stderr(&self) -> SinkStream {
        match self.err_follows_out {
            Some(Follows::Outer) => SinkStream::OuterOut,
            Some(Follows::OwnFile) => file_stream(&self.stdout).unwrap_or(SinkStream::OuterOut),
            None => file_stream(&self.stderr).unwrap_or(SinkStream::OuterErr),
        }
    }
}

// A second handle on the same open file. `try_clone` is a dup, so the
// two share one write position -- which is the point wherever both
// streams land on one destination.
fn file_stream(f: &Option<std::fs::File>) -> Option<SinkStream> {
    f.as_ref()?.try_clone().ok().map(|f| SinkStream::File(Rc::new(RefCell::new(f))))
}

/// What a virtual child is being asked to run: text that still has to
/// be lexed and parsed, or a command that already has been.
enum ChildBody<'a> {
    Source(&'a str),
    Parsed(&'a parser::Command),
}

struct StdioOverride {
    // `Some` => read from here (a real, shared, sequentially-consumed
    // reader) instead of the real stdin -- see SharedReaderState's own
    // doc comment for why this needs more than a bare File.
    stdin: Option<Rc<RefCell<SharedReaderState>>>,
    // `Some` => write here instead of the real stdout.
    stdout: Option<std::fs::File>,
    // And the same for stderr. Without it a construct's `2>` reached
    // the builtins inside it, through the output sink, and nothing
    // else: `{ /bin/ls /nosuch; } 2>/dev/null` still printed the error,
    // because an external command's stderr was always inherited.
    stderr: Option<std::fs::File>,
}

// Backs a converted construct's stdin override (see StdioOverride's own
// doc comment): the real File, plus whatever's already been pulled from
// it via a real read() syscall but not yet consumed by a BufRead::
// consume() call. `read_input_source`'s own doc comment explains why a
// fresh, throwaway BufReader on every call would silently drop
// read-ahead bytes between separate `read` builtin invocations in the
// same `while read` loop -- this is that same problem, solved the same
// way (one persistent buffer, reused across calls) for the case where
// the source is this override rather than the real process's own stdin.
struct SharedReaderState {
    file: std::fs::File,
    pending: Vec<u8>,
    /// So a read from here can give a live coroutine a turn instead of
    /// blocking on bytes only this thread can produce -- `while read
    /// ... done < <(cmd)` names a pipe whose far end is one.
    coroutines: Rc<RefCell<crate::scheduler::Scheduler>>,
}

// A thin, freshly-built-per-call BufRead over a SharedReaderState --
// read_input_source hands one of these out each time it's called, but
// they all share (and correctly hand back and forth) the same
// underlying pending-bytes buffer, so a sequence of separate calls
// behaves like one persistent reader. Needed (rather than just handing
// out `Rc<RefCell<SharedReaderState>>` directly wrapped in a
// std::io::BufReader) because BufRead::fill_buf must return a `&[u8]`
// borrowed from `&mut self` -- a RefCell borrow can't be smuggled out
// through that signature, so this owns a local copy of "the currently
// available slice" instead and reconciles it against the shared state
// on fill/consume/drop.
struct SharedStdinReader {
    state: Rc<RefCell<SharedReaderState>>,
    local: Vec<u8>,
}

impl std::io::Read for SharedStdinReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        use std::io::BufRead;
        let buf = self.fill_buf()?;
        let n = buf.len().min(out.len());
        out[..n].copy_from_slice(&buf[..n]);
        self.consume(n);
        Ok(n)
    }
}

/// `write_all`, except that a descriptor with no room hands the thread
/// to another pipeline stage instead of failing.
///
/// Only a pipe between two stages running in this process is ever
/// non-blocking, so outside one of those this loop takes its first
/// branch every time and behaves exactly like `write_all`. Inside one,
/// `WouldBlock` means the reader is behind, and the reader is another
/// coroutine -- so the answer is to let it run, not to error.
/// Runs `op` with `fd` non-blocking, and puts the flag back.
///
/// `O_NONBLOCK` lives on the open file *description*, which everything
/// that inherits the descriptor shares -- so leaving it set on a
/// pipeline stage's pipe made every external command that stage
/// spawned fail with `Resource temporarily unavailable`. Setting it
/// only around this shell's own read and write means it is never set
/// at the moment a child is forked, which is the only moment that
/// matters. Nothing else can run in between: a stage yields at a park,
/// never mid-syscall.
///
/// Only inside a coroutine, where a blocking descriptor would take the
/// whole thread down with it. Everywhere else this is the bare `op`.
fn briefly_nonblocking<T>(fd: i32, op: impl FnOnce() -> T) -> T {
    if !crate::coroutine::in_coroutine() {
        return op();
    }
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    }
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    const O_NONBLOCK: i32 = 0o4000;
    let flags = unsafe { fcntl(fd, F_GETFL, 0) };
    if flags < 0 {
        return op();
    }
    unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) };
    let out = op();
    unsafe { fcntl(fd, F_SETFL, flags) };
    out
}

/// A write into a redirect target that failed. A broken pipe is the
/// one worth remembering -- it is how an in-process stage learns its
/// reader is gone, since it cannot be told by SIGPIPE. Everything else
/// is dropped, as it was before there was anywhere to put it.
fn note_write(result: std::io::Result<()>) {
    if result.is_err_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) {
        note_broken_pipe();
    }
}

pub(crate) fn write_all_parking(file: &mut std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    write_fd_parking(file.as_raw_fd(), bytes)
}

/// How many turns a `<( )` producer gets to notice that nobody is
/// reading it any more, once the command using it has finished.
const PROC_SUB_WINDDOWN_TURNS: usize = 8;

/// A file this shell reads, which gives any live coroutine a turn
/// rather than blocking in the kernel while one is waiting to run.
///
/// The case that needs it is `read v < <(echo hello)`: the name is a
/// pipe a coroutine is on the far end of, and a plain blocking read
/// here would wait for bytes that only this thread can produce. Where
/// nothing is live -- every ordinary `< file` -- this is a bare read
/// with one `poll` in front of it.
struct PumpingFile {
    file: std::fs::File,
    coroutines: Rc<RefCell<crate::scheduler::Scheduler>>,
}

impl std::io::Read for PumpingFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        let fd = self.file.as_raw_fd();
        loop {
            if crate::poll::poll_readable_or_eof(fd, 0) {
                return self.file.read(buf);
            }
            let Ok(mut scheduler) = self.coroutines.try_borrow_mut() else {
                return self.file.read(buf);
            };
            if scheduler.is_idle() {
                // Nothing can produce anything, so waiting in the
                // kernel is right after all.
                drop(scheduler);
                return self.file.read(buf);
            }
            scheduler.step();
        }
    }
}

/// `write_all_parking` straight onto a descriptor.
fn write_fd_parking(fd: i32, bytes: &[u8]) -> std::io::Result<()> {
    unsafe extern "C" {
        fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    }
    let mut written = 0;
    while written < bytes.len() {
        let n = briefly_nonblocking(fd, || unsafe { write(fd, bytes[written..].as_ptr(), bytes.len() - written) });
        if n > 0 {
            written += n as usize;
            continue;
        }
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            std::io::ErrorKind::WouldBlock => crate::scheduler::park_writable(fd),
            std::io::ErrorKind::Interrupted => {}
            _ => return Err(e),
        }
    }
    // The write succeeded, so whatever is downstream now has something
    // to read: hand it the thread before writing more.
    //
    // Nothing else here ever would. A stage only gives the thread up
    // when it *blocks*, and a producer whose reader is keeping up never
    // blocks -- so `while true; do echo x; done | { read a; ...; }` ran
    // the producer 32768 times, until the pipe buffer filled, before
    // the reader got its first turn. Separate processes do not behave
    // that way, because the kernel preempts them; a cooperative
    // scheduler has to be told where the fair point is, and the moment
    // the data becomes readable is it.
    if crate::coroutine::in_coroutine() {
        crate::scheduler::park_ready();
    }
    Ok(())
}

/// `read`, with the same treatment: nothing to read yet means another
/// stage has not written it yet, and that stage is a coroutine.
fn read_parking(file: &mut std::fs::File, buf: &mut [u8], coroutines: &Rc<RefCell<crate::scheduler::Scheduler>>) -> std::io::Result<usize> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    loop {
        // Inside a pipeline stage: hand the thread back to the
        // scheduler, which is above this on the stack.
        if crate::coroutine::in_coroutine() {
            match briefly_nonblocking(fd, || file.read(buf)) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => crate::scheduler::park_readable(fd),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                other => return other,
            }
            continue;
        }
        // The shell itself, reading something a coroutine is feeding.
        // Blocking here would wait for bytes only this thread can
        // produce.
        if crate::poll::poll_readable_or_eof(fd, 0) {
            return file.read(buf);
        }
        let Ok(mut scheduler) = coroutines.try_borrow_mut() else {
            return file.read(buf);
        };
        if scheduler.is_idle() {
            drop(scheduler);
            return file.read(buf);
        }
        scheduler.step();
    }
}

impl std::io::BufRead for SharedStdinReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.local.is_empty() {
            let mut state = self.state.borrow_mut();
            if state.pending.is_empty() {
                let mut tmp = [0u8; 8192];
                let state = &mut *state;
                let n = read_parking(&mut state.file, &mut tmp, &state.coroutines)?;
                state.pending.extend_from_slice(&tmp[..n]);
            }
            std::mem::swap(&mut self.local, &mut state.pending);
        }
        Ok(&self.local)
    }

    fn consume(&mut self, amt: usize) {
        self.local.drain(..amt.min(self.local.len()));
    }
}

impl Drop for SharedStdinReader {
    fn drop(&mut self) {
        // Hand back whatever this call fetched but never consumed (e.g.
        // read_until found its delimiter partway through a bulk read),
        // so the *next* SharedStdinReader -- the next `read` call in the
        // same loop -- picks up exactly where this one left off.
        if !self.local.is_empty() {
            let mut state = self.state.borrow_mut();
            let mut rest = std::mem::take(&mut self.local);
            rest.append(&mut state.pending);
            state.pending = rest;
        }
    }
}

// Mirror std's print!/println!/eprint!/eprintln! but route through a
// Shell's own sink instead of unconditionally going straight to the real
// process stdout/stderr. `$self` needs `sink_out`/`sink_err` methods
// (Shell has them); forwarding the raw `$($arg)*` tokens straight into
// the real `format!` is what keeps this a faithful, low-risk mechanical
// swap at every call site -- every existing format string/argument list
// still means exactly what it did before, only the destination changes.
macro_rules! sh_print {
    ($self:expr, $($arg:tt)*) => {
        $self.sink_out(&format!($($arg)*))
    };
}
pub(crate) use sh_print;

macro_rules! sh_println {
    ($self:expr) => {
        $self.sink_out("\n")
    };
    ($self:expr, $($arg:tt)*) => {{
        let mut s = format!($($arg)*);
        s.push('\n');
        $self.sink_out(&s);
    }};
}
pub(crate) use sh_println;

macro_rules! sh_eprint {
    ($self:expr, $($arg:tt)*) => {
        $self.sink_err(&format!($($arg)*))
    };
}

macro_rules! sh_eprintln {
    ($self:expr) => {
        $self.sink_err("\n")
    };
    ($self:expr, $($arg:tt)*) => {{
        let mut s = format!($($arg)*);
        s.push('\n');
        $self.sink_err(&s);
    }};
}
pub(crate) use sh_eprintln;

#[derive(Debug, Clone)]
pub enum ExecResult {
    Status(i32),
    Break(u32),
    Continue(u32),
    Return(i32),
    // `window`-family command result. Bubbles up through run_and_or/
    // run_pipeline/run_program exactly like Break/Continue/Return, all the
    // way to whoever called run_program -- repl.rs, which is the only
    // thing that actually owns the session/window collection. This is
    // deliberate, not an oversight: a session's own Shell can't hold a
    // reference to "all sessions" (itself included) without creating an
    // Rc<RefCell<_>> self-reference cycle that would panic the moment
    // running code tried to borrow it while already borrowed one level up
    // the call stack. Threading the action through the same signal
    // mechanism control flow already uses avoids that entirely: run_window
    // (below) only validates the request and returns it, never touches
    // shared window state itself.
    Window(WindowAction),
    // `fg` on a job that was spawned pty-attached (see Job::pty_master)
    // while promoted. Bubbles up exactly like Window, for exactly the
    // same reason: rendering the job's output into the fg-ing window's
    // grid and redrawing the real terminal both need repl.rs's own
    // session/window state, which a session's own Shell can't hold a
    // reference to (see Window's doc comment). run_fg only removes the
    // job from the table and stashes it on `self.pending_fg`; repl.rs
    // reacts to this signal by calling Shell::take_pending_fg to get an
    // owned FgJob handle, pushing it as a Frame::Job on the current
    // window's stack, and driving it directly (repl.rs's drive_fg_job) --
    // unlike the Window action, this one hands over real ownership
    // rather than just a callback, since the Frame stack needs to keep
    // referencing the job across main-loop iterations, not just for the
    // duration of one call.
    Fg,
    // `e [ARG...]`. Bubbles up exactly like Fg, for exactly the same
    // reason: a builtin has no raw-mode/keystroke/rendering access of
    // its own, and `Registers` lives in `repl::run`'s own locals, not in
    // `Shell` at all. Carries no payload itself (`ExecResult` stays
    // `Copy`, same as every other variant here -- an owned argument
    // vector wouldn't be) -- `run_single` stashes the actual arguments
    // on `self.pending_edit` first, same as `run_fg` already does via
    // `pending_fg`; repl.rs reacts to this signal by calling
    // `Shell::take_pending_edit` to get them back out.
    Edit,
    // `exit`, a failing statement under `set -e`, a `set -u` violation,
    // or a failed `exec` -- a request to terminate *this Shell's own
    // top-level run*, not necessarily the real OS process. Bubbles up
    // through run_and_or/run_pipeline/run_program exactly like the other
    // signal variants; whoever called the outermost run_program decides
    // what "terminate" means: the real top level (main.rs's run_source,
    // repl.rs's two run_program call sites) turns this into a genuine
    // std::process::exit (matching this codebase's prior behavior, where
    // these four sites called std::process::exit directly, unconditionally
    // killing the whole process no matter which session/pane triggered
    // it); `run_in_child_shell` (a foreground subshell/command-substitution/
    // proc-sub running in-process, see its own doc comment) instead
    // unwraps this as *that child's own* exit status, matching real bash's
    // fork isolation for `(exit 3)` -- the enclosing real shell must not
    // die just because a subshell called `exit`. The exit trap has
    // already been run by whichever site produced this (see e.g.
    // run_program's own errexit arm) -- nothing downstream should run it
    // again.
    Exit(i32),
}

impl ExecResult {
    fn status(&self) -> i32 {
        match self {
            ExecResult::Status(s) => *s,
            ExecResult::Return(s) => *s,
            ExecResult::Exit(s) => *s,
            ExecResult::Break(_) | ExecResult::Continue(_) => 0,
            ExecResult::Window(_) | ExecResult::Fg | ExecResult::Edit => 0,
        }
    }

    fn is_signal(&self) -> bool {
        matches!(
            self,
            ExecResult::Break(_)
                | ExecResult::Continue(_)
                | ExecResult::Return(_)
                | ExecResult::Window(_)
                | ExecResult::Fg
                | ExecResult::Edit
                | ExecResult::Exit(_)
        )
    }
}

// What a paused DebugHook decided to do next -- see DebugHook's own doc
// comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugAction {
    // Keep running until the next breakpoint/step condition.
    Continue,
    // Stop at the next statement at this *same or shallower* depth --
    // i.e. don't stop again just because execution descended into a
    // function call or a converted foreground subshell/command-
    // substitution in the meantime.
    StepOver,
    // Stop at the very next statement, regardless of depth.
    StepInto,
    // Unwind the whole running program, same as a real `exit` (see
    // ExecResult::Exit) -- used to abandon a debug run entirely (`q` in
    // the debugger UI) without killing the real process.
    Quit,
}

// Where the interpreter currently is, for DebugHook::on_statement's own
// step-over/step-into bookkeeping. Compared subshell_depth first, then
// call_depth: without the subshell half, a StepOver issued in the parent
// at its own top level (subshell_depth 0, call_depth 0) could otherwise
// be fooled by a converted foreground subshell/command-substitution
// starting its own run_program at call_depth 0 too, aliasing two
// genuinely different "frames" together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DebugDepth {
    pub subshell_depth: u32,
    pub call_depth: usize,
}

// Called once per top-level ListItem, right before run_program actually
// runs it (see run_program's own call site) -- the single hook point
// every executed statement passes through uniformly, at every nesting
// depth (if/while/for/case/function bodies all recurse back through
// run_program). Defined here (using only exec.rs-level types) rather
// than in the concrete debugger UI module, so exec.rs doesn't gain a
// dependency on repl.rs/bishedit -- the concrete implementation (owning
// breakpoints, rendering, blocking key-reads) lives on the other side of
// this trait instead, depending on exec.rs, not the reverse.
pub trait DebugHook {
    // `shell: &mut Shell`, not `&Shell` -- deciding whether to keep
    // running needs write access, not just a read-only variable peek:
    // when the answer is "let this statement run for real, uninterrupted"
    // (no pause), the hook needs to actually hand the real terminal over
    // to it (DebugController::hand_off_to_script -- Shell::set_sink_real,
    // so e.g. `read -p`'s own prompt shows up immediately instead of
    // sitting invisibly captured until long after the moment it
    // mattered) and reclaim it again once it's this hook's turn to
    // decide again.
    fn on_statement(&mut self, line: usize, depth: DebugDepth, shell: &mut Shell) -> DebugAction;
}

/// One window, as the `window` builtin sees it. A flat snapshot rather
/// than a borrow of the real thing: `exec.rs` has no access to
/// `repl.rs`'s window state and shouldn't grow one (see
/// `ExecResult::Window`), and everything `ls` prints is a plain value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u32,
    pub name: Option<String>,
    pub cwd: String,
    pub panes: usize,
    pub current: bool,
}

// What repl.rs should do in response to a `window`-family command -- see
// ExecResult::Window's doc comment for why this travels as a bubbled
// signal instead of direct shared-state mutation.
/// One declared theme: the bishopts it sets, and the highlight colours
/// it sets. Two maps because they are two namespaces -- `::bish hl`
/// names are open, bishopt names are a fixed registry -- but one type,
/// because a theme is a single thing you switch to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Theme {
    pub(crate) opts: std::collections::HashMap<String, BishOptValue>,
    pub(crate) hl: std::collections::HashMap<String, String>,
}

/// Whatever a shell's output sink was before something borrowed it --
/// opaque, so `OutputSink` itself stays private.
pub(crate) struct SavedSink(OutputSink);

/// One registered hook: what to run, when, and for which languages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    /// Assigned from a per-shell counter and never reused, so `rm`
    /// always names the thing `ls` showed.
    pub id: u64,
    pub event: String,
    /// `--lang=`: a glob matched against the language of the file the
    /// event is about (`fileeditor::language_of`), the same shape and
    /// the same matcher `abbr --lang` uses -- so `--lang='*script'`
    /// covers a family and `--lang='!(bash)'` covers everything else.
    /// `*` when unscoped.
    pub lang: String,
    /// The command line to run, with the file's path appended as its
    /// first argument.
    pub command: String,
}

/// A table of long-running helper processes the shell can report on and
/// manage, without knowing what any of them are.
///
/// `::bish lsp` is a shell builtin -- it has to be readable in `$(...)`
/// (the lesson `window ls` taught) -- but nothing about running a
/// language server belongs in the shell. So the shell holds one of
/// these and knows only that entries have an id, some fields to print,
/// a recent log, and can be dropped. `lspclient::Table` implements it;
/// repl.rs, which owns the real thing, keeps the concrete handle.
///
/// The row fields are deliberately opaque `String`s rather than a
/// struct: what a language server's status consists of is not the
/// shell's business, and a typed row here would be exactly the
/// dependency this trait exists to remove.
pub trait ServiceTable {
    /// One row per entry, in the order to print, each already split
    /// into the fields `status` will join with tabs.
    fn rows(&self) -> Vec<Vec<String>>;

    /// Recent output from the entries started from declaration `id`,
    /// oldest first -- what a helper that failed to start said on its
    /// way out. Empty when there is nothing, or no such entry.
    fn logs(&self, id: u64) -> Vec<String>;

    /// Forgets every entry started from declaration `id`, including a
    /// remembered failure to start, so the next thing that needs one
    /// gets a fresh attempt. Returns how many were dropped.
    fn forget(&mut self, id: u64) -> usize;
}

/// The interactive command history, as much of it as a builtin needs.
///
/// The history lives in `repl::SessionState`, not here -- it is a
/// property of a session at a prompt, and a script has none. `plan.md`
/// called that an architecture mismatch rather than a small addition,
/// and it is: the fix is not to move the history down but to give the
/// shell an opaque way to ask.
///
/// Which is the second time this exact shape has been needed, after
/// `ServiceTable` did it for language servers, and it is the same
/// answer: a trait exec.rs owns and repl.rs implements, so the
/// dependency points the way it already does.
pub trait HistoryAccess {
    /// Every entry, oldest first, each with when it was recorded if
    /// that is known. The index a caller prints is this position plus
    /// one, matching bash's 1-based numbering.
    ///
    /// The time is `None` for anything written before the history file
    /// carried one, and for an entry rebuilt by `delete` -- see
    /// history.rs. `HISTTIMEFORMAT` shows nothing for those rather than
    /// inventing a plausible wrong time.
    fn entries(&self) -> Vec<(String, Option<i64>)>;

    /// Drops everything -- `history -c`.
    fn clear(&mut self);

    /// Drops the entry with 1-based number `n`. `false` when there is
    /// no such entry, which the caller reports.
    fn delete(&mut self, n: usize) -> bool;
}

/// What a shell has before anything installs a real one -- and what
/// every non-interactive shell keeps, since a script has no interactive
/// history to have.
pub struct NoHistory;

impl HistoryAccess for NoHistory {
    fn entries(&self) -> Vec<(String, Option<i64>)> {
        Vec::new()
    }

    fn clear(&mut self) {}

    fn delete(&mut self, _n: usize) -> bool {
        false
    }
}

/// What a shell has before anything installs a real table -- and what
/// every non-interactive shell keeps, since nothing there ever starts a
/// language server.
pub struct NoServices;

impl ServiceTable for NoServices {
    fn rows(&self) -> Vec<Vec<String>> {
        Vec::new()
    }
    fn logs(&self, _id: u64) -> Vec<String> {
        Vec::new()
    }
    fn forget(&mut self, _id: u64) -> usize {
        0
    }
}

/// One declared language server: what to run, for which languages, and
/// how to find the root of the project it should be given.
///
/// A *declaration*, not a running process -- nothing here starts
/// anything. `repl::run` owns the servers that are actually running
/// (keyed by command and project root, so one server serves every pane
/// editing the same project), the same way it owns job and edit frames;
/// this is the config a shell carries so a bishrc can say which servers
/// exist at all. `lspclient::Server` is the other half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspServer {
    /// From a per-shell counter, never reused -- `rm` always names the
    /// thing `ls` showed. Same contract as `Hook::id`.
    pub id: u64,
    /// `--lang=`: a glob over the language of the file being edited
    /// (`fileeditor::language_of`), the same shape and matcher
    /// `hook --lang` and `abbr --lang` use. `*` when unscoped.
    pub lang: String,
    /// The command line to run, split into words at registration time.
    pub command: Vec<String>,
    /// `--root=`: file names that mark the top of a project, tried in
    /// order, walking up from the file being edited. `.git` when
    /// unspecified, which is what gitignore::Stack::for_directory
    /// already treats as the boundary.
    pub root_markers: Vec<String>,
    /// `--root-cmd=`: a command that prints the project root on its
    /// first line of stdout, run in the directory of the file being
    /// opened. Empty when unset, which is the ordinary case.
    ///
    /// Exists because some roots are only knowable by asking the build
    /// tool: a Cargo *workspace* member should usually be rooted at the
    /// workspace, and only `cargo metadata` knows where that is. A
    /// generic escape hatch rather than a Cargo special case, so any
    /// language can answer the same question its own way -- and so this
    /// client stays a language server client rather than accumulating
    /// per-language knowledge.
    pub root_cmd: String,
    /// `--apply-edits=`: how much authority this server has to change
    /// files on its own.
    ///
    /// A server can ask the client to apply an edit at any moment
    /// (`workspace/applyEdit`), not only in answer to something the
    /// user asked for. That is how a command-style code action does its
    /// work, and it is also how a server could rewrite a buffer nobody
    /// invited it to touch -- so it is a policy rather than a fact.
    ///
    /// `scoped` (the default) accepts an edit only while a command the
    /// *user chose* is still running; `never` refuses always, which is
    /// what bish did before this existed; `always` accepts whenever
    /// asked, which is what VS Code does.
    pub apply_edits: String,
    /// `--setting KEY=VALUE`, repeatable: what this server should be
    /// told its configuration is.
    ///
    /// Kept as the flat `dotted.key=value` pairs they were given as,
    /// not as a parsed tree, because that is what `ls` has to print
    /// back and what a person has to be able to read. The nesting a
    /// server expects (`rust-analyzer.check.command` is an object three
    /// deep) is built where it is sent, from these.
    ///
    /// Values are JSON when they parse as JSON and strings otherwise,
    /// so `--setting x.enable=true` is a boolean and
    /// `--setting x.path=/usr/bin/foo` is a string without needing
    /// quotes fought through two levels of shell.
    pub settings: Vec<(String, String)>,
}

/// One `--setting` pass's result: the pair it found, if the flag was
/// at the front, and whatever is left of the arguments.
type ParsedSetting<'a> = (Option<(String, String)>, &'a [String]);

impl LspServer {
    /// What `ls` prints and what `status` echoes back -- the command as
    /// one line, quoting only the words that would otherwise stop being
    /// one word. `ls` is tab-separated so a script can read it, which
    /// means this field is the one a person actually reads, and
    /// `'rust-analyzer'` in quotes for no reason reads as though
    /// something odd is going on.
    pub fn command_line(&self) -> String {
        let plain = |w: &String| !w.is_empty() && w.chars().all(|c| c.is_alphanumeric() || "-_./=:+@,".contains(c));
        self.command.iter().map(|w| if plain(w) { w.clone() } else { shell_quote(w) }).collect::<Vec<_>>().join(" ")
    }
}

/// Every event a hook can be attached to.
///
/// **Naming.** `pre`/`post` is a segment rather than a prefix
/// (`editor:file:write:pre`, not `editor:file:prewrite`) because every
/// established system treats it as a modifier on one event -- git's
/// `pre-commit`/`post-commit`, vim's `BufWritePre`/`BufWritePost`,
/// emacs's `before-save-hook`/`after-save-hook`, LSP's
/// `willSave`/`didSave`. The practical half of that argument is that it
/// keeps the hierarchy prefix-matchable: `editor:file:write` and
/// `editor:file` are meaningful prefixes to glob or group by, which
/// they cannot be with the verb buried inside a segment.
///
/// `open` and `close` carry no qualifier because each has only one
/// useful moment: `open` fires once the buffer exists, and `close`
/// fires *before* it goes away -- afterwards there is nothing left for
/// a hook to look at, which is why vim's own `BufUnload`/`BufDelete`
/// are both "before" too.
///
/// The `editor:` namespace is deliberate rather than decorative: it
/// leaves room for `shell:` events (a preexec, a chpwd) to exist later
/// without renaming any of these.
pub const HOOK_EVENTS: &[&str] = &[
    "editor:file:open",
    "editor:file:write:pre",
    "editor:file:write:post",
    "editor:file:close",
    "shell:exec:pre",
    "shell:exec:post",
    "shell:cwd:change",
];

/// `::bish hook help`: how to use it, and every event with what it
/// means. Shared with the `:help hooks` page (repl.rs), so the two can't
/// drift apart.
pub fn hook_help() -> Vec<String> {
    let mut out = vec![
        "::bish hook ls [--lang=GLOB]           what is registered".to_string(),
        "::bish hook add [--lang=GLOB] EVENT COMMAND...".to_string(),
        "::bish hook rm ID                      remove one, by the id `add` printed".to_string(),
        String::new(),
        "A hook runs COMMAND with the event's own argument appended.".to_string(),
        "--lang is a glob over the file's language, as `abbr --lang` uses.".to_string(),
        String::new(),
    ];
    for (event, description) in HOOK_EVENT_HELP {
        out.push(event.to_string());
        out.push(format!("    {description}"));
    }
    out
}

/// `::bish lsp help`.
pub fn lsp_help() -> Vec<String> {
    vec![
        "::bish lsp ls [--lang=GLOB]            what is registered".to_string(),
        "::bish lsp add [--lang=GLOB] [--root=NAME,...] [--root-cmd=CMD]".to_string(),
        "                [--apply-edits=scoped|never|always] [--setting KEY=VALUE]...".to_string(),
        "               [--apply-edits=scoped|never|always] COMMAND...".to_string(),
        "::bish lsp rm ID                       remove one, by the id `add` printed".to_string(),
        "::bish lsp status                      what is actually running".to_string(),
        "::bish lsp log ID                      what a server wrote to stderr".to_string(),
        "::bish lsp restart ID                  forget it so the next file starts it afresh".to_string(),
        String::new(),
        "A registered server is started the first time a file it covers is opened,".to_string(),
        "once per project root, and shared by every pane editing that project.".to_string(),
        String::new(),
        "--lang is a glob over the file's language, as `hook --lang` uses.".to_string(),
        "--root names the files that mark the top of a project, tried in order".to_string(),
        "       walking up from the file being edited. Defaults to `.git`.".to_string(),
        "--root-cmd runs a command in the file's own directory and takes its first".to_string(),
        "       line of output as the root, for a root only a build tool knows:".to_string(),
        "       --root-cmd 'json -r .workspace_root <(cargo metadata --no-deps --format-version 1)'".to_string(),
        "       Falls back to --root when it prints nothing or fails.".to_string(),
        "--apply-edits says how far a server may change files on its own. A server".to_string(),
        "       can ask at any moment, not only when you asked it for something:".to_string(),
        "         scoped  (default) only while a command you chose is running".to_string(),
        "         never             refuse always".to_string(),
        "         always            accept whenever asked".to_string(),
        String::new(),
        "  ::bish lsp add --lang=rust --root=Cargo.toml,.git rust-analyzer".to_string(),
        String::new(),
        "`ls` and `status` print tab-separated fields, for reading in a script:".to_string(),
        "  ls:     id, language glob, root markers, command".to_string(),
        "  status: id, state, position encoding, open documents, root, command".to_string(),
        String::new(),
        "Turn the whole thing off with `bishopt --unset lsp`.".to_string(),
    ]
}

/// One line about each event: when it fires, and what its argument is.
/// A parallel table guarded by a test, for the same reason
/// `BISHOPT_HELP` is one -- see `every_hook_event_is_described`.
pub const HOOK_EVENT_HELP: &[(&str, &str)] = &[
    ("editor:file:open", "A file has been opened in the editor and its options are resolved. Argument: the path."),
    ("editor:file:write:pre", "A file is about to be written. Argument: the path."),
    ("editor:file:write:post", "A file has been written to disk. Argument: the path."),
    ("editor:file:close", "A file is about to be closed, while its buffer still exists. Argument: the path."),
    ("shell:exec:pre", "A command line is about to run at the prompt. Argument: the command line."),
    ("shell:exec:post", "A command line has finished. Argument: the command line; `$?` is its exit status."),
    ("shell:cwd:change", "The working directory has changed, however it happened. Argument: the new directory."),
];

#[derive(Debug, Clone)]
pub enum WindowAction {
    /// `window create [--name NAME]`. A name is what the tab bar shows
    /// for this window instead of its cwd, and what `window select`
    /// finds it by -- the two halves of making a workflow scriptable:
    /// something to call a window, and a way to ask for it back.
    New {
        name: Option<String>,
    },
    /// `window rename [NAME]` -- the current window. No name clears it
    /// back to showing the cwd, which is what an unnamed window shows.
    Rename(Option<String>),
    /// `::bish window select NAME|ID`. Only ever produced once the
    /// target has been found in `Shell::windows` -- a miss is an
    /// ordinary failing builtin (status 1) rather than an action,
    /// which is what makes `select || create` work inside a function,
    /// a subshell or an `if`.
    Select(usize),
    Next,
    Previous,
    Close,
    /// `::bish window minimize`/`_`, and `<C-w> _`. Shrinks the focused
    /// pane to just its divider and moves focus to a neighbour --
    /// focusing it again restores it. The underlying `minimized` state
    /// already existed for the diagnostics pane; this is what lets any
    /// pane use it.
    Minimize,
    // `window fg <window-id>`: push the target window's current top
    // frame onto *this* window's stack too -- vim-like "the same
    // session shown in multiple windows" (see WindowEntry::stack's doc
    // comment in repl.rs). The u32 is the target window's id, not an
    // index -- ids are stable identifiers, indices shift as windows
    // close.
    FgSession(u32),
    // `window split`/`s` (horizontal divider, panes stacked top/bottom)
    // or `window vsplit`/`v` (vertical divider, panes side by side):
    // divides the focused pane of the current window in two, the new
    // half holding a freshly cloned session (same session-cloning
    // primitive `New` already uses) and taking focus. See repl.rs's
    // PaneLayout for how the split tree itself is represented.
    Split {
        horizontal: bool,
    },
    // `window h/left`, `j/below`, `k/above`, `l/right`: move focus to
    // the nearest pane in that direction from the currently focused
    // one, vim Ctrl-w-hjkl style. A no-op if the current window isn't
    // split, or nothing lies in that direction.
    FocusPane(PaneDirection),
    /// `::bish window zoom`/`z`, and `<C-w>z`/`<C-w>o`: the focused
    /// pane fills the whole window until the next zoom toggles it back.
    /// The split tree underneath is untouched, so unzooming restores
    /// the arrangement exactly -- see WindowEntry::zoomed.
    Zoom,
    // `window =`/`balance`: resets every pane's size weight throughout
    // the current window's whole layout tree back to even.
    Balance,
    // `window +`/`sizeup` and `window -`/`sizedown`: grow/shrink the
    // focused pane's own share of its immediate parent split by one
    // step, relative to its siblings there. A no-op if the window isn't
    // split.
    SizeUp,
    SizeDown,
    // `window size <N>`/`<N>%`/`<N>/<M>`: set the focused pane's size
    // directly along its parent split's own axis (rows for a stacked
    // split, columns for a side-by-side one) -- see SizeSpec's own doc
    // comment for what each form means. A no-op if the window isn't
    // split.
    SetSize(SizeSpec),
}

// The parsed form of `window size <arg>`'s own argument -- see
// run_window's own parsing for the three accepted spellings plan.md
// documents: a bare integer is an absolute character count along the
// parent split's axis, `N%` is a percentage of that axis, and `N/M` is
// a plain fraction (equivalent to `(N/M)*100`%). All three ultimately
// resolve to the same thing once applied: a target fraction of the
// parent split's available space, converted into a size *weight*
// relative to the other panes there (see repl.rs's set_focused_pane_
// size) -- panes aren't given fixed sizes directly, since the whole
// layout has to keep resizing sanely as the real terminal itself
// resizes.
#[derive(Debug, Clone, Copy)]
pub enum SizeSpec {
    Characters(usize),
    Percent(f64),
    Fraction(f64),
}

#[derive(Debug, Clone, Copy)]
pub enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
}

// `window size <arg>`'s own argument, in the three forms plan.md
// documents: `N%` (trailing percent sign), `N/M` (a literal slash --
// bish's own arithmetic isn't involved, this is parsed directly as two
// integers), or a bare `N` (absolute character count). Whichever form,
// invalid input (unparseable numbers, `/0`) is reported as `None` so
// run_window can print one consistent usage error rather than a parse
// panic.
pub(crate) fn parse_size_spec(arg: &str) -> Option<SizeSpec> {
    if let Some(pct) = arg.strip_suffix('%') {
        return Some(SizeSpec::Percent(pct.parse().ok()?));
    }
    if let Some((n, m)) = arg.split_once('/') {
        let n: f64 = n.parse().ok()?;
        let m: f64 = m.parse().ok()?;
        if m == 0.0 {
            return None;
        }
        return Some(SizeSpec::Fraction(n / m));
    }
    Some(SizeSpec::Characters(arg.parse().ok()?))
}

// One entry per name a call frame shadowed: the name, the value it
// displaced (None if there was none), and whether the name already
// carried the array attribute. Both halves come back on return, or
// `local -a x` would leave `x` an array after the function returned.
type ArrayShadowStack<V> = Vec<Vec<(String, Option<V>, bool)>>;

pub struct Shell {
    pub last_status: i32,
    pub(crate) functions: HashMap<String, parser::Command>,
    // Stack of positional-parameter frames; last() is the current scope
    // ($0 is tracked separately since it's never shifted/reassigned by calls).
    pub(crate) arg_frames: Vec<Vec<String>>,
    // Stack of `local` overlays; empty unless we're inside a function call.
    // A name only lives here if `local` explicitly declared it -- plain
    // assignment still targets the global (process-env) variable unless it
    // matches an existing local of the same name, matching bash semantics.
    // `None` is a name declared local and *unset*: `local x` shadows
    // whatever the caller had, and `${x-default}` and `set -u` both
    // have to be able to tell that from an empty string -- which is
    // how every "did my caller set this?" check in a function is
    // written.
    pub(crate) var_scopes: Vec<HashMap<String, Option<String>>>,
    script_name: String,
    // Whether this shell is running a named script file rather than
    // `-c` text. The one thing it decides is whether the call stack has
    // an outermost `main` frame -- see refresh_call_arrays.
    pub running_a_script: bool,
    // Indexed arrays (`arr=(...)`). A BTreeMap (not Vec) so arrays are
    // genuinely sparse like bash's: `arr[10]=x` doesn't materialize empty
    // strings for indices 0..9, and `${#arr[@]}` counts only what's
    // actually set. Kept as one flat global map (every read/write site
    // just indexes straight into it) rather than a var_scopes-style stack,
    // since only `local -a`/`-A` need scoping at all -- see
    // array_local_stack/assoc_local_stack below for how that's retrofitted
    // via save/restore instead of a parallel lookup chain.
    pub(crate) arrays: HashMap<String, std::collections::BTreeMap<usize, String>>,
    // Associative arrays (`declare -A name`). Kept in a separate map from
    // `arrays` since their keys are arbitrary strings, not indices -- a name
    // in `assoc_names` is looked up here instead of `arrays` everywhere an
    // array is read or written.
    pub(crate) assoc_arrays: HashMap<String, OrderedMap>,
    // Carrying the array attribute is not the same as having an array
    // *value*. `declare -a A` leaves A unset -- bash prints it as
    // `declare -a A`, where `A=()` really is an assignment and prints
    // `declare -a A=()`. `arrays`/`assoc_arrays` hold the values, these
    // two hold the attribute, and a name is an array if it is in either
    // half of its pair. The attribute is what makes a later plain
    // `A=x` mean `A[0]=x` rather than a scalar shadowing the array.
    // Names this shell has explicitly `unset`, so the real-environment
    // fallback the three lookups end in does not hand the value back.
    // That fallback is there for a name set behind the shell's back
    // after startup (the session bridge's `XDG_RUNTIME_DIR`, `TZ` in a
    // test); everything inherited is seeded into `globals` at startup
    // instead. Without the tombstone it also answered for a name the
    // shell had just removed, so `unset HOME` left `${HOME-gone}`
    // saying `/home/jussi` -- and `unset` looked like it had worked,
    // because a child's environment is built from `globals` and really
    // had lost it.
    // `c`, `s` or `i` -- how this shell was invoked, which is the last
    // character of `$-`. `None` for a named script, which contributes
    // no letter at all.
    pub invocation_flag: Option<char>,
    unset_names: std::collections::HashSet<String>,
    pub(crate) array_names: std::collections::HashSet<String>,
    pub(crate) assoc_names: std::collections::HashSet<String>,
    // `alias name=value`, expanded for real -- see
    // `lexer::expand_aliases`.
    //
    // This used to be stored and never expanded, on the reasoning that
    // bash substitutes aliases *textually* before a line is tokenized,
    // which is at odds with a shell that tokenizes and parses a whole
    // script up front. The token stream turned out to be the seam: a
    // command-position word is replaced by the *tokens* of its value
    // before the parser sees either, so an alias whose value is a whole
    // pipeline is a pipeline rather than a command with `|` as an
    // argument. That was the half-correct expansion worth avoiding, and
    // it is avoided by not doing it at the word level.
    //
    // Gated on `shopt -s expand_aliases`, which is bash's own gate and
    // is off for a non-interactive shell -- so a script that never
    // touches it still sees exactly what real bash gives it. repl.rs
    // turns it on for an interactive session, which is also bash.
    // Vec, not a map -- bash's own `alias` listing (and real bash's own
    // internal table) is in definition order, not sorted, and this list is
    // never large enough for linear lookup to matter.
    pub(crate) aliases: Vec<(String, String)>,
    // `abbr -a NAME EXPANSION`: fish-style abbreviations -- unlike
    // `aliases` above, these *do* expand, but never here in exec.rs. The
    // trigger lives entirely in editor.rs's own read_line (Space/Enter
    // right after typing NAME in command position splices EXPANSION into
    // the line the user sees, before it's ever submitted; an expansion
    // with `%s` placeholders splices in as a live snippet instead -- see
    // bishedit::snippet) -- this table exists in `Shell` purely as the
    // thing `abbr`'s own builtin (below) reads/writes and repl.rs
    // snapshots per prompt to hand to read_line, same "owned-snapshot,
    // not a live borrow" pattern as `cwd`/`function_names()` already use
    // for `HighlightContext`/`ShellCompletionProvider`. Same Vec-not-map
    // choice as `aliases`, same reasoning. `pub`, not private, for
    // exactly that snapshotting (repl.rs is a different module) --
    // matching `cwd`'s own visibility.
    pub abbrs: Vec<Abbr>,
    /// `::bish map` -- key remappings, scoped to modes by a glob. Held
    /// here beside the abbreviations because it is the same kind of
    /// thing: a small user-defined table the editor consults, defined
    /// from config.bash and changeable at runtime.
    pub mappings: Vec<crate::keymap::Mapping>,
    /// `enable -n NAME`: builtins taken out of service, so the external
    /// of the same name runs instead.
    pub(crate) disabled_builtins: std::collections::HashSet<String>,
    /// One entry per function call in progress, oldest first -- what
    /// `caller` reports on. Pushed by `call_function`, which is the
    /// only place a function body is entered.
    pub(crate) call_stack: Vec<CallFrame>,
    /// The file each function was defined in. `BASH_SOURCE` reports
    /// where a function *is*, not where it was called from, and those
    /// differ the moment anything is `source`d.
    pub(crate) function_sources: HashMap<String, String>,
    /// `::bish hook`-registered commands, in the order they were added.
    /// Inherited by a virtual child exactly as `abbrs` is: a window you
    /// split off should behave like the one you split it from.
    pub hooks: Vec<Hook>,
    pub(crate) next_hook_id: u64,
    /// Declared language servers -- config, not processes. See
    /// `LspServer`.
    pub lsp_servers: Vec<LspServer>,
    pub(crate) next_lsp_id: u64,
    /// The helper processes `::bish lsp` reports on, behind a trait so
    /// this module never names a language-server type. See
    /// `ServiceTable`.
    ///
    /// Shared by every shell in the process the same way `jobs` is
    /// (`Rc::clone` in `new_virtual_child`). Because it is the live
    /// table and not a snapshot, `::bish lsp status` is an ordinary
    /// builtin readable in `$(...)`, which is the lesson `window ls`
    /// taught.
    pub lsp: Rc<RefCell<dyn ServiceTable>>,
    /// The interactive history, when there is one -- see
    /// `HistoryAccess`. Shared with whichever session owns it, the same
    /// `Rc<RefCell<_>>` way `lsp` is.
    pub history: Rc<RefCell<dyn HistoryAccess>>,
    /// Set while a hook is running, so a hook that causes its own event
    /// -- a `shell:cwd:change` hook that `cd`s, most obviously -- fires
    /// once rather than forever. Not shared with a virtual child: a hook
    /// that legitimately starts a subshell should not have that
    /// subshell's own hooks suppressed.
    firing_hooks: bool,
    // One frame per active function call (pushed/popped alongside
    // var_scopes in call_function). `local -a`/`-A name` snapshots the
    // array's pre-local value here (None if it didn't exist) and takes
    // the name away from it, so returning from the function restores
    // whatever the caller had -- a save/restore shadow rather than a real
    // nested scope chain, since `arrays`/`assoc_arrays` themselves stay
    // flat (see the comment on `arrays` above).
    array_local_stack: ArrayShadowStack<std::collections::BTreeMap<usize, String>>,
    assoc_local_stack: ArrayShadowStack<OrderedMap>,
    // `declare -n`/`local -n ref=target`: ref's own stored value is the
    // *name* of the target variable, not user data -- lookup_var/assign_var/
    // var_is_set all redirect through resolve_nameref for any name in this
    // set before doing anything else, so reading/writing `ref` transparently
    // reads/writes `target` instead. Scalars only; array-element namerefs
    // (`declare -n ref=arr[0]`) aren't supported, a scoped gap.
    pub(crate) nameref_names: std::collections::HashSet<String>,
    // Scoping for `local -n` (mirrors array_local_stack): each frame
    // records, for every name it nameref'd, whether that name was already
    // a nameref beforehand -- so call_function can undo just the
    // membership change on return without disturbing a same-named nameref
    // the *caller* had declared. `declare -n` at top level (empty
    // var_scopes) has no frame to push onto and just leaks globally,
    // matching bash's own top-level `declare -n` behavior.
    nameref_local_stack: Vec<Vec<(String, bool)>>,
    // pushd/popd/dirs: does NOT include the current directory itself (bash
    // convention -- `dirs` prints cwd first, then this stack). `+N`/`-N`
    // rotation forms aren't implemented, a scoped gap.
    pub(crate) dir_stack: Vec<String>,
    // shopt -s/-u NAME: explicit overrides only, keyed by name -- absent
    // means "use that name's own default from KNOWN_SHOPT_OPTIONS", not
    // "off" (several real bash options, e.g. cmdhist/promptvars, default
    // on). Most of these have no actual effect on bish's behavior beyond
    // being trackable/queryable/listable (e.g. extglob is unconditionally
    // on regardless of this map, see glob.rs), but recognizing the names
    // at all means `shopt -s extglob`/`shopt -s nullglob` in a script no
    // longer fails as an unknown command, which would otherwise abort the
    // whole script under `set -e`.
    pub(crate) shopt_options: std::collections::HashMap<String, bool>,
    // `bishopt --set/--unset NAME [VALUE]`: bish's own config surface, a
    // deliberately separate namespace from shopt_options above (shopt
    // exists only for bash-script compatibility -- see KNOWN_BISHOPTS'
    // own doc comment for why the two shouldn't mix). Same override-only
    // shape as shopt_options: absent means "use that option's own
    // registered default", `--unset` removes the entry outright rather
    // than writing the default back, so "explicitly unset" and "never
    // touched" collapse to the same state.
    pub(crate) bishopts: std::collections::HashMap<String, BishOptValue>,
    // `::bish theme begin`/`::bish theme end`'s own registry -- theme
    // name -> the bishopt overrides captured while declaring it (see
    // pending_theme's own doc comment for how a declaration fills this
    // in). Consulted by bishopt_value as a second-tier default, below an
    // explicit self.bishopts override but above KNOWN_BISHOPTS' own
    // hardcoded one, whenever the "theme" bishopt itself (an ordinary
    // Str option, set the normal way -- outside any declaration) names
    // one of these. A shell-wide table, cloned into a forked child the
    // same way bishopts itself is.
    pub(crate) themes: std::collections::HashMap<String, Theme>,
    /// Syntax-highlighting colours, by name -- see `::bish hl`.
    ///
    /// A plain map rather than a registry, because the names are open:
    /// `HighlightKind`'s own are what bish produces today, and a
    /// language server's semantic token types will be more of the same
    /// without any of them needing to be declared first. That is the
    /// whole reason these are not bishopts, which are a closed set with
    /// a default each.
    pub(crate) hl: std::collections::HashMap<String, String>,
    // `Some` for the entire span between `::bish theme begin` and its
    // matching `::bish theme end` -- every `bishopt --set NAME VALUE` in
    // between is diverted here (keyed by NAME) instead of applying live,
    // exactly the values that end up under a new entry in `themes` once
    // `end` runs (see run_bish_theme_end's own doc comment for how the
    // "theme" key specifically is pulled back out to name that entry,
    // rather than becoming part of the theme's own opts). `None` outside
    // a declaration -- the ordinary, overwhelmingly common state. Reset
    // to `None` (not cloned) in a forked child: a declaration in
    // progress is transient, top-level-only state, not something that
    // makes sense to carry into a subshell/command-substitution mid-way.
    pub(crate) pending_theme: Option<Theme>,
    // `complete NAME`: registered completion specs, by command name -- see
    // run_complete's own doc comment. Consulted both by `compgen`-adjacent
    // introspection (`complete -p`/`-r`/`compopt`) and, via a per-prompt
    // snapshot repl.rs builds the same way as cwd/known_functions, by
    // bish's own interactive Tab completion (ShellCompletionProvider).
    pub(crate) completions: std::collections::HashMap<String, compgen::CompgenSpec>,
    // `complete -D`: the fallback spec used when no exact name matches.
    pub(crate) default_completion: Option<compgen::CompgenSpec>,
    // `readonly NAME`. Checked by assign_var, the single write path plain
    // assignment/local/export/declare/arithmetic-assignment/read/getopts
    // all funnel through, so marking a name here blocks writes everywhere
    // at once.
    pub(crate) readonly_names: std::collections::HashSet<String>,
    // `declare -i`/`local -i`: assignments to these names are evaluated as
    // arithmetic expressions instead of stored as literal text (checked in
    // assign_var, the single write path).
    pub(crate) integer_names: std::collections::HashSet<String>,
    // `declare -u`/`-l`: assignments to these names are case-folded
    // (checked in assign_var alongside integer_names).
    pub(crate) upper_names: std::collections::HashSet<String>,
    pub(crate) lower_names: std::collections::HashSet<String>,
    // `declare -x`/`export -x NAME` on a name that's currently a `local`:
    // globals are already unconditionally visible to children (assign_var
    // writes them straight to the process env), so this only matters for a
    // local -- assign_var additionally mirrors the value into the process
    // env for any name in this set, so child processes can see it despite
    // it living in var_scopes rather than env.
    pub(crate) exported_names: std::collections::HashSet<String>,
    // Every proc-sub temp file created for the command currently being
    // built, deleted once it finishes (drain_proc_subs).
    proc_sub_cleanup: Vec<String>,
    /// Coroutines that outlive the command that started them -- a
    /// `<( )` producer, for now.
    ///
    /// Shared with virtual children the way the job table is: a
    /// substitution written inside a subshell is still something this
    /// process has to keep running. Given time wherever the shell would
    /// otherwise be idle in the kernel; see `pump_coroutines`.
    background_coroutines: Rc<RefCell<crate::scheduler::Scheduler>>,
    /// This shell's own ends of the pipes a substitution is on the far
    /// side of, held until the command using them is done.
    proc_sub_pipes: Vec<std::os::fd::OwnedFd>,
    /// Substitutions that are real processes, with whether this shell
    /// may stop one that is still running -- a `<( )` producer may be,
    /// a `>( )` consumer may not. Killing one of those cost `tee` its
    /// buffered file: it had written to stdout and not yet flushed.
    proc_sub_children: Vec<(std::process::Child, bool)>,
    // $RANDOM: xorshift64* state, reseeded from the current time at
    // startup (no external RNG crate). Advanced on every read.
    rng_state: u64,
    // $SECONDS: wall-clock time the shell started, so `$SECONDS` reads as
    // elapsed-since-start by default. `SECONDS=n` (bash lets you reset the
    // counter) is handled by assign_var recording an offset instead of
    // trying to rewind this.
    shell_start: std::time::Instant,
    seconds_offset: i64,
    // Background jobs (`cmd &`), tracked well enough for `jobs`/`fg`/`bg`/
    // `wait`/`kill %N` to work against real child processes. Real
    // terminal job control -- process-group isolation, `tcsetpgrp`
    // foreground reassignment, genuine Ctrl-Z/SIGTSTP suspend-to-stopped
    // -- is layered on top of this (M11, gated on `set -m`/opt_monitor)
    // for a single foreground external command run_single spawns directly
    // (see its own pre_exec setpgid hook, tcsetpgrp reassignment, and
    // Job::pgid): that's the one case a human interactively suspending a
    // running command actually hits. A *backgrounded* multi-stage
    // pipeline (run_multi) or redirected compound command (run_
    // compound_redirected) also gets process-group isolation (every
    // pipeline stage sharing one group, seeded from its first stage's
    // pid) -- enough for `kill %N`/`bg`'s own SIGCONT to correctly reach
    // every process in the job at once, without the terminal-foreground-
    // reassignment/stop-handling machinery a *foreground* run of either
    // construct would additionally need (not implemented -- Ctrl-Z on a
    // foreground pipeline/redirected compound still does nothing useful,
    // matching a plain foreground external command before M11 added
    // that). The self-exec'd subshell/coproc spawn sites are still
    // deliberately left with no isolation at all, backgrounded or not.
    //
    // Held behind Rc<RefCell<_>> (single-threaded, so plain interior
    // mutability, no Arc/Mutex needed) rather than owned directly:
    // std::process::Child (inside Job) isn't Clone, so this table can only
    // ever be referenced, never duplicated -- the moment more than one
    // Shell exists (planned: a `window new` virtual session sharing job
    // control with the shell it was opened from), each one needs to point
    // at the *same* table, not a copy of it. With only one Shell alive
    // today, this is behaviorally invisible -- it just draws the seam that
    // work needs.
    pub(crate) jobs: Rc<RefCell<JobTable>>,
    // `trap CMD SIGNAL`. EXIT is handled separately (exit_trap below) since
    // it isn't a real OS signal. Everything else here is a genuine
    // sigaction-installed handler (see install_trap_handler) -- the numeric
    // signal is the key since that's what the handler's async-signal-safe
    // bookkeeping (PENDING_SIGNALS) uses; SIGNAL_NAMES maps both directions
    // for `trap`'s own name-based syntax.
    traps: std::collections::HashMap<i32, TrapAction>,
    // `trap CMD EXIT` handler. Not a real signal (there's no SIGEXIT), so
    // it's run directly wherever the shell is about to terminate rather
    // than through the sigaction/PENDING_SIGNALS machinery.
    exit_trap: Option<String>,
    // `$BASH_COMMAND`: the command being run, for a DEBUG or ERR trap
    // to name. bash holds the *source text*, which needs spans this
    // parser does not carry -- the same wall the
    // `function-body-formatting` divergence describes -- so this is the
    // command with its words expanded. It differs from bash only where
    // the command contained an expansion, and is identical otherwise.
    bash_command: String,
    // The subshell nesting depth the EXIT trap was set at. It fires for
    // the exit of the shell that armed it and for no other -- see
    // run_exit_trap.
    exit_trap_depth: u32,
    // The other three pseudo-signals `trap` takes. Not signals at all --
    // the interpreter fires them itself, at three places it already
    // passes through: before each simple command, after a command fails
    // under the rules `errexit` already uses, and on a function's
    // return.
    //
    // Separate fields rather than entries in `traps` for the reason
    // `exit_trap` is: `traps` is keyed by a real signal number, and
    // these have none.
    debug_trap: Option<String>,
    err_trap: Option<String>,
    return_trap: Option<String>,
    // The call depth each pseudo-trap was set at. A pseudo-trap fires
    // at the depth it was installed and nowhere else, unless the option
    // that makes it inherited is on -- so a RETURN trap set inside a
    // function runs when *that* function returns (no `set -T` needed),
    // while one set at the top level does not follow calls into it.
    // Gating on the option alone got the first case backwards and never
    // ran it at all.
    pseudo_trap_depth: [usize; 3],
    // Whether a trap's own body is running. A DEBUG trap whose body is a
    // command would otherwise fire DEBUG again, forever; bash guards the
    // same way.
    in_trap: bool,
    // Whether `PROMPT_COMMAND` is running. It is invoked once per
    // prompt from one place, so nothing can currently re-enter it --
    // this exists so that stays true if a second call site ever appears.
    in_prompt_command: bool,
    // Whether `command_not_found_handle` is running -- a handler that
    // itself mistypes a command would otherwise call itself forever.
    in_command_not_found: bool,
    // `set -T` / `set -o functrace` covers DEBUG and RETURN; `set -E` /
    // `set -o errtrace` covers ERR. Two options and not one, because
    // bash makes them two -- inheriting DEBUG into every function is a
    // very different appetite from wanting an error handler to still
    // fire inside one.
    //
    // Without them, none of the three is inherited by a shell function,
    // which is why a trap set at the top level fires once for a failing
    // function call rather than once inside and once outside.
    opt_functrace: bool,
    opt_errtrace: bool,
    // How many shell functions deep we are, which is the question
    // `functrace` is really about.
    function_depth: usize,
    // Set when a call has been refused for nesting too deeply, cleared
    // once the stack it was refused on has unwound all the way out.
    //
    // Refusing the one call is not enough on its own. `f() { f; }` does
    // unwind by itself, because the refused call is the last thing in
    // the body -- but `f() { f; f; }` would call `f` again at every
    // level on the way out and take exponential time to get nowhere.
    // Real bash does not have this problem because it longjmps to the
    // top level; this flag is what stands in for that jump.
    nesting_unwind: bool,
    // `coproc`'s pipe halves that the *shell* keeps (the coprocess's own
    // ends are handed to the child and closed here after spawn). Kept
    // alive here, keyed by raw fd number, for as long as the coprocess
    // might still be interacted with -- otherwise the PipeReader/Writer
    // would drop and close the fd the moment run_coproc returns. Numbered-
    // fd redirects on *other* commands (`cmd <&"${NAME[0]}"`) just need
    // the fd number to still be open in this process at dup2 time, which
    // this satisfies without those redirects needing to know anything
    // about coproc specifically; `read -u FD` is the one thing that reads
    // through this table directly (see run_single's "read" arm).
    coproc_fds: std::collections::HashMap<i32, KeptFd>,
    // `set -e`/`-u`/`-x`/`-o pipefail`/`-f`.
    opt_errexit: bool,
    opt_nounset: bool,
    opt_xtrace: bool,
    opt_pipefail: bool,
    opt_noglob: bool,
    // `set -m`/`-o monitor`: real bash's `fg`/`bg` refuse to run at all
    // ("no job control") unless this is on -- confirmed against real bash,
    // which defaults it off for a non-interactive script (only real
    // interactive shells, or a script that explicitly opts in, get it).
    // `jobs`/`wait`/`kill` aren't gated by this; only fg/bg are.
    pub(crate) opt_monitor: bool,
    // `set -r` (bash's own restricted-shell mode -- NOT the same thing
    // as restrict_to_builtins below, which is bish's own unrelated
    // command-mode-colon-line feature). Only a short flag in real bash
    // -- confirmed there's no `-o restricted` name at all (`set -o`'s
    // own listing never includes it, unlike every other `-o` name). A
    // one-way latch: apply_shell_flag only ever turns this on, never
    // off, matching real bash's own "turning off restricted mode is not
    // possible" rule (confirmed: `set +r` errors there and leaves it
    // set). See run_cd/the "exec"/"."/"source" arms in run_single,
    // check_restricted_command_name, and open_out for what it actually
    // enforces.
    opt_restricted: bool,
    // `set -C` / `set -o noclobber`: a plain `>` refuses to truncate an
    // existing regular file. Enforced in open_out, the one place every
    // output redirect opens its file (see its own doc comment), and
    // defeated per-redirect by `>|`, which carries a `clobber` flag all
    // the way from the lexer for exactly this.
    opt_noclobber: bool,
    // `set -o posix`: recognized and toggleable (so a script that
    // merely checks/sets it doesn't break) but not behaviorally
    // enforced -- real POSIX mode is dozens of small, scattered parsing/
    // expansion differences throughout bash; this doesn't attempt that,
    // same "accepted but not enforced" spirit as this shell's own -u/
    // -l/-n declare attributes on names that aren't its own scalar
    // variables.
    opt_posix: bool,
    // Suppresses errexit while >0 -- set around if/while/until conditions
    // and negated (`!`) pipelines, the cases POSIX explicitly exempts from
    // triggering -e (a failing condition is meant to be checked, not
    // treated as a fatal error).
    suppress_errexit: u32,
    // Set by run_and_or when the status it returns came from a command
    // `set -e` does not apply to. bash exits on a failing pipeline
    // "except the command following the final && or ||" -- so in
    // `false && echo no` the `false` is exempt and the `echo` never
    // runs, and the whole list's failing status must not abort the
    // shell either. Only the ListItem check below reads it.
    errexit_exempt: bool,
    // The exit status of the most recent `$(...)`, for the one command
    // that takes it as its own: a bare assignment. `x=$(false)` is a
    // failing command in bash -- which is what makes `set -e` catch a
    // failed capture, the single most common way a strict script means
    // to stop.
    last_subst_status: Option<i32>,
    // Set by run_single around a command's own word-expansion so mid-
    // expansion diagnostics (nounset's "unbound variable", etc) go to that
    // command's own `2>` target instead of unconditionally to the shell's
    // real stderr, matching bash routing its own error messages through
    // the command's redirects too.
    current_stderr_target: Option<String>,
    // Set while running command-mode (`:`) input: only builtins may run
    // directly (see the fallthrough gate in run_single, right before
    // function lookup) -- `command NAME` is the explicit escape hatch for
    // externals, matching its existing "force external" semantics.
    pub restrict_to_builtins: bool,
    // Whether `window`-family promotion into full-screen mode has already
    // happened (see run_window/promote_if_needed) -- only ever flips once.
    // Rc<Cell<_>>, not a plain bool: promotion is a whole-terminal concept
    // ("the real screen is in full-screen mode"), not a per-session one --
    // every virtual session sharing this root must see the same flag, or a
    // second session invoking `window` would wrongly re-trigger the
    // promotion stub. Cell (not RefCell) since get/set on a bool never
    // needs borrow tracking.
    promoted: Rc<Cell<bool>>,
    /// Whether something is actually managing windows for this shell --
    /// set by `repl.rs` on the sessions it drives, false everywhere
    /// else. It is what lets the `window` builtin be callable from an
    /// ordinary shell function (which is the whole point of naming and
    /// selecting windows) without letting `ExecResult::Window` escape
    /// from `bish script.sh`, where nothing would ever act on it and
    /// `run_program` would take it for a signal and stop early.
    pub windows_available: bool,
    /// What windows currently exist, refreshed by the repl loop before
    /// every command -- the same owned-snapshot pattern `cwd`,
    /// `known_functions` and the completion context already use.
    ///
    /// This is what lets `window ls` be an ordinary builtin that writes
    /// to its own sink instead of an action the repl applies afterwards,
    /// and so what makes `$(window ls)` capture anything at all. Cloned
    /// into a virtual child, so a substitution or a subshell sees the
    /// same list its parent does.
    pub windows: Vec<WindowInfo>,
    // Mirrors the OS process's real cwd, kept in sync by run_cd/run_pushd/
    // run_popd (which still delegate the actual directory change to
    // std::env::set_current_dir -- this field doesn't yet let a session
    // diverge from the process's real cwd, that only matters once
    // multiple sessions can exist). Exists now so callers read cwd from
    // here rather than re-querying the OS each time, and so every spawn
    // site already passes an explicit cwd -- both wiring this milestone's
    // later multi-session work will need without changing yet again.
    pub cwd: std::path::PathBuf,
    // See OutputSink's doc comment: Real until repl.rs flips it to Grid
    // at promotion time.
    pub(crate) sink: OutputSink,
    // Whether the last byte sink_out/sink_err actually wrote (through
    // *any* sink, though only OutputSink::Real's caller -- repl.rs's main
    // loop, before drawing the next prompt -- ever reads this) was
    // something other than a newline. OutputSink::Grid doesn't need this:
    // its vt100 emulator already tracks its own cursor precisely, and
    // compositor_redraw always repaints from that real state rather than
    // assuming anything about what row it owns. OutputSink::Real has no
    // such model -- it writes straight to the real terminal -- so without
    // this, a builtin's output that doesn't end in "\n" (`printf foo`,
    // `echo -n foo`) leaves the terminal cursor stuck mid-row, and the
    // next prompt's own redraw() (which assumes it's redrawing a row it
    // already owns -- see its own doc comment) erases that output by
    // writing blank padding over it before the prompt, instead of just
    // leaving it alone or moving to a fresh line first. Only covers
    // bish's own builtins -- see ran_external_since_prompt, below, for
    // the same problem with an *external* command's own output, which
    // bish never sees a byte of. Cell, not a plain field: sink_out/
    // sink_err are `&self` (not `&mut self` -- every builtin that prints
    // goes through them, and making that `&mut` would ripple
    // everywhere), so updating this from there needs interior mutability.
    real_output_needs_newline: std::cell::Cell<bool>,
    // Whether an external process has been spawned (with its stdout
    // inherited straight from the real terminal -- run_single's ordinary
    // foreground case, the `command` builtin's own spawn, or run_multi's
    // pipeline stages) since the last time repl.rs's main loop checked.
    // Unlike real_output_needs_newline (above), there's no way to track
    // *whether* such a command's own output ended in a newline just by
    // watching what bish itself writes -- an external child's stdout
    // goes straight to the inherited fd, bypassing sink_out/sink_err
    // entirely. So this only records that repl.rs needs to actually ask
    // the terminal (term::query_cursor_column, a real DSR round-trip --
    // more expensive than checking real_output_needs_newline, which is
    // why this flag exists at all: to skip that round-trip on the common
    // pure-builtin command, where the cheap tracking above is already
    // enough). Same Cell-for-interior-mutability reasoning as above.
    ran_external_since_prompt: std::cell::Cell<bool>,
    // Set by run_fg immediately before returning ExecResult::Fg, taken
    // right back out via take_pending_fg (called by repl.rs in response
    // to that signal) -- see ExecResult::Fg's doc comment. Not shared via
    // Rc: only ever set and cleared within the single session that ran
    // `fg`, never meant to be visible to any other session.
    pub(crate) pending_fg: Option<Job>,
    // Same pattern as `pending_fg` just above, for `e` -- see
    // ExecResult::Edit's own doc comment. Holds `e`'s own argument
    // vector verbatim (everything after the command word, unparsed):
    // what those arguments *mean* is the editor's business, not the
    // shell's -- see fileeditor::parse_edit_args.
    pending_edit: Option<Vec<String>>,
    // Set by check_nounset (a `set -u` violation) instead of calling
    // std::process::exit directly -- that call happens deep inside word
    // expansion (expand_word and its many callers), which has no
    // ExecResult to bubble a signal through. Consulted at the two
    // choke points that matter: run_single checks right after building a
    // simple command's argv (stopping *that* command from ever
    // dispatching/spawning using a bad expansion, the highest-value
    // containment -- matches the immediacy std::process::exit used to
    // give), and run_program's own per-statement loop checks as a
    // backstop (same granularity `set -e`'s own check there already has,
    // catching violations from expansion sites run_single's own check
    // doesn't cover -- e.g. a `for`/`case`/arithmetic list). Not shared
    // via Rc: each Shell (including a new_virtual_child) gets its own.
    pending_exit: Option<i32>,
    // Whether a human is typing at this shell. Set once by repl.rs at
    // interactive startup (see enable_monitor_mode) and inherited by
    // every virtual child. The one thing it gates is which errors are
    // *fatal*: bash exits a non-interactive shell on an assignment to
    // a readonly variable, and would be intolerable if it did that to
    // someone's session over a typo.
    interactive: bool,
    // Set when expanding a *word* of the command currently being built
    // failed: `${x!y}` and `${a[}` (no such parameter), `${x:?}` (the
    // one whose whole purpose is to stop), and an arithmetic expansion
    // that does not parse. bash treats all three as fatal in a
    // non-interactive shell -- and only in a word: `(( 1+ ))` and `let
    // '1+'` are ordinary commands that fail and are stepped over.
    // Cleared at the checkpoints in run_single that consult it, so it
    // never leaks into the next command.
    expansion_failed: bool,
    // See StdioOverride's own doc comment. `None` for every ordinary
    // Shell (spawned external commands really inherit the real process's
    // fds); set by run_in_child_shell on the Shell it builds for a
    // converted foreground subshell/command-substitution/proc-sub. Not
    // shared via Rc: each Shell gets its own (a nested converted
    // construct inside another sets its own fresh override, it doesn't
    // see or touch the enclosing one's).
    stdio_override: Option<Rc<RefCell<StdioOverride>>>,
    // The active debugger, if this Shell (or an ancestor it was
    // new_virtual_child'd from) is running under one -- see DebugHook's
    // own doc comment. Shared via Rc::clone (same treatment as `jobs`/
    // `promoted`) so a breakpoint set from the debug session still fires
    // inside a converted foreground subshell/command-substitution, which
    // is otherwise a genuinely separate Shell.
    debug_hook: Option<Rc<RefCell<dyn DebugHook>>>,
    // The source line of the statement currently running -- `$LINENO`.
    //
    // Recorded in `run_program`, which already had it: the debugger has
    // been handed `item.line` on every statement since line tracking
    // landed on the executable AST, and nothing else needed it. Which is
    // why `LINENO` was not possible before and is nearly free now.
    current_line: usize,
    // How many nested foreground subshells/command-substitutions/proc-
    // subs (run_in_child_shell) deep this Shell is, relative to the real
    // top-level one -- incremented (never shared) by new_virtual_child.
    // Half of DebugDepth; see its own doc comment for why this can't just
    // be `var_scopes.len()` alone.
    subshell_depth: u32,
    // This session's own remembered idea of "what the real process
    // environment/umask should look like" -- see sync_real_state_in/out's
    // own doc comment. Exists because ordinary (non-`local`) variable
    // assignment and `umask` both mutate real, process-wide OS state
    // (raw_var_write's fallback is std::env::set_var; run_umask calls the
    // real umask(2) syscall) rather than anything Shell-owned -- fine for
    // a single session, but every session sharing this one real process
    // (repl.rs's `window new`/pane-split code, via new_virtual_child)
    // would otherwise silently clobber each other's variables/umask the
    // instant more than one of them ever runs a command. cwd doesn't need
    // an equivalent snapshot: every real-process spawn site already
    // passes `.current_dir(&self.cwd)` explicitly rather than relying on
    // the inherited real cwd, so `self.cwd` (already deep-cloned by
    // new_virtual_child, already kept live-accurate by run_cd) is by
    // itself already enough for that half -- *except* for plain relative-
    // path file I/O (open_out, a redirect's own `Redirect::In`, `source`,
    // ...), which does still resolve against the real process cwd; see
    // sync_real_state_in's own doc comment for how that's covered.
    // `Rc` rather than a plain map: `new_virtual_child` runs on every
    // foreground `$( )`/`( )`, and a fresh `std::env::vars().collect()`
    // there was ~18us per substitution on a 74-variable environment for
    // a copy no in-process child ever reads (only `sync_real_state_in`,
    // i.e. session switching, does). Shared until something writes,
    // through `Rc::make_mut`.
    env_snapshot: Rc<std::collections::HashMap<String, String>>,
    // Every global shell variable. Seeded at startup from the real
    // environment (a variable a shell inherits is an exported one, as
    // in bash) and the home of everything assigned since.
    //
    // It used to be the process environment itself -- raw_var_write's
    // fallback was `std::env::set_var`. That had three costs. Every
    // variable was exported, so `x=1` was handed to every command bish
    // ran, and there was no such thing as an unexported variable. A
    // value the environment cannot hold took the shell down with it:
    // `read -d ''` panicked on the NUL. And it is process-global, so
    // two tests in the same process saw each other's variables.
    //
    // The real environment is now *derived*: a name is written there
    // only while it is in `exported_names` (see raw_var_write), which
    // is also what a spawned child inherits. BTreeMap so `declare -p`
    // and `compgen -v` enumerate in a stable order.
    globals: std::collections::BTreeMap<String, String>,
    // A monotonic count of *effects*: every builtin or external command
    // that ran, every assignment that was performed, every read of a
    // deliberately-volatile variable ($RANDOM, $SECONDS, $EPOCH*).
    //
    // Deliberately not bumped by a shell-function call itself -- a call is
    // not an effect, its body's commands are, and that distinction is the
    // whole point: it lets `call_function` recognise a function that has
    // re-entered itself having done *nothing at all* since the previous
    // entry, which (with identical positional parameters) is a proof that
    // the program cannot terminate rather than a guess. See
    // `check_nonproductive_recursion`.
    effects: u64,
    pub(crate) umask_snapshot: u32,
}

// A fresh, process/time-derived seed -- used both for a brand-new Shell
// and for new_virtual_child's child (which deliberately does NOT inherit
// the parent's current rng_state, so sibling sessions don't produce
// correlated $RANDOM sequences).
fn fresh_rng_seed() -> u64 {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0x2545F4914F6CDD1D)
        ^ (std::process::id() as u64).wrapping_mul(0x9E3779B97F4A7C15);
    if seed == 0 { 0x2545F4914F6CDD1D } else { seed }
}

/// One in-progress function call, for `caller`.
///
/// `called` is the function whose body is running; `call_line` is the
/// line its call sits on, and `source` the file that line is in.
///
/// Who *made* the call is not stored: it is the `called` of the frame
/// below, or the script itself at the bottom. Deriving it keeps one
/// source of truth, so a frame cannot disagree with the stack it is in.
#[derive(Clone)]
pub(crate) struct CallFrame {
    pub(crate) called: String,
    pub(crate) call_line: usize,
    pub(crate) source: String,
    // `Shell::effects` as it stood when this frame was entered, and a hash
    // of the positional parameters it was entered with -- the two halves of
    // the non-productive-recursion check in `call_function`.
    pub(crate) effects_at_entry: u64,
    pub(crate) args_hash: u64,
}

impl Shell {
    pub fn new() -> Self {
        // Walked once, not three times. The three collections below
        // all describe the same inherited environment -- what is
        // exported, what the variables are, and what the real process
        // had at startup -- and `std::env::vars()` is a fresh traversal
        // of `environ` with a String pair allocated per variable each
        // time it is called. That was 169us of this function's 253us on
        // a 74-variable environment, and every re-exec'd construct pays
        // it.
        let mut inherited: Vec<(String, String)> = std::env::vars().collect();
        // `PS4` is a real variable with a default, not a fallback used
        // when it is missing. The difference shows the moment a script
        // says `unset PS4`: bash then traces with no prefix at all,
        // where a fallback would keep printing `+ `.
        if !inherited.iter().any(|(k, _)| k == "PS4") {
            inherited.push(("PS4".to_string(), "+ ".to_string()));
        }
        let mut shell = Shell {
            last_status: 0,
            functions: HashMap::new(),
            arg_frames: vec![Vec::new()],
            var_scopes: Vec::new(),
            script_name: "bish".to_string(),
            running_a_script: false,
            // Scripts branch on `${BASH_VERSINFO[0]}` far more often
            // than they parse `$BASH_VERSION`, so the taken-apart form
            // has to exist too.
            arrays: HashMap::from([("BASH_VERSINFO".to_string(), BASH_VERSINFO.iter().enumerate().map(|(i, v)| (i, (*v).to_string())).collect())]),
            assoc_arrays: HashMap::new(),
            invocation_flag: None,
            unset_names: std::collections::HashSet::new(),
            array_names: std::collections::HashSet::new(),
            assoc_names: std::collections::HashSet::new(),
            aliases: Vec::new(),
            abbrs: Vec::new(),
            mappings: Vec::new(),
            disabled_builtins: std::collections::HashSet::new(),
            call_stack: Vec::new(),
            function_sources: HashMap::new(),
            hooks: Vec::new(),
            next_hook_id: 1,
            lsp_servers: Vec::new(),
            next_lsp_id: 1,
            lsp: Rc::new(RefCell::new(NoServices)),
            history: Rc::new(RefCell::new(NoHistory)),
            firing_hooks: false,
            array_local_stack: Vec::new(),
            assoc_local_stack: Vec::new(),
            nameref_names: std::collections::HashSet::new(),
            nameref_local_stack: Vec::new(),
            dir_stack: Vec::new(),
            shopt_options: std::collections::HashMap::new(),
            bishopts: std::collections::HashMap::new(),
            themes: std::collections::HashMap::new(),
            hl: std::collections::HashMap::new(),
            pending_theme: None,
            completions: std::collections::HashMap::new(),
            default_completion: None,
            // BASH_VERSINFO is readonly, as it is in bash: `declare -p`
            // says `-ar`, and an assignment to it is refused.
            readonly_names: std::collections::HashSet::from(["BASH_VERSINFO".to_string()]),
            integer_names: std::collections::HashSet::new(),
            upper_names: std::collections::HashSet::new(),
            lower_names: std::collections::HashSet::new(),
            // A variable inherited from the environment is an exported
            // variable -- bash's rule, and what makes `env` agree with
            // `declare -p` on a fresh shell.
            exported_names: inherited.iter().map(|(k, _)| k.clone()).collect(),
            globals: inherited.iter().cloned().collect(),
            effects: 0,
            proc_sub_cleanup: Vec::new(),
            rng_state: fresh_rng_seed(),
            shell_start: std::time::Instant::now(),
            seconds_offset: 0,
            jobs: Rc::new(RefCell::new(JobTable::new())),
            background_coroutines: Rc::new(RefCell::new(crate::scheduler::Scheduler::new())),
            proc_sub_pipes: Vec::new(),
            proc_sub_children: Vec::new(),
            traps: std::collections::HashMap::new(),
            bash_command: String::new(),
            exit_trap: None,
            exit_trap_depth: 0,
            debug_trap: None,
            err_trap: None,
            return_trap: None,
            pseudo_trap_depth: [0; 3],
            in_trap: false,
            in_prompt_command: false,
            in_command_not_found: false,
            opt_functrace: false,
            opt_errtrace: false,
            function_depth: 0,
            nesting_unwind: false,
            coproc_fds: std::collections::HashMap::new(),
            opt_errexit: false,
            opt_nounset: false,
            opt_xtrace: false,
            opt_pipefail: false,
            opt_noglob: false,
            opt_monitor: false,
            opt_restricted: false,
            opt_noclobber: false,
            opt_posix: false,
            suppress_errexit: 0,
            errexit_exempt: false,
            last_subst_status: None,
            current_stderr_target: None,
            restrict_to_builtins: false,
            promoted: Rc::new(Cell::new(false)),
            windows_available: false,
            windows: Vec::new(),
            cwd: std::env::current_dir().unwrap_or_default(),
            sink: OutputSink::Real,
            real_output_needs_newline: std::cell::Cell::new(false),
            ran_external_since_prompt: std::cell::Cell::new(false),
            pending_fg: None,
            pending_edit: None,
            pending_exit: None,
            interactive: false,
            expansion_failed: false,
            stdio_override: None,
            debug_hook: None,
            current_line: 0,
            subshell_depth: 0,
            env_snapshot: Rc::new(inherited.into_iter().collect()),
            umask_snapshot: current_umask(),
        };
        // bash starts with IFS set to space/tab/newline, and scripts
        // rely on that: `old=$IFS; IFS=,; ...; IFS=$old` is the standard
        // way to change it temporarily, and with IFS merely *unset* that
        // captures the empty string and restores it as "split on
        // nothing". Splitting itself was already right (`get_ifs` falls
        // back to the default when unset, and still tells unset apart
        // from empty) -- what was missing was `$IFS` reading as anything.
        shell.assign_var("IFS", " \t\n".to_string());
        // bash sets `PWD` at startup whether or not it inherited one,
        // and scripts read it far more often than they call `pwd`. bish
        // only ever had one when something upstream had exported it --
        // so `env -i bish -c 'echo $PWD'` printed nothing, and every
        // `cd` afterwards was updating a variable that had never been
        // set. Exported, as bash's is.
        // An inherited one is kept only when it names this same
        // directory by another route -- which is the point of
        // inheriting it, since it is how a path through a symlink
        // survives. One that names somewhere else is stale and bash
        // replaces it.
        let inherited_is_here = shell
            .globals
            .get("PWD")
            .map(std::path::PathBuf::from)
            .and_then(|p| std::fs::canonicalize(p).ok())
            .is_some_and(|p| std::fs::canonicalize(&shell.cwd).is_ok_and(|here| p == here));
        if !inherited_is_here {
            let here = shell.cwd.to_string_lossy().into_owned();
            shell.export_var("PWD", here);
        }
        shell
    }

    // pub: repl.rs's own top-level orchestration code (syntax-error
    // messages tied to a specific session, command-mode diagnostics)
    // needs these too -- it has only a handful of sites, so it calls
    // these directly rather than pulling the sh_print!-family macros
    // (private to this module) across the crate boundary.
    pub fn sink_out(&self, s: &str) {
        self.sink.write_out(s);
        self.note_output(s);
    }

    pub fn sink_err(&self, s: &str) {
        self.sink.write_err(s);
        self.note_output(s);
    }

    // Updates real_output_needs_newline's own tracking -- see its doc
    // comment. An empty `s` leaves it unchanged (nothing was actually
    // written, so the terminal's cursor position, if this is the Real
    // sink, hasn't moved either).
    fn note_output(&self, s: &str) {
        if let Some(c) = s.chars().last() {
            self.real_output_needs_newline.set(c != '\n');
        }
    }

    // repl.rs's main loop calls this right before drawing the next
    // prompt (only meaningful when that session's sink is still Real --
    // see real_output_needs_newline's own doc comment) -- returns
    // whether the last thing written left the cursor mid-row, and clears
    // the flag either way so it reflects only what's happened since the
    // last check.
    pub fn take_needs_newline(&self) -> bool {
        self.real_output_needs_newline.replace(false)
    }

    // Marks that an external process has just been spawned with its
    // stdout inherited from the real terminal -- see
    // ran_external_since_prompt's own doc comment. Called from the three
    // sites that actually do this (run_single's ordinary foreground
    // case, the `command` builtin, run_multi's pipeline stages), right
    // before spawning.
    fn note_external_spawn(&self) {
        self.ran_external_since_prompt.set(true);
    }

    // repl.rs's main loop calls this alongside take_needs_newline, right
    // before drawing the next prompt -- see ran_external_since_prompt's
    // own doc comment for why this needs a real terminal query
    // (term::query_cursor_column) rather than tracking bish's own writes
    // the way take_needs_newline does.
    pub fn take_ran_external(&self) -> bool {
        self.ran_external_since_prompt.replace(false)
    }

    // Read-only access to this session's own defined function names (not
    // bodies) -- `functions` itself stays private since callers have no
    // business touching the parsed Command bodies directly; this exists
    // for the syntax highlighter's command-validity check (bishedit::
    // highlight), which needs to know a function call is valid without
    // reaching into Shell's own execution internals.
    pub fn function_names(&self) -> impl Iterator<Item = &str> {
        self.functions.keys().map(String::as_str)
    }

    // repl.rs calls this once per session at promotion time (and
    // immediately for any session created afterward) to redirect that
    // session's output into its own VT100 grid instead of the real
    // terminal -- see OutputSink's doc comment.
    pub fn set_sink_grid(&mut self, screen: Rc<RefCell<vt100::Screen>>) {
        self.sink = OutputSink::Grid(screen);
    }

    // repl.rs's run_command_mode calls this immediately before running one
    // command, then calls set_sink_grid again immediately after to
    // restore -- see OutputSink::Capture's own doc comment.
    pub(crate) fn set_sink_capture(&mut self, buf: Rc<RefCell<String>>) {
        self.sink = OutputSink::Capture(buf);
    }

    /// Redirects output into `buf` and hands back what was there, for a
    /// caller that must put it *back* rather than assume what it was.
    ///
    /// `set_sink_capture` + `set_sink_grid` looks like the same thing
    /// and isn't: a session that hasn't been promoted writes to the real
    /// terminal, and "restoring" it to a grid nobody paints makes every
    /// later line vanish. Whoever borrows the sink is the only one who
    /// knows what it was.
    pub(crate) fn borrow_sink(&mut self, buf: Rc<RefCell<String>>) -> SavedSink {
        SavedSink(std::mem::replace(&mut self.sink, OutputSink::Capture(buf)))
    }

    pub(crate) fn return_sink(&mut self, saved: SavedSink) {
        self.sink = saved.0;
    }

    /// The exit status of the last command, for a caller that is about
    /// to run something the user didn't type and must put it back.
    ///
    /// Running a hook goes through `run_program`, which sets
    /// `last_status` like anything else -- so without this a hook
    /// becomes the answer to the user's next `$?`, and a
    /// `shell:cwd:change` hook could turn a successful command into a
    /// failed one from the prompt's point of view. Restoring it also
    /// makes `$?` *inside* a `shell:exec:post` hook mean the command's
    /// own status, which is the thing such a hook exists to look at.
    pub fn last_status(&self) -> i32 {
        self.last_status
    }

    pub fn set_last_status(&mut self, status: i32) {
        self.last_status = status;
    }

    // debugger.rs's own "hand the real terminal to the script" moment
    // (see DebugController::hand_off_to_script's own doc comment): a
    // statement about to run uninterrupted -- most visibly `read -p`'s
    // own prompt -- needs its builtin output to land on the real
    // terminal immediately, not sit invisibly in a captured buffer until
    // the debugger regains control long after the moment it mattered.
    pub(crate) fn set_sink_real(&mut self) {
        self.sink = OutputSink::Real;
    }

    // Undoes any `stdio_override` currently in place -- debugger.rs's own
    // "hand the real terminal to the script" moment (PauseState::
    // hand_off_to_script) calls this alongside set_sink_real so a
    // spawned external process's own stdout goes straight to the real
    // terminal too, consistent with builtin output during that same
    // window.
    pub(crate) fn clear_stdio_override(&mut self) {
        self.stdio_override = None;
    }

    // How big a freshly-opened pty (run_single's use_pty path) should be
    // sized before a full-screen program (vim, htop, less, ...) gets to
    // query it -- otherwise it inherits whatever posix_openpt's kernel
    // default winsize is (effectively unset), and falls back to some
    // small hardcoded default of its own, rendering into a tiny corner
    // of a screen that's visually much bigger. When this session's sink
    // is already Grid-backed, its own vt100::Screen's size *is* the
    // right answer: repl.rs keeps that screen resized to exactly this
    // session's current on-screen area (the whole promoted window, or
    // its own pane's rect once split -- see snapshot_window), and pty
    // attachment only ever happens once promoted anyway (see use_pty's
    // own gate), so this is the common, effectively only, case in
    // practice. Falls back to a live TIOCGWINSZ query against bish's
    // own controlling terminal for the (should be unreachable) Real
    // case, same default-on-failure as repl.rs's own query_term_size.
    fn pty_size(&self) -> (u16, u16) {
        // Through `sink_grid`, not off `self.sink` directly: a command
        // with its own output redirect runs under a `Builtin` sink, and
        // background_pty asks for a size in exactly that case -- read
        // straight, this fell through to the real-terminal query below
        // and sized a background job's pty to the whole terminal rather
        // than to the pane it will actually be rendered in.
        if let Some(screen) = self.sink_grid() {
            let (rows, cols) = screen.borrow().size();
            return (rows as u16, cols as u16);
        }
        // No grid at all (never promoted, or command mode's own transient
        // Capture) -- fall back to asking the real terminal rather than
        // guessing.
        match pty::get_size(0) {
            Ok(ws) if ws.rows > 0 && ws.cols > 0 => (ws.rows, ws.cols),
            _ => (24, 80),
        }
    }

    // The in-process session-cloning primitive: creates an independent but
    // related Shell, the way `window new` creates a virtual session
    // (rather than execing a fresh bish process). Unlike subshells/$(),
    // which self-exec a real child process specifically for *isolation*
    // (see run_subshell's doc comment), this is the opposite goal --
    // sharing jobs is the entire point, so a real child process -- with
    // its own PID, unable to share this process's Rc<RefCell<JobTable>>
    // -- would be the wrong tool. History sharing (every session's
    // commands landing in one interleaved timeline) isn't this
    // function's job at all -- repl.rs handles it directly via a shared
    // History plus a per-session boundary index, see history.rs.
    //
    // Variables/functions/aliases/options/cwd are a deep-cloned snapshot
    // of the parent at creation time: the two sessions start identical but
    // then evolve independently, matching a new terminal-multiplexer
    // window starting in the same directory with the same shell state.
    // jobs and promoted are shared (Rc::clone -- the same underlying
    // table/flag, not a copy), matching the plan's "job control tally is
    // shared across the shell sessions" requirement. Everything else
    // reset below is either non-cloneable (coproc_fds holds real pipe
    // fds, std::process::Child isn't Clone -- not applicable here but the
    // same reasoning) or transient/invocation-scoped state (local-scope
    // stacks, pending proc-subs, the current command's stderr-redirect
    // target) that belongs to whatever the *parent* is presently
    // executing, not to a brand-new session that hasn't run anything yet.
    //
    pub fn new_virtual_child(&self) -> Shell {
        Shell {
            last_status: 0,
            functions: self.functions.clone(),
            // A subshell sees what the shell that made it sees: its
            // positional parameters, and whatever `local` is in scope.
            // Both used to start empty here, so `$(... "$1" ...)` was
            // blank and a `local` was invisible to any substitution
            // inside the function that declared it -- while everything
            // *around* them (functions, arrays, aliases) was inherited
            // correctly, which is what made this look like a lookup bug
            // rather than a missing copy.
            //
            // One frame, not the whole stack: a subshell is a single
            // scope, and `shift` or `set` inside it must not reach a
            // caller's frame. It is a copy either way -- a virtual
            // child owns its own `Shell`.
            arg_frames: vec![self.arg_frames.last().cloned().unwrap_or_default()],
            var_scopes: self.var_scopes.clone(),
            script_name: self.script_name.clone(),
            running_a_script: self.running_a_script,
            arrays: self.arrays.clone(),
            assoc_arrays: self.assoc_arrays.clone(),
            invocation_flag: self.invocation_flag,
            unset_names: self.unset_names.clone(),
            array_names: self.array_names.clone(),
            assoc_names: self.assoc_names.clone(),
            aliases: self.aliases.clone(),
            abbrs: self.abbrs.clone(),
            mappings: self.mappings.clone(),
            disabled_builtins: self.disabled_builtins.clone(),
            call_stack: self.call_stack.clone(),
            function_sources: self.function_sources.clone(),
            hooks: self.hooks.clone(),
            next_hook_id: self.next_hook_id,
            lsp_servers: self.lsp_servers.clone(),
            next_lsp_id: self.next_lsp_id,
            lsp: Rc::clone(&self.lsp),
            history: Rc::clone(&self.history),
            firing_hooks: false,
            array_local_stack: Vec::new(),
            assoc_local_stack: Vec::new(),
            nameref_names: self.nameref_names.clone(),
            nameref_local_stack: Vec::new(),
            dir_stack: self.dir_stack.clone(),
            shopt_options: self.shopt_options.clone(),
            bishopts: self.bishopts.clone(),
            themes: self.themes.clone(),
            hl: self.hl.clone(),
            pending_theme: None,
            completions: self.completions.clone(),
            default_completion: self.default_completion.clone(),
            readonly_names: self.readonly_names.clone(),
            integer_names: self.integer_names.clone(),
            upper_names: self.upper_names.clone(),
            lower_names: self.lower_names.clone(),
            exported_names: self.exported_names.clone(),
            globals: self.globals.clone(),
            effects: self.effects,
            proc_sub_cleanup: Vec::new(),
            rng_state: fresh_rng_seed(),
            shell_start: std::time::Instant::now(),
            seconds_offset: 0,
            jobs: self.jobs.clone(),
            background_coroutines: Rc::clone(&self.background_coroutines),
            proc_sub_pipes: Vec::new(),
            proc_sub_children: Vec::new(),
            traps: self.traps.clone(),
            bash_command: self.bash_command.clone(),
            exit_trap: self.exit_trap.clone(),
            exit_trap_depth: self.exit_trap_depth,
            debug_trap: self.debug_trap.clone(),
            err_trap: self.err_trap.clone(),
            return_trap: self.return_trap.clone(),
            pseudo_trap_depth: self.pseudo_trap_depth,
            in_trap: self.in_trap,
            in_prompt_command: false,
            in_command_not_found: false,
            opt_functrace: self.opt_functrace,
            opt_errtrace: self.opt_errtrace,
            function_depth: 0,
            nesting_unwind: false,
            coproc_fds: std::collections::HashMap::new(),
            opt_errexit: self.opt_errexit,
            opt_nounset: self.opt_nounset,
            opt_xtrace: self.opt_xtrace,
            opt_pipefail: self.opt_pipefail,
            opt_noglob: self.opt_noglob,
            opt_monitor: self.opt_monitor,
            // Restricted mode is a shell-wide security property, not
            // something a subshell/command-substitution child should be
            // able to shed -- matches real bash, which keeps it in every
            // descendant.
            opt_restricted: self.opt_restricted,
            opt_noclobber: self.opt_noclobber,
            opt_posix: self.opt_posix,
            suppress_errexit: 0,
            errexit_exempt: false,
            last_subst_status: None,
            current_stderr_target: None,
            restrict_to_builtins: false,
            promoted: self.promoted.clone(),
            windows_available: self.windows_available,
            windows: self.windows.clone(),
            cwd: self.cwd.clone(),
            sink: OutputSink::Real,
            real_output_needs_newline: std::cell::Cell::new(false),
            ran_external_since_prompt: std::cell::Cell::new(false),
            pending_fg: None,
            pending_edit: None,
            pending_exit: None,
            interactive: self.interactive,
            expansion_failed: false,
            stdio_override: None,
            debug_hook: self.debug_hook.clone(),
            current_line: self.current_line,
            subshell_depth: self.subshell_depth + 1,
            // Shared with the parent (Rc), not re-collected from the
            // real process: it is equal to `self.env_snapshot` at this
            // exact instant regardless (new_virtual_child only ever runs
            // while `self` is the currently-synced-in session; see
            // sync_real_state_in/out's own doc comment), and only
            // session switching ever reads it -- a foreground `$( )`
            // child never does, so re-collecting `std::env::vars()`
            // here was pure cost on the hottest path in the shell.
            // Writers (`run_cd`, `set_terminal_capability_env`) go
            // through Rc::make_mut.
            env_snapshot: Rc::clone(&self.env_snapshot),
            umask_snapshot: current_umask(),
        }
    }

    // True once `window`-family promotion has switched the terminal into
    // full-screen mode -- repl.rs checks this at exit time to know whether
    // it needs to restore the normal screen buffer before quitting.
    pub fn is_promoted(&self) -> bool {
        self.promoted.get()
    }

    pub fn run_exit_trap(&mut self) {
        // Only for the exit of the shell that armed it. A subshell
        // inherits the trap and can still see it -- `trap -p EXIT`
        // inside one prints it, which is bash's behaviour too -- but
        // reaching the end of a subshell is not the exit it was set
        // for.
        //
        // Running it there is silently destructive, because the shape
        // this trap is nearly always written in is a cleanup:
        //
        //     tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
        //
        // The very next command substitution ended a subshell, so the
        // directory was removed while the script was still using it --
        // and then again at every `( )`, every pipeline stage, and once
        // more at the real exit.
        if self.subshell_depth != self.exit_trap_depth {
            return;
        }
        if let Some(cmd) = self.exit_trap.take() {
            self.run_source_here(&cmd, "trap");
        }
    }

    /// Whether a `command_not_found_handle` function is defined and
    /// could be called right now.
    ///
    /// The "right now" matters: the handler is itself a command, and a
    /// handler that mistypes something would otherwise call itself
    /// forever.
    fn has_command_not_found_handler(&self) -> bool {
        !self.in_command_not_found && self.functions.contains_key(COMMAND_NOT_FOUND_HANDLER)
    }

    /// bash's `command_not_found_handle`: the hook every distribution's
    /// "command not found -- install package X" integration needs.
    ///
    /// Called with the whole failed command line as its positional
    /// parameters, and its own exit status becomes the shell's -- so a
    /// handler that installs and re-runs the thing can report success,
    /// and one that only prints advice can still return 127.
    fn run_command_not_found_handler(&mut self, argv: &[String]) -> ExecResult {
        let Some(body) = self.functions.get(COMMAND_NOT_FOUND_HANDLER).cloned() else {
            return ExecResult::Status(127);
        };
        self.in_command_not_found = true;
        let result = self.call_function(&argv[0], &body, argv.to_vec());
        self.in_command_not_found = false;
        result
    }

    /// Runs `PROMPT_COMMAND` before drawing a primary prompt.
    ///
    /// The conventional way to make a prompt dynamic without inventing a
    /// format language, and what every prompt tool in the ecosystem
    /// hooks into. bash 5.1 lets it be an array, each element run in
    /// turn, and that costs nothing to honour here.
    ///
    /// Its output is deliberately *not* captured, unlike a `::bish
    /// hook`'s: printing is most of what a `PROMPT_COMMAND` is for --
    /// a title escape, a blank line, a right-hand clock -- so it writes
    /// wherever this session's output already goes.
    ///
    /// `$?` is borrowed and given back: the command did not run because
    /// the user typed it, and a prompt hook that quietly rewrote the
    /// status of what they *did* type would break the next prompt that
    /// tried to colour itself by it.
    pub fn run_prompt_command(&mut self) {
        if self.in_prompt_command {
            return;
        }
        let mut bodies: Vec<String> = match self.arrays.get("PROMPT_COMMAND") {
            Some(map) if !map.is_empty() => map.values().cloned().collect(),
            _ => vec![self.lookup_var("PROMPT_COMMAND")],
        };
        bodies.retain(|b| !b.trim().is_empty());
        if bodies.is_empty() {
            return;
        }
        let status = self.last_status;
        self.in_prompt_command = true;
        for body in bodies {
            self.run_source_here(&body, "PROMPT_COMMAND");
        }
        self.in_prompt_command = false;
        self.last_status = status;
    }

    /// Runs one of the pseudo-signal traps, if it is set.
    ///
    /// `$?` is borrowed and given back around it, exactly as
    /// `run_hooks_for` does: the trap did not run because the user typed
    /// it, and a DEBUG trap that quietly changed the status of the
    /// command it fired before would be worse than no trap at all.
    ///
    /// Re-entry is refused rather than counted: a trap's body is
    /// commands, and those would fire the same trap again.
    fn run_pseudo_trap(&mut self, which: PseudoTrap) {
        if self.in_trap {
            return;
        }
        // Not inherited by a shell function unless the matching option
        // says so -- bash's own rule, and the reason a trap set at the
        // top level fires once for a failing function call rather than
        // once inside and once outside.
        let inherited = match which {
            PseudoTrap::Err => self.opt_errtrace,
            PseudoTrap::Debug | PseudoTrap::Return => self.opt_functrace,
        };
        // ERR keeps the older, coarser gate. bash fires it once per
        // failure, at the innermost level where a trap is active, and
        // does *not* fire it again for the function call that failure
        // propagates out of -- which the depth rule alone would do.
        let deeper = match which {
            PseudoTrap::Err => self.function_depth > 0,
            PseudoTrap::Debug | PseudoTrap::Return => self.function_depth > self.pseudo_trap_depth[which as usize],
        };
        if deeper && !inherited {
            return;
        }
        let Some(cmd) = (match which {
            PseudoTrap::Debug => self.debug_trap.clone(),
            PseudoTrap::Err => self.err_trap.clone(),
            PseudoTrap::Return => self.return_trap.clone(),
        }) else {
            return;
        };
        let saved = self.last_status;
        self.in_trap = true;
        self.run_source_here(&cmd, "trap");
        self.in_trap = false;
        self.last_status = saved;
    }

    // Real bash enables job control (`-m`/monitor) by default for an
    // interactive shell, no explicit `set -m` needed -- only a
    // non-interactive script has to opt in. repl.rs calls this once for
    // the root session at interactive startup, matching that; every
    // `window new` virtual child then inherits it automatically the same
    // way it inherits every other opt_* flag (see new_virtual_child).
    pub fn enable_monitor_mode(&mut self) {
        self.opt_monitor = true;
        self.interactive = true;
        crate::term::ignore_tty_signals();
    }

    // repl.rs's EOF handler uses this to decide whether to warn ("There
    // are stopped jobs.") instead of exiting outright -- matching real
    // bash's own behavior of refusing a plain Ctrl-D exit the first time
    // there's a stopped job, requiring a second immediate EOF to
    // actually confirm. Deliberately checks *stopped* jobs only, not
    // merely-running background ones: real bash doesn't warn about
    // those on exit either (by default, without huponexit even).
    pub fn has_stopped_jobs(&self) -> bool {
        self.jobs.borrow().jobs.iter().any(|j| j.stopped)
    }

    // Resolves a job spec (%N, %%/%+  current, %-  previous, %name  prefix
    // match on the job's command text) to an index into self.jobs. Bare
    // job numbers without the `%` are also accepted, matching bash's own
    // leniency in `fg`/`bg`/`wait`.
    // The session's own grid, looked through whatever sink is temporarily
    // installed on top of it. Reading `self.sink` directly got this
    // wrong in exactly the case that matters here: a command with its own
    // output redirect runs under a `Builtin` sink for its duration (see
    // that variant's own doc comment), so `cmd >file &` recorded no grid
    // at all and the stream it *didn't* redirect stayed invisible. The
    // `Builtin` sink keeps what it displaced, so following that chain
    // finds the real one.
    //
    // `None` for a session that was never promoted (nothing to feed) and
    // for command mode's `Capture` (its output is a transient overlay,
    // not the pane's scrollback -- and it can't start background jobs
    // anyway, being restricted to builtins).
    fn sink_grid(&self) -> Option<Rc<RefCell<vt100::Screen>>> {
        let mut sink = &self.sink;
        loop {
            match sink {
                OutputSink::Grid(screen) => return Some(screen.clone()),
                OutputSink::Builtin { previous, .. } => sink = previous,
                OutputSink::Real | OutputSink::Capture(_) => return None,
            }
        }
    }

    // A pty for a *background* job's stdio, sized to this session's own
    // on-screen area. `None` when this session isn't promoted (there's no
    // grid to feed, and inherited stdio already lands where it should) or
    // the pty can't be opened, in which case every caller keeps its
    // previous behavior exactly.
    //
    // Why a pty and not a plain pipe: the child's own isatty() then
    // answers what it would have with the inherited terminal, so `ls &`
    // keeps its colors -- and, more importantly, `fg` on the job can go
    // through the same drive_fg_job rendering path a foreground command
    // already uses, rather than a blocking wait with nothing draining the
    // far end.
    fn background_pty(&self) -> Option<pty::Pty> {
        if !self.is_promoted() {
            return None;
        }
        let p = pty::open().ok()?;
        let (rows, cols) = self.pty_size();
        let _ = p.resize(rows, cols);
        Some(p)
    }

    // Reads whatever the pty-backed background jobs have produced since
    // the last call and feeds each one into the grid of the session that
    // started it (Job::sink_screen). Returns true iff anything arrived,
    // which is the caller's cue to repaint.
    //
    // Without this a background job's output simply never appeared: the
    // pty buffer filled up invisibly and only spilled onto the screen if
    // the job was later `fg`'d. The job table is shared by every session
    // (see the `jobs` field's own doc comment), so one call from any live
    // shell services all of them -- which is exactly why each job records
    // its own destination rather than this trying to work one out.
    pub(crate) fn drain_background_output(&self) -> bool {
        use std::io::Read;
        use std::os::unix::io::AsRawFd;
        // Bounded per call, the same way repl.rs's own job-pty drain is:
        // a firehose in the background must not be able to hold up
        // whichever interactive loop called this.
        const MAX_READS_PER_TICK: u32 = 16;
        let mut buf = [0u8; 4096];
        let mut fed = false;
        let mut table = self.jobs.borrow_mut();
        for job in &mut table.jobs {
            let (Some(master), Some(screen)) = (&mut job.pty_master, &job.sink_screen) else {
                continue;
            };
            // An editor frame has this session's screen switched to its
            // alternate buffer for as long as it owns the pane (see
            // repl.rs's enter_alt_screen), and that buffer is the
            // editor's, not this job's -- feeding it here would scribble
            // over the file being edited. The output waits in the pty
            // until the frame closes, which is also when the primary
            // buffer is showing again and there's somewhere for it to go.
            if screen.borrow().using_alternate {
                continue;
            }
            if !job.nonblocking {
                // Lazily, so no spawn site has to think about it: a
                // blocking read here would park the whole shell on a job
                // that simply has nothing to say yet.
                pty::set_nonblocking(master.as_raw_fd());
                job.nonblocking = true;
            }
            for _ in 0..MAX_READS_PER_TICK {
                match master.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        screen.borrow_mut().feed(&buf[..n]);
                        fed = true;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        fed
    }

    // Registers a freshly spawned background job (one child for a plain
    // backgrounded command, several for a backgrounded pipeline) in the
    // job table, and updates $! to the last child's PID (bash's own
    // convention for a backgrounded pipeline).
    fn push_job(&mut self, children: Vec<std::process::Child>, cmd_text: String) {
        self.push_job_with_pty(children, cmd_text, None);
    }

    // Same as push_job, but additionally records the pty master (if any)
    // that job's stdio is attached to -- see Job::pty_master's doc
    // comment. Only the single-external-command background-spawn site
    // (run_single) ever passes Some here. Never a real process-group
    // job -- see push_job_with_pgid for that.
    fn push_job_with_pty(&mut self, children: Vec<std::process::Child>, cmd_text: String, pty_master: Option<std::fs::File>) {
        self.push_job_full(children, cmd_text, pty_master, None);
    }

    fn push_job_full(&mut self, children: Vec<std::process::Child>, cmd_text: String, pty_master: Option<std::fs::File>, pgid: Option<u32>) {
        let pids: Vec<u32> = children.iter().map(|c| c.id()).collect();
        let mut table = self.jobs.borrow_mut();
        table.last_bg_pid = pids.last().copied();
        let id = table.next_job_id;
        table.next_job_id += 1;
        let sink_screen = self.sink_grid();
        table.jobs.push(Job { id, pids, children, cmd_text, pty_master, sink_screen, nonblocking: false, pgid, stopped: false });
    }

    pub(crate) fn resolve_job_spec(&self, spec: &str) -> Option<usize> {
        let table = self.jobs.borrow();
        let rest = spec.strip_prefix('%').unwrap_or(spec);
        if rest.is_empty() || rest == "%" || rest == "+" {
            return if table.jobs.is_empty() { None } else { Some(table.jobs.len() - 1) };
        }
        if rest == "-" {
            return if table.jobs.len() >= 2 { Some(table.jobs.len() - 2) } else { None };
        }
        if let Ok(n) = rest.parse::<u32>() {
            return table.jobs.iter().position(|j| j.id == n);
        }
        table.jobs.iter().position(|j| j.cmd_text.starts_with(rest))
    }

    // Safety net for the one place ExecResult::Fg can arise that repl.rs
    // doesn't drive it from directly: command mode (`:fg`) has its own
    // separate, nested read-eval loop (run_command_mode) that can't hand
    // off to the outer compositor the way a normal insert-mode command
    // can (see command_mode's own doc comment) -- rather than leaving a
    // job orphaned on self.pending_fg, repl.rs calls this to reap it with
    // an ordinary blocking wait instead, same as if it had never had a
    // pty attached at all.
    pub fn discard_pending_fg(&mut self) -> i32 {
        match self.pending_fg.take() {
            Some(mut job) => job.wait(),
            None => 0,
        }
    }

    // Hands ownership of a job stashed via run_fg's ExecResult::Fg to
    // repl.rs, wrapped so its private fields (Job holds a
    // std::process::Child, which isn't the kind of thing exec.rs wants
    // to expose directly) stay inaccessible outside this module -- see
    // FgJob's own doc comment for why repl.rs needs to own this at all
    // rather than exec.rs continuing to drive it internally.
    pub fn take_pending_fg(&mut self) -> Option<FgJob> {
        self.pending_fg.take().map(FgJob)
    }

    // See ExecResult::Edit's own doc comment. `unwrap_or_default`
    // collapses "`e` was never run" into the same empty vector a bare
    // `e` (no arguments -- a fresh unnamed buffer) produces: repl.rs
    // only ever calls this in direct response to just having received
    // ExecResult::Edit, so the former is unreachable in practice, and a
    // caller who already knows it's reacting to that exact signal has no
    // use for telling the two apart.
    pub fn take_pending_edit(&mut self) -> Vec<String> {
        self.pending_edit.take().unwrap_or_default()
    }

    // See `pending_exit`'s own doc comment. Runs the exit trap exactly
    // once, at the point the violation is actually acted on (not inside
    // check_nounset itself, which runs from many different, sometimes
    // re-entrant, expansion contexts).
    fn take_pending_exit(&mut self) -> Option<ExecResult> {
        self.pending_exit.take().map(|code| {
            self.run_exit_trap();
            ExecResult::Exit(code)
        })
    }

    // The `Stdio::inherit()` fallback for a spawned external command's own
    // stdout, honoring `stdio_override` (see its own doc comment) when
    // set. Every real-process spawn site's `redirs.stdout.unwrap_or_else
    // (Stdio::inherit)`-style fallback should go through this instead, so
    // a converted foreground subshell/command-substitution/proc-sub still
    // captures an external command's output correctly even though it's no
    // longer itself a separate OS process with its own real fd 1.
    /// A `Command` that will inherit *this shell's* environment rather
    /// than whatever happens to be in the process's.
    ///
    /// Every place the shell starts a process goes through here. That
    /// is not a convention: `every_process_the_shell_starts_goes_
    /// through_command` reads this crate's own source and fails on a
    /// `Command::new` anywhere that is not either this function or an
    /// entry in its allow list -- with a reason written next to it. A
    /// spawn that misses this gets a stale environment and says
    /// nothing about it, which is exactly the kind of mistake that is
    /// worth making impossible rather than remembering not to make.
    pub(crate) fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.env_clear();
        command.envs(self.exported_pairs());
        // The shell's cwd, for the same reason as its variables: the
        // process's is shared, and two shells in one process can be in
        // different directories. A caller that wants somewhere else
        // still says so afterwards.
        command.current_dir(&self.cwd);
        command
    }

    /// Every exported variable and its value, as a child should see it.
    ///
    /// Resolved with the same precedence a `$name` gets, so an exported
    /// `local` shadows the global of the same name. NUL-bearing names
    /// and values are dropped rather than passed: a C environment
    /// string cannot hold one, and the variable is worth more than the
    /// export (see `export_to_environment`, which draws the same line).
    /// Borrowed, not cloned: this runs on every spawn, and building it
    /// out of owned `String`s allocated twice per exported variable
    /// each time -- about 140 allocations to start one external
    /// command, which is more work than starting it.
    fn exported_pairs(&self) -> Vec<(&str, &str)> {
        self.exported_names
            .iter()
            .filter(|name| !name.contains('\0'))
            .filter_map(|name| {
                let value = self.lookup_var_for_export(name)?;
                (!value.contains('\0')).then_some((name.as_str(), value))
            })
            .collect()
    }

    /// The value a child should see for `name`, or `None` if it has
    /// none -- an exported name that is not set exports nothing, the
    /// same as in bash.
    fn lookup_var_for_export(&self, name: &str) -> Option<&str> {
        for scope in self.var_scopes.iter().rev() {
            if let Some(value) = scope.get(name) {
                return value.as_deref();
            }
        }
        self.globals.get(name).map(String::as_str)
    }

    fn spawn_stdout_stdio(&self) -> Stdio {
        match &self.stdio_override {
            Some(o) => match &o.borrow().stdout {
                Some(f) => match f.try_clone() {
                    Ok(f) => Stdio::from(f),
                    Err(_) => Stdio::inherit(),
                },
                None => Stdio::inherit(),
            },
            None => Stdio::inherit(),
        }
    }

    /// Where an *external* command inside a redirected construct should
    /// send its stderr -- the mirror of ChildStdio::sink_stderr, which
    /// answers the same question for a builtin.
    fn effective_stderr(&self, stdio: &ChildStdio, effective_stdout: &Option<std::fs::File>) -> Option<std::fs::File> {
        let own = match stdio.err_follows_out {
            // `2>&1` before this construct rebound fd 1: whatever fd 1
            // is for what runs inside it.
            Some(Follows::Outer) => effective_stdout.as_ref().and_then(|f| f.try_clone().ok()),
            Some(Follows::OwnFile) => stdio.stdout.as_ref().and_then(|f| f.try_clone().ok()),
            None => stdio.stderr.as_ref().and_then(|f| f.try_clone().ok()),
        };
        own.or_else(|| self.stdio_override.as_ref().and_then(|o| o.borrow().stderr.as_ref().and_then(|f| f.try_clone().ok())))
    }

    /// The same for stderr -- see StdioOverride::stderr.
    fn spawn_stderr_stdio(&self) -> Stdio {
        match &self.stdio_override {
            Some(o) => match &o.borrow().stderr {
                Some(f) => match f.try_clone() {
                    Ok(f) => Stdio::from(f),
                    Err(_) => Stdio::inherit(),
                },
                None => Stdio::inherit(),
            },
            None => Stdio::inherit(),
        }
    }

    // Same as spawn_stdout_stdio, for stdin.
    fn spawn_stdin_stdio(&self) -> Stdio {
        match &self.stdio_override {
            Some(o) => match &o.borrow().stdin {
                // Any bytes already sitting in `pending` (fetched by this
                // shell's own `read` but not yet consumed) are invisible
                // to a spawned external process, which reads the real fd
                // directly -- an accepted, pre-existing-shape limitation
                // (real re-exec'd children have the exact same gap today
                // via std::io::stdin()'s own read-ahead buffering; this
                // isn't a regression this conversion introduces).
                Some(r) => match r.borrow().file.try_clone() {
                    Ok(f) => Stdio::from(f),
                    Err(_) => Stdio::inherit(),
                },
                None => Stdio::inherit(),
            },
            None => Stdio::inherit(),
        }
    }

    // The pty-attached counterpart to run_single's own inline Stopped-job
    // registration for the non-pty foreground path: called by repl.rs's
    // drive_fg_job loop when FgJob::poll_untraced reports the job it was
    // driving has stopped (Ctrl-Z forwarded into the job's own pty, or an
    // explicit `kill -STOP`) rather than exited. Unlike that job, this
    // one was never in the job table at all (it bubbled straight from
    // spawn to ExecResult::Fg, on the assumption it'd just run and exit
    // -- see run_single's own comment on why `id: 0` there is a
    // throwaway placeholder) -- assigns it a real one now that it
    // actually needs to be `jobs`/`fg`/`bg`-addressable. Returns the id
    // and cmd_text so the caller can print the bash-style "[N]+  Stopped
    // ..." message itself (repl.rs owns which session's grid that
    // belongs in, not this module; Job::cmd_text isn't otherwise
    // readable from outside it either).
    pub fn park_stopped_fg_job(&mut self, job: FgJob) -> (u32, String) {
        let mut job = job.0;
        job.stopped = true;
        let mut table = self.jobs.borrow_mut();
        let id = table.next_job_id;
        table.next_job_id += 1;
        job.id = id;
        let cmd_text = job.cmd_text.clone();
        table.jobs.push(job);
        (id, cmd_text)
    }

    // declare -f [name...] / declare -F [name...]: print each named
    // function's definition (or, under -F, just "declare -f NAME"); no
    // names means every currently-defined function, sorted for
    // deterministic output (real bash prints in definition order --
    // sorting is simpler and good enough for what this is mainly used
    // for: introspection/completion scripts, not byte-for-byte diffing
    // against real bash). Reuses the exact same Command::FuncDef ->
    // serialize_program round-trip functions_preamble already does for
    // re-declaring functions across a subprocess boundary.
    pub(crate) fn print_functions(&mut self, names: &[String], names_only: bool) -> i32 {
        let targets: Vec<String> = if names.is_empty() {
            let mut all: Vec<String> = self.functions.keys().cloned().collect();
            all.sort();
            all
        } else {
            names.to_vec()
        };
        let mut status = 0;
        for name in targets {
            match self.functions.get(&name).cloned() {
                Some(body) => {
                    if names_only {
                        // `declare -F name` prints the bare name;
                        // `declare -F` with no names prints a
                        // `declare -f NAME` line for each, which is
                        // what makes the listing re-readable. Printing
                        // the long form either way answered a question
                        // about one function with a declaration of it.
                        match names.is_empty() {
                            true => sh_println!(self, "declare -f {}", name),
                            false => sh_println!(self, "{}", name),
                        }
                    } else {
                        let def = parser::Command::FuncDef { name: name.clone(), body: Box::new(body) };
                        let src = crate::serialize::serialize_program(&[ListItem {
                            and_or: AndOr { first: Pipeline { commands: vec![def], negate: false, timed: None }, rest: Vec::new() },
                            sep: Sep::Seq,
                            line: 0,
                        }]);
                        // Without the separator `serialize_program`
                        // puts after every item. A function definition
                        // needs no terminator, and the one idiom this
                        // output exists for puts a command straight
                        // after it: `sh -c "$(declare -f f); f"` became
                        // `};; f`, which is a syntax error in either
                        // shell. bash ends its own at the `}`.
                        let src = src.trim_end();
                        sh_println!(self, "{}", src.strip_suffix(';').unwrap_or(src));
                    }
                }
                None => {
                    // Matches real bash: `declare -f`/`-F` on a name that
                    // isn't a function just fails silently (no message),
                    // unlike `declare -p` on a nonexistent variable below,
                    // which does print one -- confirmed against real bash.
                    status = 1;
                }
            }
        }
        status
    }

    // declare -p [name...]: print each variable's current declare-style
    // representation; no names means every currently-visible variable/
    // array (globals + any locals in the current function scope),
    // sorted for deterministic output. Unlike real bash, a variable that
    // was `declare -i`'d but never actually assigned a value can't be
    // distinguished from "doesn't exist" here (bish has no separate
    // "declared but unset" state) -- such a variable just doesn't appear.
    pub(crate) fn print_declared(&mut self, names: &[String]) -> i32 {
        let targets: Vec<String> = if names.is_empty() { self.var_names_with_prefix("") } else { names.to_vec() };
        let mut status = 0;
        for name in targets {
            match self.declare_p_line(&name) {
                Some(line) => sh_println!(self, "{}", line),
                None => {
                    sh_eprintln!(self, "bish: declare: {}: not found", name);
                    status = 1;
                }
            }
        }
        status
    }

    // The attribute-flag letters bash's own declare -p/${v@a}/${v@A} all
    // use, in the same fixed order: array-ness (A/a, mutually exclusive)
    // first, then i/r/x/n/u/l. Shared by declare_p_line (declare -p's
    // own output), transform_attributes (${v@A}), and, standing alone,
    // ${v@a} itself.
    fn attribute_flags_string(&self, name: &str) -> String {
        let mut flags = String::new();
        if self.assoc_names.contains(name) {
            flags.push('A');
        } else if self.arrays.contains_key(name) || self.array_names.contains(name) {
            flags.push('a');
        }
        if self.integer_names.contains(name) {
            flags.push('i');
        }
        if self.readonly_names.contains(name) {
            flags.push('r');
        }
        if self.exported_names.contains(name) {
            flags.push('x');
        }
        if self.nameref_names.contains(name) {
            flags.push('n');
        }
        if self.upper_names.contains(name) {
            flags.push('u');
        }
        if self.lower_names.contains(name) {
            flags.push('l');
        }
        flags
    }

    // ${v@A}: an assignment/`declare` statement that would recreate the
    // named variable, matching real bash's own (slightly inconsistent)
    // formatting rules -- confirmed against real bash:
    // - A full array/assoc reconstruction (single_element is None and
    //   the name is array/assoc) is identical to declare -p's own
    //   output (double-quoted elements, always "declare -a"/"-A"
    //   prefixed) -- bash's own bare `${arr@A}` (no subscript) actually
    //   collapses to just the first element instead, a quirk this
    //   deliberately doesn't replicate since reconstructing the *whole*
    //   array is what a script reaching for @A almost certainly wants
    //   (bish also doesn't collapse a bare `$arr` to `${arr[0]}` at
    //   all, a separate pre-existing simplification this doesn't fix
    //   either).
    // - Anything else (a plain scalar, or one specific array/assoc
    //   index via `single_element`) is single-quoted, with a leading
    //   "declare -flags" only when there's at least one real attribute
    //   -- a bare `name='value'` otherwise (unlike declare -p, which
    //   always prefixes even a plain scalar with "declare --"). An
    //   array/assoc name still shows its own -a/-A flag here even for
    //   one specific element, matching bash's own `${arr[0]@A}` ->
    //   `declare -a arr='1'`.
    fn transform_attributes(&mut self, name: &str, single_element: Option<&str>) -> String {
        let is_array = self.assoc_names.contains(name) || self.arrays.contains_key(name) || self.array_names.contains(name);
        if is_array && single_element.is_none() {
            return self.declare_p_line(name).unwrap_or_default();
        }
        let flags = self.attribute_flags_string(name);
        let value = match single_element {
            Some(v) => v.to_string(),
            None => self.lookup_var(name),
        };
        let quoted = crate::serialize::quote_literal(&value);
        if flags.is_empty() { format!("{name}={quoted}") } else { format!("declare -{flags} {name}={quoted}") }
    }

    // ${arr[@]@K}/${assoc[@]@K}: "key value key value ..." pairs,
    // values double-quoted the same way declare -p's own array elements
    // are. array_keys/array_all iterate the same underlying map, so
    // zipping them together pairs each key with its own value.
    fn array_key_value_pairs(&self, name: &str) -> String {
        let keys = self.array_keys(name);
        let values = self.array_all(name);
        keys.iter().zip(values.iter()).map(|(k, v)| format!("{k} {}", declare_p_quote(v))).collect::<Vec<_>>().join(" ")
    }

    // ${v@P}: expands `s` as if it were a PS1-style prompt string --
    // bash's own backslash escapes, computed fresh from this shell's
    // live state (real local time, real hostname/cwd/job count, ...).
    // Deliberately standalone from bish's own actual prompt (prompt.rs
    // stays exactly as hardcoded as it already was) -- this exists
    // purely as an on-request value transform, the same as every other
    // `${v@X}` operator, not a step toward a live PS1-driven prompt.
    // `\!`/`\#` (history/command number) always read "0": that state
    // lives in repl.rs's SessionState, not exec::Shell, and isn't
    // threaded down here -- a narrower, documented gap rather than
    // plumbing an unrelated module's state into this one for it.
    fn expand_prompt_string(&self, s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            let Some(&next) = chars.peek() else {
                out.push('\\');
                break;
            };
            // \nnn: 1-3 octal digits.
            if next.is_digit(8) {
                let mut digits = String::new();
                while digits.len() < 3 {
                    match chars.peek() {
                        Some(&d) if d.is_digit(8) => {
                            digits.push(d);
                            chars.next();
                        }
                        _ => break,
                    }
                }
                if let Some(c) = u32::from_str_radix(&digits, 8).ok().and_then(char::from_u32) {
                    out.push(c);
                }
                continue;
            }
            chars.next();
            match next {
                'a' => out.push('\x07'),
                'e' => out.push('\x1b'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                '\\' => out.push('\\'),
                // Non-printing-sequence brackets: stripped entirely,
                // matching real bash's own ${v@P} (confirmed -- they're
                // only meaningful for the live prompt's own cursor-
                // column bookkeeping, which doesn't apply here).
                '[' | ']' => {}
                '$' => out.push(if is_effective_root() { '#' } else { '$' }),
                'u' => out.push_str(&prompt_username()),
                'h' => out.push_str(get_hostname().split('.').next().unwrap_or("")),
                'H' => out.push_str(&get_hostname()),
                'w' => out.push_str(&self.prompt_cwd(false)),
                'W' => out.push_str(&self.prompt_cwd(true)),
                's' => out.push_str("bish"),
                'v' => out.push_str("5.2"),
                'V' => out.push_str("5.2.21"),
                'j' => out.push_str(&self.jobs.borrow().jobs.len().to_string()),
                '!' | '#' => out.push('0'),
                'l' => out.push_str(&tty_basename()),
                'd' => out.push_str(&crate::time::prompt_date()),
                't' => out.push_str(&crate::time::strftime("%H:%M:%S", &crate::time::local_time_now())),
                'T' => out.push_str(&crate::time::strftime("%I:%M:%S", &crate::time::local_time_now())),
                '@' => out.push_str(&crate::time::strftime("%I:%M %p", &crate::time::local_time_now())),
                'A' => out.push_str(&crate::time::strftime("%H:%M", &crate::time::local_time_now())),
                'D' if chars.peek() == Some(&'{') => {
                    chars.next();
                    let mut fmt = String::new();
                    for d in chars.by_ref() {
                        if d == '}' {
                            break;
                        }
                        fmt.push(d);
                    }
                    let fmt = if fmt.is_empty() { "%a %b %e %H:%M:%S %Y" } else { &fmt };
                    out.push_str(&crate::time::strftime(fmt, &crate::time::local_time_now()));
                }
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
        }
        out
    }

    // `\w`/`\W`: the real bash forms (full path with `~` home
    // substitution, or just its basename) -- deliberately not
    // prompt.rs's own shorten_path, which uses a different, bish-
    // specific abbreviation convention for its own hardcoded prompt.
    fn prompt_cwd(&self, basename_only: bool) -> String {
        let home = std::env::var("HOME").unwrap_or_default();
        let cwd = self.cwd.to_string_lossy();
        let display = if !home.is_empty() && (cwd == home || cwd.starts_with(&format!("{home}/"))) {
            format!("~{}", &cwd[home.len()..])
        } else {
            cwd.to_string()
        };
        if basename_only {
            if display == "~" || display == "/" {
                return display;
            }
            display.rsplit('/').next().unwrap_or(&display).to_string()
        } else {
            display
        }
    }

    fn declare_p_line(&mut self, name: &str) -> Option<String> {
        let flags = self.attribute_flags_string(name);
        let flag_str = if flags.is_empty() { "--".to_string() } else { format!("-{flags}") };

        // A name with the attribute and no map has been *declared* and
        // not assigned, which bash prints with no `=` at all -- it falls
        // through to the value-less line further down. An empty map is a
        // different thing: `M=()` assigned it, and prints `=()`.
        if let Some(map) = self.assoc_names.contains(name).then(|| self.assoc_arrays.get(name)).flatten() {
            let mut body = String::new();
            for (k, v) in map.iter() {
                body.push('[');
                body.push_str(k);
                body.push_str("]=");
                body.push_str(&declare_p_quote(v));
                body.push(' ');
            }
            // The trailing space is bash's, and only for an
            // associative array -- an indexed one is trimmed. Confirmed
            // against real bash, quirk and all, because `declare -p`
            // output is meant to be re-read.
            return Some(format!("declare {} {}=({})", flag_str, name, body));
        }
        if let Some(items) = self.arrays.get(name) {
            let mut body = String::new();
            for (idx, v) in items {
                body.push('[');
                body.push_str(&idx.to_string());
                body.push_str("]=");
                body.push_str(&declare_p_quote(v));
                body.push(' ');
            }
            return Some(format!("declare {} {}=({})", flag_str, name, body.trim_end()));
        }
        if !self.var_is_set(name) {
            // A name can carry attributes without carrying a value:
            // `export E` on an unset `E` records that it is to be
            // exported if it is ever set, and bash prints that as
            // `declare -x E`, with no `=` at all. Only "no attributes
            // either" is genuinely not found.
            if flags.is_empty() {
                return None;
            }
            return Some(format!("declare {} {}", flag_str, name));
        }
        let value = self.lookup_var(name);
        Some(format!("declare {} {}={}", flag_str, name, declare_p_quote(&value)))
    }

    // Effective on/off state for a known shopt option name: an explicit
    // `-s`/`-u` override if there's been one this session, else that
    // name's own default from KNOWN_SHOPT_OPTIONS. `extglob` is special-
    // cased to always report "on" regardless of either, since bish's
    // extglob support is unconditional (see glob.rs) rather than actually
    // gated by this flag.
    /// Substitutes this shell's aliases into a tokenized line, when
    /// `shopt -s expand_aliases` says to.
    ///
    /// Public so every place that turns text into a `Program` can go
    /// through it -- there are several (`eval`, `source`, a script,
    /// the interactive prompt, command mode) and an alias that worked
    /// in one and not the others would be worse than none at all.
    /// Turns a shopt on from Rust, for a default a *mode* implies rather
    /// than a user does -- `expand_aliases` for an interactive shell,
    /// which is bash's own rule and not something anyone should have to
    /// write in a config file.
    pub fn enable_shopt(&mut self, name: &str) {
        self.shopt_options.insert(name.to_string(), true);
    }

    pub fn expand_aliases(&self, toks: Vec<(crate::lexer::Tok, usize)>) -> Vec<(crate::lexer::Tok, usize)> {
        if !self.shopt_is_on("expand_aliases") || self.aliases.is_empty() {
            return toks;
        }
        crate::lexer::expand_aliases(toks, &|name| self.aliases.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone()))
    }

    pub(crate) fn shopt_is_on(&self, name: &str) -> bool {
        if name == "extglob" {
            return true;
        }
        self.shopt_options.get(name).copied().unwrap_or_else(|| shopt_default_on(name).unwrap_or(false))
    }

    pub(crate) fn print_shopt_line(&mut self, name: &str, reusable: bool) {
        let on = self.shopt_is_on(name);
        if reusable {
            sh_println!(self, "shopt -{} {}", if on { "s" } else { "u" }, name);
        } else {
            // 20, then a tab -- bash's own column, and the reason
            // `shopt | grep` scripts line up the same way there.
            sh_println!(self, "{:<20}\t{}", name, if on { "on" } else { "off" });
        }
    }

    // Effective value for a known bishopt name, in priority order: (1) its
    // own explicit override, if one's been set; (2) else, unless `name`
    // itself is "theme" (a theme naming itself would be circular and
    // meaningless), whatever the currently active theme (the "theme"
    // bishopt's own value, if any) declared for this name -- see
    // run_bish_theme_end's own doc comment for how a theme's declared
    // opts get there; (3) else that option's own registered default
    // (always `false` for a Bool -- there's no separate stored default
    // for booleans, see KNOWN_BISHOPTS' own doc comment). `None` means
    // `name` isn't a registered option at all. A Color default is CSS
    // source text, parsed the same way a `--set` value is -- `.expect()`
    // on failure is deliberate: an unparseable *registered* default is a
    // bug in KNOWN_BISHOPTS itself, not something a user can trigger.
    pub(crate) fn bishopt_value(&self, registry: &[(&str, BishOptDefault)], name: &str) -> Option<BishOptValue> {
        let default = registry.iter().find(|(n, _)| *n == name)?.1.clone();
        if let Some(v) = self.bishopts.get(name) {
            return Some(v.clone());
        }
        if name != "theme"
            && let Some(BishOptValue::Str(active)) = self.bishopts.get("theme")
            && let Some(v) = self.themes.get(active).and_then(|theme| theme.opts.get(name))
        {
            return Some(v.clone());
        }
        Some(match default {
            BishOptDefault::Bool(b) => BishOptValue::Bool(b),
            BishOptDefault::Int(n, _) => BishOptValue::Int(n),
            BishOptDefault::Str(s) => BishOptValue::Str(s.to_string()),
            BishOptDefault::Color(s) => {
                let c = crate::csscolor::parse_terminal_list(s)
                    .unwrap_or_else(|e| panic!("KNOWN_BISHOPTS: {name}: default color {s:?} doesn't parse: {e}"));
                BishOptValue::Color(s.to_string(), c)
            }
        })
    }

    // Every `bishopt --set` call site's actual write -- diverted into
    // `pending_theme` (keyed by `name`, exactly like `self.bishopts`
    // itself) instead of applying live whenever a `::bish theme
    // begin`/`end` declaration is in progress, per that command's own
    // doc comment. `--unset` deliberately does NOT go through this (it
    // isn't "declaring" a value the way `--set` is -- see run_bish_
    // theme_end's own doc comment for the full reasoning), so it always
    // acts on live state even mid-declaration.
    // `::bish hl --set`'s own write, diverted into a theme declaration
    // exactly as `store_bishopt` diverts a bishopt -- which is what
    // makes `::bish theme begin` capture colours and options together,
    // as one thing to switch to.
    pub(crate) fn store_hl(&mut self, name: &str, value: String) {
        match &mut self.pending_theme {
            Some(pending) => {
                pending.hl.insert(name.to_string(), value);
            }
            None => {
                self.hl.insert(name.to_string(), value);
            }
        }
    }

    /// The colour set for `name`, following the active theme when
    /// nothing is set directly -- the same precedence a bishopt has.
    ///
    /// `None` means nothing has been said about this name, and the
    /// caller keeps its own default. There is no registry of defaults
    /// here on purpose: the names are open (see `Shell::hl`), so
    /// "unset" is the only thing that can be said about one nobody has
    /// mentioned.
    pub fn hl_color(&self, name: &str) -> Option<vt100::Color> {
        self.hl_color_for(name, detect_color_support())
    }

    // `hl_color`'s own logic with the terminal's support passed in --
    // split out for the reason `bishopt_color_for` is: so tests can
    // exercise every tier without mutating process-global env vars.
    fn hl_color_for(&self, name: &str, support: crate::csscolor::ColorSupport) -> Option<vt100::Color> {
        let text = match self.hl.get(name) {
            Some(text) => text.clone(),
            None => {
                let BishOptValue::Str(active) = self.bishopts.get("theme")? else { return None };
                self.themes.get(active)?.hl.get(name)?.clone()
            }
        };
        let candidates = crate::csscolor::parse_terminal_list(&text).ok()?;
        Some(match crate::csscolor::pick(&candidates, support) {
            crate::csscolor::TermColor::Rgba(rgba) => vt100::Color::Rgb(rgba.r, rgba.g, rgba.b),
            crate::csscolor::TermColor::Ansi(n) => vt100::Color::Indexed(n),
        })
    }

    /// Every highlight colour currently in force, theme included --
    /// what a redraw builds its overrides from, and what `::bish hl`
    /// with no arguments lists.
    pub fn hl_colors(&self) -> Vec<(String, String)> {
        let mut out: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if let Some(BishOptValue::Str(active)) = self.bishopts.get("theme")
            && let Some(theme) = self.themes.get(active)
        {
            out.extend(theme.hl.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        // Anything set directly wins over the theme, same as a bishopt.
        out.extend(self.hl.iter().map(|(k, v)| (k.clone(), v.clone())));
        let mut list: Vec<(String, String)> = out.into_iter().collect();
        list.sort();
        list
    }

    pub(crate) fn store_bishopt(&mut self, name: &str, value: BishOptValue) {
        match &mut self.pending_theme {
            Some(pending) => {
                pending.opts.insert(name.to_string(), value);
            }
            None => {
                self.bishopts.insert(name.to_string(), value);
            }
        }
    }

    // bishopt [--quiet|-q NAME | --set|-s NAME [VALUE] | --unset|-u NAME |
    // NAME]. `registry` is threaded in as a parameter (rather than
    // reaching for KNOWN_BISHOPTS directly) purely for testability -- the
    // one real caller (run_single's dispatch) always passes
    // KNOWN_BISHOPTS itself.
    //
    // Bare `bishopt` lists every registered option's *name* only, no
    // values -- unlike shopt's own table, a bishopt's value isn't always
    // printable text the same way for every type (see the get arm below),
    // so there's no single format that would fit every row.
    //
    // Get (`bishopt NAME`): a Bool option prints "on"/"off", a Str prints
    // its own value, and a Color prints back the exact text it was last
    // `--set` to (or its registered default's own text, if never set) --
    // not a re-serialization, so `--set accent cornflowerblue` reads back
    // as "cornflowerblue", not "#6495ed" (BishOptValue::Color keeps that
    // original text around for exactly this). All exit 0. `--quiet`/`-q`
    // (matching `shopt -q`'s own convention,
    // which is what prompted this) suppresses that printing; for a Bool
    // its value becomes the exit status instead (0 = on, 1 = off), so a
    // script can test it directly (`bishopt -q NAME && ...`) without
    // parsing text; Str/Color have no meaningful boolean to report, so
    // quiet just exits 0 once NAME is confirmed to exist.
    //
    // Set: `--set NAME` turns a Bool on; `--set NAME on`/`--set NAME off`
    // sets it explicitly either way (the same "on"/"off" vocabulary get
    // prints, so nothing new to learn) -- either spelling is optional
    // sugar over `--unset`, not a replacement for it. `--set NAME VALUE`
    // sets a Str's value (VALUE required, not restricted to on/off), or
    // a Color's -- VALUE is a CSS font-family-style comma-separated
    // fallback list (crate::csscolor::parse_terminal_list) of one or more
    // candidates, each any valid CSS Color (named, #hex, rgb()/hsl()/
    // hwb(), color-mix() for basic "color math" -- see that module's own
    // doc comment for what's deliberately out of scope) or a `-bish-*`
    // vendor reference into the terminal's own ANSI palette. Whichever
    // candidate is first in the list AND actually suits the terminal's
    // detected color support wins at render time (crate::csscolor::pick,
    // via Shell::bishopt_color) -- e.g. "#ff0000, -bish-ansi(1),
    // -bish-red" picks the truecolor hex on a modern terminal but still
    // degrades gracefully to plain ANSI red on an old one, all in one
    // value. An unparseable list (any single candidate failing) is a
    // usage error, same as any other type mismatch here. Unset always
    // removes the override and falls back to the option's registered
    // default -- for a Bool that default is definitionally `false`, so
    // "--unset turns it off" and "revert to default" are the same
    // operation; for a Str or Color it reverts to whatever default that
    // option was registered with.
    // One block per option: its name, what it accepts, what it is set to
    // now, and the line from BISHOPT_HELP. Shared by `bishopt
    // --describe` and the `:help options` page, so the two can't drift.
    /// The same, over the real registry -- for a caller outside this
    /// module, which has no name for `BishOptDefault` and no reason to.
    pub fn describe_options(&self, which: Option<&str>) -> Vec<String> {
        self.describe_bishopts(KNOWN_BISHOPTS, which)
    }

    pub(crate) fn describe_bishopts(&self, registry: &[(&str, BishOptDefault)], which: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        for (name, default) in registry.iter().filter(|(n, _)| which.is_none_or(|w| *n == w)) {
            let (accepts, default_text) = match default {
                BishOptDefault::Bool(d) => ("on | off".to_string(), if *d { "on".to_string() } else { "off".to_string() }),
                BishOptDefault::Int(d, range) => (format!("{}-{}", range.start(), range.end()), d.to_string()),
                BishOptDefault::Str(d) => ("text".to_string(), format!("{d:?}")),
                BishOptDefault::Color(d) => ("a CSS colour".to_string(), (*d).to_string()),
            };
            let description = BISHOPT_HELP.iter().find(|(n, _)| n == name).map(|(_, d)| *d).unwrap_or("");
            // The value as `bishopt NAME` itself would print it, so the
            // two never disagree about what is set.
            let current = self.bishopt_display(registry, name);
            out.push(name.to_string());
            out.push(format!("    {description}"));
            out.push(format!("    accepts: {accepts}    default: {default_text}    now: {current}"));
        }
        out
    }

    // What `bishopt NAME` prints, as a string rather than to the sink.
    fn bishopt_display(&self, registry: &[(&str, BishOptDefault)], name: &str) -> String {
        match self.bishopt_value(registry, name) {
            Some(BishOptValue::Bool(on)) => if on { "on" } else { "off" }.to_string(),
            Some(BishOptValue::Int(n)) => n.to_string(),
            Some(BishOptValue::Str(s)) => format!("{s:?}"),
            Some(BishOptValue::Color(text, _)) => text,
            None => String::new(),
        }
    }

    // `::bish hook ls|add|rm` -- what runs when the editor opens, writes
    // or closes a file. The whole point is that a config file can attach
    // behaviour to a language without the editor knowing anything about
    // that behaviour: `::bish hook add --lang=rust editor:file:open
    // __rust_setup` and the editor just runs it.
    // `--lang=GLOB` or `--lang GLOB`, returning it and whatever
    // follows. Its own helper because `add` and `ls` have to agree
    // about the spelling.
    pub(crate) fn hook_lang_flag<'a>(&mut self, subcommand: &str, args: &'a [String]) -> Result<(Option<String>, &'a [String]), i32> {
        match args.first().map(String::as_str) {
            Some(flag) if flag.starts_with("--lang=") => Ok((Some(flag["--lang=".len()..].to_string()), &args[1..])),
            Some("--lang") => match args.get(1) {
                Some(lang) => Ok((Some(lang.clone()), &args[2..])),
                None => {
                    sh_eprintln!(self, "bish: ::bish hook: {subcommand}: --lang needs a glob");
                    Err(2)
                }
            },
            _ => Ok((None, args)),
        }
    }

    // `--root-cmd=COMMAND`/`--root-cmd COMMAND`. Its own helper rather
    // than folding into `lsp_root_flag`, so `--root` and `--root-cmd`
    // can be given in either order and neither is positional.
    pub(crate) fn lsp_root_cmd_flag<'a>(&mut self, args: &'a [String]) -> Result<(String, &'a [String]), i32> {
        match args.first().map(String::as_str) {
            Some(flag) if flag.starts_with("--root-cmd=") => Ok((flag["--root-cmd=".len()..].to_string(), &args[1..])),
            Some("--root-cmd") => match args.get(1) {
                Some(command) if !command.trim().is_empty() => Ok((command.clone(), &args[2..])),
                _ => {
                    sh_eprintln!(self, "bish: ::bish lsp: add: --root-cmd needs a command");
                    Err(2)
                }
            },
            _ => Ok((String::new(), args)),
        }
    }

    // `--setting KEY=VALUE`/`--setting=KEY=VALUE`, repeatable.
    //
    // Returns at most one pair per call; the caller's round-robin loop
    // is what makes it repeatable, and what lets it appear anywhere
    // among the other flags.
    pub(crate) fn lsp_setting_flag<'a>(&mut self, args: &'a [String]) -> Result<ParsedSetting<'a>, i32> {
        let (raw, rest) = match args.first().map(String::as_str) {
            Some(flag) if flag.starts_with("--setting=") => (flag["--setting=".len()..].to_string(), &args[1..]),
            Some("--setting") => match args.get(1) {
                Some(v) => (v.clone(), &args[2..]),
                None => {
                    sh_eprintln!(self, "bish: ::bish lsp: add: --setting needs KEY=VALUE");
                    return Err(2);
                }
            },
            _ => return Ok((None, args)),
        };
        // Split on the *first* `=`: a value is free to contain more of
        // them, and a key is not.
        let Some(at) = raw.find('=') else {
            sh_eprintln!(self, "bish: ::bish lsp: add: --setting: expected KEY=VALUE, got '{raw}'");
            return Err(2);
        };
        let (key, value) = raw.split_at(at);
        let key = key.trim();
        if key.is_empty() {
            sh_eprintln!(self, "bish: ::bish lsp: add: --setting: empty key in '{raw}'");
            return Err(2);
        }
        Ok((Some((key.to_string(), value[1..].to_string())), rest))
    }

    // `--apply-edits=scoped|never|always`, defaulting to `scoped`.
    pub(crate) fn lsp_apply_edits_flag<'a>(&mut self, args: &'a [String]) -> Result<(String, &'a [String]), i32> {
        let (value, rest) = match args.first().map(String::as_str) {
            Some(flag) if flag.starts_with("--apply-edits=") => (flag["--apply-edits=".len()..].to_string(), &args[1..]),
            Some("--apply-edits") => match args.get(1) {
                Some(v) => (v.clone(), &args[2..]),
                None => {
                    sh_eprintln!(self, "bish: ::bish lsp: add: --apply-edits needs scoped, never or always");
                    return Err(2);
                }
            },
            _ => return Ok(("scoped".to_string(), args)),
        };
        if !matches!(value.as_str(), "scoped" | "never" | "always") {
            sh_eprintln!(self, "bish: ::bish lsp: add: --apply-edits: expected scoped, never or always, got '{value}'");
            return Err(2);
        }
        Ok((value, rest))
    }

    /// The declared server for a file of `language`, if any -- the first
    /// whose `--lang` glob matches, so an earlier, more specific
    /// registration wins over a later catch-all, the same
    /// first-match-wins rule `hooks_for` uses for ordering.
    pub fn lsp_server_for(&self, language: &str) -> Option<&LspServer> {
        if !self.bishopt_bool("lsp") {
            return None;
        }
        self.lsp_servers.iter().find(|s| crate::glob::matches(&s.lang, language))
    }

    // `--lang=GLOB`/`--lang GLOB`, exactly as `hook_lang_flag` does it.
    pub(crate) fn lsp_lang_flag<'a>(&mut self, subcommand: &str, args: &'a [String]) -> Result<(Option<String>, &'a [String]), i32> {
        match args.first().map(String::as_str) {
            Some(flag) if flag.starts_with("--lang=") => Ok((Some(flag["--lang=".len()..].to_string()), &args[1..])),
            Some("--lang") => match args.get(1) {
                Some(lang) => Ok((Some(lang.clone()), &args[2..])),
                None => {
                    sh_eprintln!(self, "bish: ::bish lsp: {subcommand}: --lang needs a glob");
                    Err(2)
                }
            },
            _ => Ok((None, args)),
        }
    }

    // `--root=NAME[,NAME...]`. Defaults to `.git`, which is already this
    // codebase's notion of where a project stops (gitignore::Stack::
    // for_directory walks up to exactly there).
    pub(crate) fn lsp_root_flag<'a>(&mut self, args: &'a [String]) -> Result<(Vec<String>, &'a [String]), i32> {
        let (raw, rest) = match args.first().map(String::as_str) {
            Some(flag) if flag.starts_with("--root=") => (flag["--root=".len()..].to_string(), &args[1..]),
            Some("--root") => match args.get(1) {
                Some(v) => (v.clone(), &args[2..]),
                None => {
                    sh_eprintln!(self, "bish: ::bish lsp: add: --root needs a name");
                    return Err(2);
                }
            },
            _ => return Ok((vec![".git".to_string()], args)),
        };
        let markers: Vec<String> = raw.split(',').map(str::trim).filter(|m| !m.is_empty()).map(str::to_string).collect();
        if markers.is_empty() {
            sh_eprintln!(self, "bish: ::bish lsp: add: --root needs at least one name");
            return Err(2);
        }
        Ok((markers, rest))
    }

    /// The commands to run for `event` on a file of `language`, in the
    /// order they were added. Empty is the overwhelmingly common answer,
    /// which is what keeps firing an event free when nobody is listening.
    pub fn hooks_for(&self, event: &str, language: &str) -> Vec<String> {
        if self.firing_hooks {
            return Vec::new();
        }
        self.hooks.iter().filter(|h| h.event == event && crate::glob::matches(&h.lang, language)).map(|h| h.command.clone()).collect()
    }

    /// Brackets a run of hooks, so anything they do can't fire more.
    pub fn set_firing_hooks(&mut self, firing: bool) {
        self.firing_hooks = firing;
    }

    // Resolves a bishopt Color option's current effective value as a
    // vt100::Color -- Rgb for an ordinary CSS color, Indexed for a
    // `-bish-*` vendor reference into the terminal's own palette (see
    // csscolor::TermColor). `None` if `name` isn't a registered Color
    // option at all (an unknown name, or a Bool/Str one). Used by repl.rs
    // to build a session's live bishedit::highlight::ColorOverrides each
    // redraw, from bishedit::highlight::HL_NAMES' own list of
    // names -- exposed this way (rather than BishOptValue/bishopt_value
    // themselves, both private) since repl.rs only ever needs the
    // resolved color, never the raw bishopt machinery. Detects the real
    // terminal's own color support fresh each call (see
    // detect_color_support) -- cheap (two env lookups), and this is only
    // ever called once per colour name per redraw, not per character.
    // A registered Bool/Int/Str option's current value. Panicking on an
    // unregistered name is deliberate: every caller names a constant
    // from KNOWN_BISHOPTS, so a miss is a typo in this codebase rather
    // than anything a user can cause.
    pub fn bishopt_bool(&self, name: &str) -> bool {
        match self.bishopt_value(KNOWN_BISHOPTS, name) {
            Some(BishOptValue::Bool(b)) => b,
            other => panic!("bishopt {name}: expected a boolean option, found {other:?}"),
        }
    }

    pub fn bishopt_int(&self, name: &str) -> i64 {
        match self.bishopt_value(KNOWN_BISHOPTS, name) {
            Some(BishOptValue::Int(n)) => n,
            other => panic!("bishopt {name}: expected a numeric option, found {other:?}"),
        }
    }

    pub fn bishopt_str(&self, name: &str) -> String {
        match self.bishopt_value(KNOWN_BISHOPTS, name) {
            Some(BishOptValue::Str(s)) => s,
            other => panic!("bishopt {name}: expected a string option, found {other:?}"),
        }
    }

    pub fn bishopt_color(&self, name: &str) -> Option<vt100::Color> {
        self.bishopt_color_for(name, detect_color_support())
    }

    // bishopt_color's own logic, with the terminal's color support
    // passed in rather than detected -- split out purely so tests can
    // exercise every support tier without mutating process-global env
    // vars (racy under Rust's default parallel test execution).
    fn bishopt_color_for(&self, name: &str, support: crate::csscolor::ColorSupport) -> Option<vt100::Color> {
        let BishOptValue::Color(_, candidates) = self.bishopt_value(KNOWN_BISHOPTS, name)? else {
            return None;
        };
        Some(match crate::csscolor::pick(&candidates, support) {
            crate::csscolor::TermColor::Rgba(rgba) => vt100::Color::Rgb(rgba.r, rgba.g, rgba.b),
            crate::csscolor::TermColor::Ansi(n) => vt100::Color::Indexed(n),
        })
    }

    // ${!prefix*}/${!prefix@} -- every currently-visible variable or
    // array name starting with `prefix`, sorted and deduped (matching
    // real bash's own sorted output). Same enumeration as
    // action_context's own `variables`/`arrays` fields (env vars +
    // every var_scopes frame, plus indexed/associative array names) --
    // built fresh here rather than routed through action_context since
    // this only needs the names, not the whole compgen::ActionContext
    // shape.
    fn var_names_with_prefix(&self, prefix: &str) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = self.globals.keys().cloned().collect();
        for scope in &self.var_scopes {
            names.extend(scope.keys().cloned());
        }
        names.extend(self.arrays.keys().cloned());
        names.extend(self.assoc_arrays.keys().cloned());
        // The ones computed on demand rather than stored (see
        // lookup_var's own tail): they are set as far as anything can
        // tell, so `${!BASH*}` has to find them. Listing only what is
        // in the tables found `BASH_VERSINFO`, which is an array and
        // therefore real, and nothing else.
        names.extend(COMPUTED_VAR_NAMES.iter().map(|n| n.to_string()));
        names.into_iter().filter(|n| n.starts_with(prefix)).collect()
    }

    // A snapshot of every bit of Shell state compgen.rs's contextual
    // actions (alias/arrayvar/command/enabled/export/function/job/running/
    // stopped/variable) need -- built fresh each call (compgen/complete)
    // rather than cached, same "cheap enough, not a hot path" reasoning as
    // functions_preamble. repl.rs builds the exact same shape once per
    // prompt (not per keystroke) for the interactive Tab-completion path,
    // via this same method -- see its own call site.
    pub(crate) fn action_context(&self) -> compgen::ActionContext {
        let mut arrays: Vec<String> = self.arrays.keys().cloned().collect();
        arrays.extend(self.assoc_arrays.keys().cloned());
        let mut variables: Vec<String> = self.globals.keys().cloned().collect();
        for scope in &self.var_scopes {
            variables.extend(scope.keys().cloned());
        }
        let jobs = self.jobs.borrow();
        compgen::ActionContext {
            aliases: self.aliases.iter().map(|(n, _)| n.clone()).collect(),
            functions: self.functions.keys().cloned().collect(),
            arrays,
            exported: self.exported_names.iter().cloned().collect(),
            variables,
            builtins: KNOWN_BUILTINS.iter().map(|s| s.to_string()).collect(),
            shopt_names: KNOWN_SHOPT_OPTIONS.iter().map(|(n, _)| n.to_string()).collect(),
            set_o_names: SET_O_OPTIONS.iter().map(|s| s.to_string()).collect(),
            signal_names: SIGNAL_NAMES.iter().map(|(n, _)| n.to_string()).collect(),
            jobs: jobs.jobs.iter().map(|j| j.cmd_text.clone()).collect(),
            running_jobs: jobs.jobs.iter().filter(|j| !j.stopped).map(|j| j.cmd_text.clone()).collect(),
            stopped_jobs: jobs.jobs.iter().filter(|j| j.stopped).map(|j| j.cmd_text.clone()).collect(),
            path_commands: highlight::enumerate_path_matches(""),
        }
    }

    // repl.rs's own owned-snapshot pattern (see its construction of
    // shell_completion) -- read-only access to the registered completion
    // specs bish's own interactive Tab completion consults, without
    // exposing `completions`/`default_completion` themselves (or letting a
    // caller mutate them directly, bypassing run_complete's own
    // registration/removal bookkeeping).
    pub(crate) fn completions_snapshot(&self) -> std::collections::HashMap<String, compgen::CompgenSpec> {
        self.completions.clone()
    }

    pub(crate) fn default_completion_snapshot(&self) -> Option<compgen::CompgenSpec> {
        self.default_completion.clone()
    }

    // Prints a compgen.rs ParseError the way real bash phrases it for
    // whichever builtin hit it (`compgen`/`complete` share this since the
    // message shape is identical either way, just with a different own
    // name in the "bish: NAME: ..." prefix), and picks the matching exit
    // code (2 for every parse error, matching real bash's own usage-error
    // convention).
    pub(crate) fn report_compgen_parse_error(&mut self, who: &str, err: &compgen::ParseError) -> i32 {
        match err {
            compgen::ParseError::UnknownAction(name) => sh_eprintln!(self, "bish: {who}: {name}: invalid action name"),
            compgen::ParseError::UnknownOption(c) => sh_eprintln!(self, "bish: {who}: -{c}: invalid option"),
            compgen::ParseError::UnknownOptName(name) => sh_eprintln!(self, "bish: {who}: {name}: invalid option name"),
            compgen::ParseError::MissingArg(flag) => sh_eprintln!(self, "bish: {who}: {flag}: option requires an argument"),
        }
        2
    }

    pub(crate) fn print_all_completions(&mut self) {
        let mut names: Vec<&String> = self.completions.keys().collect();
        names.sort();
        for name in names {
            sh_println!(self, "{}", compgen::format_spec(&self.completions[name], name));
        }
        if let Some(default) = &self.default_completion {
            sh_println!(self, "{}", compgen::format_spec(default, "-D"));
        }
    }

    pub(crate) fn print_completions(&mut self, names: &[String]) -> i32 {
        if names.is_empty() {
            self.print_all_completions();
            return 0;
        }
        let mut status = 0;
        for name in names {
            if name == "-D" {
                match &self.default_completion {
                    Some(spec) => sh_println!(self, "{}", compgen::format_spec(spec, "-D")),
                    None => {
                        sh_eprintln!(self, "bish: complete: -D: no completion specification");
                        status = 1;
                    }
                }
                continue;
            }
            match self.completions.get(name) {
                Some(spec) => sh_println!(self, "{}", compgen::format_spec(spec, name)),
                None => {
                    sh_eprintln!(self, "bish: complete: {name}: no completion specification");
                    status = 1;
                }
            }
        }
        status
    }

    pub(crate) fn remove_completions(&mut self, names: &[String]) -> i32 {
        if names.is_empty() {
            self.completions.clear();
            self.default_completion = None;
            return 0;
        }
        let mut status = 0;
        for name in names {
            if name == "-D" {
                if self.default_completion.take().is_none() {
                    sh_eprintln!(self, "bish: complete: -D: no completion specification");
                    status = 1;
                }
                continue;
            }
            if self.completions.remove(name).is_none() {
                sh_eprintln!(self, "bish: complete: {name}: no completion specification");
                status = 1;
            }
        }
        status
    }

    fn run_arith_print(&mut self, args: &[String]) -> i32 {
        // Joined with spaces rather than evaluated per argument: the
        // shell has already split `= 3 * (2 + 7)` into words, and it is
        // one expression.
        let expr = args.join(" ");
        if expr.trim().is_empty() {
            sh_eprintln!(self, "bish: =: usage: = EXPRESSION");
            return 2;
        }
        match self.eval_arith(&expr) {
            Ok(value) => {
                sh_println!(self, "{value}");
                0
            }
            Err(e) => {
                sh_eprintln!(self, "bish: =: {e}");
                1
            }
        }
    }

    // Switches into the terminal's alternate screen buffer (mode 1049) the
    // first time any window-family command runs, so whatever was on
    // screen before promotion stays untouched in the real terminal's own
    // native scrollback. Only the screen-buffer switch happens here --
    // drawing the actual tab bar needs the real window list/cwd-per-
    // session, which lives in repl.rs (the only thing that can hold it
    // without an Rc<RefCell<_>> self-reference cycle, see
    // ExecResult::Window's doc comment), so repl.rs draws it right after
    // this, and again after every WindowAction -- which is also what
    // knows the terminal's real size (repl.rs queries TIOCGWINSZ and
    // pins the tab bar to the actual last row); nothing here needs to.
    // pub(crate): also called directly by repl.rs's bishedit normal-mode
    // entry (Ctrl+Space), which bypasses run_window/the command-dispatch
    // path entirely -- see repl.rs's ensure_promoted.
    pub(crate) fn promote_if_needed(&mut self) {
        if self.promoted.get() {
            return;
        }
        self.promoted.set(true);
        sh_print!(self, "\x1b[?1049h\x1b[2J\x1b[H");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }

    // POSIX has no query-only umask read -- `umask(new) -> previous` is the
    // only syscall shape, so reading the current value means setting a
    // throwaway mask and immediately restoring what was there. Moved here
    // alongside run_ulimit for the same M6 sink reason.
    /// Rebuilds `FUNCNAME`, `BASH_SOURCE` and `BASH_LINENO` from the
    /// call stack.
    ///
    /// Kept as real arrays, refreshed on every push and pop, rather than
    /// computed on lookup: `${FUNCNAME[@]}`, `${FUNCNAME[0]}` and
    /// `${#FUNCNAME[@]}` then all work with no changes to the variable
    /// system at all, and a call costs three small vectors.
    ///
    /// Index 0 is innermost, and each array is one longer than the stack
    /// -- the extra entry is the script itself, which bash names `main`,
    /// sources to the top-level file and gives line 0. Verified against
    /// bash: inside `inner`, called by `outer` from a sourced library,
    ///
    ///   FUNCNAME=(inner outer main)
    ///   BASH_SOURCE=(./lib.sh ./lib.sh main.sh)
    ///   BASH_LINENO=(7 2 0)
    ///
    /// so `BASH_SOURCE[i]` is where `FUNCNAME[i]` was *defined* and
    /// `BASH_LINENO[i]` is the line in `BASH_SOURCE[i+1]` where it was
    /// called. Those are two different files as soon as anything is
    /// sourced, which is why the defining file is recorded separately.
    fn refresh_call_arrays(&mut self) {
        let (mut names, mut sources, mut lines) = (Vec::new(), Vec::new(), Vec::new());
        for frame in self.call_stack.iter().rev() {
            names.push(frame.called.clone());
            sources.push(self.function_sources.get(&frame.called).cloned().unwrap_or_else(|| frame.source.clone()));
            lines.push(frame.call_line.to_string());
        }
        // The outermost frame is the script itself, which bash names
        // `main`. It only exists when there *is* a script: `bash -c
        // 'f(){ g; }; g(){ echo "${FUNCNAME[*]}"; }; f'` prints `g f`,
        // where the same functions in a file print `g f main`. Checked
        // against bash 5.3 both ways.
        if self.running_a_script {
            names.push("main".to_string());
            lines.push("0".to_string());
        }
        sources.push(self.script_name.clone());
        // At the top level bash has no FUNCNAME at all, and a script
        // testing `${FUNCNAME[0]}` should see nothing rather than
        // "main".
        if self.call_stack.is_empty() {
            self.arrays.remove("FUNCNAME");
            self.arrays.remove("BASH_LINENO");
        } else {
            self.set_array("FUNCNAME", names);
            self.set_array("BASH_LINENO", lines);
        }
        self.set_array("BASH_SOURCE", sources);
    }

    fn set_array(&mut self, name: &str, values: Vec<String>) {
        self.arrays.insert(name.to_string(), values.into_iter().enumerate().collect());
    }

    /// Whether `name` is a builtin that would actually run.
    ///
    /// `enable -n NAME` takes one out of service, and everything that
    /// asks "is this a builtin" has to agree with the dispatch that
    /// decides -- otherwise `type echo` still calls it a builtin while
    /// `/usr/bin/echo` is what runs, and a pipeline stage still self-
    /// execs bish to run a builtin that is not going to run. Both were
    /// true before this; bash reports the external in both cases.
    ///
    /// Deliberately a method where `is_known_builtin` is a free
    /// function: the question now depends on this shell's own state,
    /// which a free function cannot see.
    pub(crate) fn is_active_builtin(&self, name: &str) -> bool {
        is_known_builtin(name) && !self.disabled_builtins.contains(name)
    }

    // Moved here from a builtins.rs free function once cd needed to update
    // self.cwd (a Shell-owned field) rather than just the OS process's
    // cwd -- matches every other builtin that needs shell state (see
    // run_pushd/run_declare/etc, all Shell methods for the same reason).
    // Still delegates the actual directory change to
    // std::env::set_current_dir so path resolution/symlink/error behavior
    // is unchanged from before; self.cwd is read back from the OS
    // afterward rather than computed locally, so it stays exactly in sync
    // with what the OS considers the real cwd.
    // repl.rs's Alt+Left/Right/Up directory-history navigation calls
    // this directly (bypassing the `cd` builtin's own argv/`-` parsing,
    // since it always has a concrete absolute path already in hand) to
    // reuse run_cd's actual directory-change logic -- path resolution,
    // OLDPWD/PWD updates, error reporting -- rather than duplicating it.
    pub fn cd_to(&mut self, path: &std::path::Path) -> i32 {
        crate::builtins::dirs::run_cd(self, &[path.to_string_lossy().into_owned()])
    }

    /// `target` resolved against `CDPATH`, and whether that is where it
    /// came from.
    ///
    /// Left alone -- and `false` -- for anything `CDPATH` does not apply
    /// to: an absolute path, one starting `.` or `..`, an unset or empty
    /// `CDPATH`, or a name no component holds.
    ///
    /// An *empty* component means the current directory, per the same
    /// convention `PATH` uses, and a hit there is the only one not
    /// announced -- bash's rule is that a **non-empty** component
    /// announces, so a literal `.` in `CDPATH` does print, even though
    /// it lands exactly where a plain `cd` would have.
    pub(crate) fn resolve_cdpath(&mut self, target: String) -> (String, bool) {
        let path = std::path::Path::new(&target);
        if path.is_absolute() || target.starts_with('.') || target.is_empty() {
            return (target, false);
        }
        let cdpath = self.lookup_var("CDPATH");
        if cdpath.is_empty() {
            return (target, false);
        }
        for component in cdpath.split(':') {
            // Against *this shell's* own cwd, not the process's. They
            // are the same for whichever session last ran something, and
            // genuinely different for any other window -- and `cd` is
            // defined relative to the shell doing it.
            let base = match std::path::Path::new(component) {
                _ if component.is_empty() => self.cwd.clone(),
                p if p.is_absolute() => p.to_path_buf(),
                p => self.cwd.join(p),
            };
            let candidate = base.join(&target);
            if candidate.is_dir() {
                return (candidate.to_string_lossy().into_owned(), !component.is_empty());
            }
        }
        (target, false)
    }

    // Moving this shell to another directory -- the single write path,
    // so everything that can do it keeps `cwd`, `OLDPWD` and `PWD` in
    // step and nothing can route around restricted mode. `cd` is one
    // caller; the file browser's own "make this the shell's directory"
    // (repl.rs's expand_browse_targets) is the other, and adding it was
    // what made a shared path worth having.
    pub fn change_directory(&mut self, target: &std::path::Path) -> Result<(), String> {
        if self.opt_restricted {
            return Err(RESTRICTED.to_string());
        }
        let old = self.cwd.to_string_lossy().into_owned();
        // The *logical* path, not what `getcwd` reports. A shell
        // remembers the route you took: `cd link` leaves `$PWD` ending
        // in `link` and a following `cd ..` goes back to where `link`
        // sits, not to the parent of whatever it points at. Taking
        // `current_dir()` here resolved every symlink instead, so the
        // route was lost the moment it was walked -- `pwd` answered
        // what only `pwd -P` should.
        //
        // Falling back to the resolved path if the lexical one cannot
        // be entered: `..` is removed textually, which is the whole
        // point, and on a path that has been rearranged underneath the
        // shell that can name somewhere that no longer exists.
        let logical = logical_path(&self.cwd, target);
        match std::env::set_current_dir(&logical) {
            Ok(()) => self.cwd = logical,
            Err(_) => {
                std::env::set_current_dir(target).map_err(|e| os_message(&e))?;
                self.cwd = std::env::current_dir().unwrap_or_else(|_| target.to_path_buf());
            }
        }
        let new = self.cwd.to_string_lossy().into_owned();
        // Not written to the real environment: `PWD` and `OLDPWD` are
        // exported shell variables, and a child gets them the way it
        // gets every other one.
        // ...and into the variable table, which is what `$PWD` actually
        // reads now. Both are exported in bash, so they stay in the
        // real environment too, above.
        self.globals.insert("OLDPWD".to_string(), old.clone());
        self.globals.insert("PWD".to_string(), new.clone());
        self.exported_names.insert("OLDPWD".to_string());
        self.exported_names.insert("PWD".to_string());
        {}
        // ...and into this session's own remembered environment, not
        // just the real one. `sync_real_state_in` reapplies that
        // snapshot before every command, so a raw `set_var` made outside
        // a command -- which is exactly what the file browser's Ctrl-Y
        // does -- survives until the next command and is then silently
        // reverted. (`cd` itself never saw this: it runs inside a
        // command, and `sync_real_state_out` captures the real
        // environment right afterwards.) Same trap, and the same fix, as
        // `set_terminal_capability_env` below.
        let snap = Rc::make_mut(&mut self.env_snapshot);
        snap.insert("OLDPWD".to_string(), old);
        snap.insert("PWD".to_string(), new);
        Ok(())
    }

    fn collapse_home(path: &str) -> String {
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                if path == home {
                    return "~".to_string();
                }
                if let Some(rest) = path.strip_prefix(&format!("{}/", home)) {
                    return format!("~/{}", rest);
                }
            }
        }
        path.to_string()
    }

    pub(crate) fn print_dirs(&self, vertical: bool) {
        let mut all = vec![self.cwd.to_string_lossy().into_owned()];
        all.extend(self.dir_stack.iter().cloned());
        if vertical {
            for (i, d) in all.iter().enumerate() {
                sh_println!(self, "{:2}  {}", i, Self::collapse_home(d));
            }
        } else {
            sh_println!(self, "{}", all.iter().map(|d| Self::collapse_home(d)).collect::<Vec<_>>().join(" "));
        }
    }

    pub(crate) fn apply_shell_flag(&mut self, c: char, on: bool) {
        match c {
            'e' => self.opt_errexit = on,
            'T' => self.opt_functrace = on,
            'E' => self.opt_errtrace = on,
            'u' => self.opt_nounset = on,
            'x' => self.opt_xtrace = on,
            'f' => self.opt_noglob = on,
            'm' => self.opt_monitor = on,
            'C' => self.opt_noclobber = on,
            // One-way latch: `set +r` is simply ignored rather than
            // turning it back off (see opt_restricted's own doc
            // comment). Real bash instead makes `set +r` itself a hard
            // "invalid option" error -- not replicated here, since no
            // other flag in this shell errors on an unrecognized/
            // disallowed combination either; the net behavioral
            // guarantee (restricted mode can't be turned off) is the
            // part that actually matters.
            'r' if on => self.opt_restricted = true,
            _ => {}
        }
    }

    // The read side of apply_shell_option, for `set -o`/`set +o`'s own
    // listing. `None` for a name this shell doesn't gate anything on --
    // see SET_O_OPTIONS' doc comment for why that list is shorter than
    // real bash's.
    pub(crate) fn shell_option_enabled(&self, name: &str) -> Option<bool> {
        Some(match name {
            "pipefail" => self.opt_pipefail,
            "errexit" => self.opt_errexit,
            "functrace" => self.opt_functrace,
            "errtrace" => self.opt_errtrace,
            "nounset" => self.opt_nounset,
            "xtrace" => self.opt_xtrace,
            "noglob" => self.opt_noglob,
            "monitor" => self.opt_monitor,
            "noclobber" => self.opt_noclobber,
            "posix" => self.opt_posix,
            _ => return None,
        })
    }

    pub(crate) fn apply_shell_option(&mut self, name: &str, on: bool) {
        match name {
            "pipefail" => self.opt_pipefail = on,
            "errexit" => self.opt_errexit = on,
            "functrace" => self.opt_functrace = on,
            "errtrace" => self.opt_errtrace = on,
            "nounset" => self.opt_nounset = on,
            "xtrace" => self.opt_xtrace = on,
            "noglob" => self.opt_noglob = on,
            "monitor" => self.opt_monitor = on,
            "noclobber" => self.opt_noclobber = on,
            "posix" => self.opt_posix = on,
            _ => {}
        }
    }

    pub fn set_script_args(&mut self, name: String, args: Vec<String>) {
        self.script_name = name;
        self.arg_frames = vec![args];
    }

    // Runs any trapped signal's handler code for signals that arrived
    // since the last check (see the comment on PENDING_SIGNALS for why
    // this poll-at-checkpoints approach exists instead of running trap
    // code directly from the signal handler). Called once per top-level
    // statement in run_program -- frequent enough to feel responsive for
    // real scripts, without needing signal-safety anywhere outside the
    // one-line handler itself.
    fn check_pending_signals(&mut self) {
        let pending = PENDING_SIGNALS.swap(0, std::sync::atomic::Ordering::SeqCst);
        if pending == 0 {
            return;
        }
        for sig in 0..32 {
            if pending & (1 << sig) == 0 {
                continue;
            }
            if let Some(TrapAction::Run(code)) = self.traps.get(&sig).cloned() {
                self.run_source_here(&code, "trap");
            }
        }
    }

    pub fn run_program(&mut self, prog: &Program) -> ExecResult {
        let mut result = ExecResult::Status(self.last_status);
        for item in prog {
            self.current_line = item.line;
            self.check_pending_signals();
            // Nothing is reading this stage's output any more. A stage
            // that was its own process would already be dead of
            // SIGPIPE; this is where the in-process one stops instead,
            // with the status that signal would have given it.
            if broken_pipe_seen() {
                return ExecResult::Exit(128 + 13);
            }
            // Clone the Rc (not borrow self.debug_hook directly) before
            // calling into it: on_statement takes `&Shell`, and the hook
            // itself may call back into read-only Shell methods (variable
            // inspection for the debugger's own hover/print) -- holding a
            // live borrow of `self.debug_hook`'s own RefCell across that
            // call would panic the moment it tried.
            if let Some(hook) = self.debug_hook.clone() {
                let depth = DebugDepth { subshell_depth: self.subshell_depth, call_depth: self.var_scopes.len() };
                match hook.borrow_mut().on_statement(item.line, depth, self) {
                    DebugAction::Quit => {
                        self.run_exit_trap();
                        return ExecResult::Exit(self.last_status);
                    }
                    DebugAction::Continue | DebugAction::StepOver | DebugAction::StepInto => {}
                }
            }
            let background = matches!(item.sep, Sep::Background);
            result = self.run_and_or(&item.and_or, background);
            self.last_status = result.status();
            // Backstop for a `set -u` violation from an expansion site
            // run_single's own check doesn't cover (a `for`/`case` list, an
            // arithmetic expansion, ...) -- see pending_exit's own doc
            // comment. A no-op the vast majority of the time: run_single's
            // check already turns the common case into `result` being
            // ExecResult::Exit directly, which is_signal() below catches,
            // leaving nothing here left to take.
            if let Some(exit) = self.take_pending_exit() {
                return exit;
            }
            // Cloned rather than moved at each use: `ExecResult` stopped
            // being `Copy` when `WindowAction` grew a name to carry.
            if result.is_signal() {
                return result;
            }
            // `set -e`: abort on any failing top-level statement, except
            // while suppressed (if/while/until conditions, negated
            // pipelines -- POSIX exempts those explicitly). Checking once
            // per ListItem here (the *overall* and-or result) rather than
            // per-pipeline also naturally exempts non-last commands in a
            // &&/|| chain, since only the chain's final status reaches here.
            // ERR fires under exactly `errexit`'s own rules -- not in a
            // condition, not behind `!`, not for a non-final command of
            // a `&&`/`||` chain -- because that is what bash means by
            // it, and this is already the one place those rules are
            // decided. It fires whether or not `errexit` is *on*, which
            // is the point: a script traps ERR precisely so it does not
            // have to exit.
            let exempt = std::mem::take(&mut self.errexit_exempt);
            if self.suppress_errexit == 0 && !exempt && result.status() != 0 {
                self.run_pseudo_trap(PseudoTrap::Err);
            }
            if self.opt_errexit && self.suppress_errexit == 0 && !exempt && result.status() != 0 {
                self.run_exit_trap();
                return ExecResult::Exit(result.status());
            }
        }
        result
    }

    fn run_and_or(&mut self, and_or: &AndOr, background: bool) -> ExecResult {
        // Every member but the last is exempt from `set -e`, and the
        // exemption reaches *inside* it: `set -e; f() { false; }; f &&
        // echo ok` carries on in bash, because `f`'s own failing body
        // is part of a chain whose result is being tested. Checking
        // only the chain's overall status at the ListItem level was not
        // enough -- the function body has ListItems of its own, and one
        // of those was aborting the shell.
        let last = and_or.rest.len();
        if last > 0 {
            self.suppress_errexit += 1;
        }
        let mut result = self.run_pipeline(&and_or.first, background);
        if last > 0 {
            self.suppress_errexit -= 1;
        }
        // Set *after* the member runs, not before: a function body
        // contains and_or lists of its own, and each one writes this
        // field on its way out.
        //
        // A single pipeline is its own final command; in a chain, only
        // the member after the last `&&`/`||` is.
        self.errexit_exempt = last > 0;
        self.last_status = result.status();
        if result.is_signal() {
            return result;
        }
        let mut status = result.status();
        for (i, (comb, pipeline)) in and_or.rest.iter().enumerate() {
            let should_run = match comb {
                Combinator::And => status == 0,
                Combinator::Or => status != 0,
            };
            if should_run {
                let is_last = i + 1 == last;
                if !is_last {
                    self.suppress_errexit += 1;
                }
                result = self.run_pipeline(pipeline, background);
                if !is_last {
                    self.suppress_errexit -= 1;
                }
                self.errexit_exempt = !is_last;
                self.last_status = result.status();
                if result.is_signal() {
                    return result;
                }
                status = result.status();
            }
        }
        ExecResult::Status(status)
    }

    fn run_pipeline(&mut self, pipeline: &Pipeline, background: bool) -> ExecResult {
        if let Some(style) = pipeline.timed {
            let started = std::time::Instant::now();
            let before = child_cpu_times();
            let result = self.run_pipeline_untimed(pipeline, background);
            let after = child_cpu_times();
            let report = self.format_times(style, started.elapsed().as_secs_f64(), after.0 - before.0, after.1 - before.1);
            sh_eprint!(self, "{report}");
            return result;
        }
        self.run_pipeline_untimed(pipeline, background)
    }

    // `time`'s own report. bash's default is a blank line and then
    // three `0m0.000s` rows; `-p` is POSIX's three bare seconds.
    // `TIMEFORMAT` replaces the first of those -- the subset of its
    // language anyone writes: `%R`/`%U`/`%S` for the three numbers,
    // `%P` for the percentage, an `l` for the `0m0.000s` spelling, a
    // digit for the precision, and `%%` for a literal one.
    fn format_times(&mut self, style: TimeStyle, real: f64, user: f64, sys: f64) -> String {
        let format = match style {
            TimeStyle::Posix => "real %2R\nuser %2U\nsys %2S".to_string(),
            TimeStyle::Shell => match self.var_is_set("TIMEFORMAT") {
                true => self.lookup_var("TIMEFORMAT"),
                false => "\nreal\t%3lR\nuser\t%3lU\nsys\t%3lS".to_string(),
            },
        };
        let mut out = String::new();
        let mut chars = format.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let mut precision = 3;
            if let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                precision = d.min(3) as usize;
                chars.next();
            }
            let long = chars.peek() == Some(&'l');
            if long {
                chars.next();
            }
            let value = match chars.next() {
                Some('R') => real,
                Some('U') => user,
                Some('S') => sys,
                Some('P') => {
                    let percent = match real > 0.0 {
                        true => (user + sys) / real * 100.0,
                        false => 0.0,
                    };
                    out.push_str(&format!("{percent:.precision$}"));
                    continue;
                }
                Some('%') => {
                    out.push('%');
                    continue;
                }
                other => {
                    out.push('%');
                    out.extend(other);
                    continue;
                }
            };
            match long {
                // `0m0.000s`: whole minutes, then the rest of the
                // seconds. No padding on the integer part -- 65 seconds
                // is `1m5.000s`.
                true => out.push_str(&format!("{}m{:.precision$}s", (value as i64) / 60, value % 60.0)),
                false => out.push_str(&format!("{value:.precision$}")),
            }
        }
        out.push('\n');
        out
    }

    fn run_pipeline_untimed(&mut self, pipeline: &Pipeline, background: bool) -> ExecResult {
        if pipeline.negate {
            // POSIX exempts a `!`-negated pipeline's own failure from -e
            // (that's usually the whole point of negating it).
            self.suppress_errexit += 1;
            let result = self.run_pipeline_inner(pipeline, background);
            self.suppress_errexit -= 1;
            return match result {
                ExecResult::Status(s) => ExecResult::Status(if s == 0 { 1 } else { 0 }),
                signal => signal,
            };
        }
        self.run_pipeline_inner(pipeline, background)
    }

    fn run_pipeline_inner(&mut self, pipeline: &Pipeline, background: bool) -> ExecResult {
        if pipeline.commands.len() == 1 {
            let result = crate::builtins::shell::run_command(self, &pipeline.commands[0], background);
            // A one-command pipeline is still a pipeline as far as
            // `PIPESTATUS` is concerned: bash gives it a one-element
            // array, and a script that reads `${PIPESTATUS[0]}` after a
            // plain command expects to find `$?` there.
            if let ExecResult::Status(code) = result {
                self.set_pipestatus(&[code]);
            }
            return result;
        }
        ExecResult::Status(self.run_multi(&pipeline.commands, background))
    }

    /// Publishes `PIPESTATUS`, every stage's own exit status in order.
    ///
    /// The array bash gives you for `cmd | tee log` so you can still
    /// find out whether `cmd` failed -- which is the entire reason it
    /// exists, and the reason `$?` alone is not enough.
    fn set_pipestatus(&mut self, codes: &[i32]) {
        let map: std::collections::BTreeMap<usize, String> = codes.iter().enumerate().map(|(i, c)| (i, c.to_string())).collect();
        self.arrays.insert("PIPESTATUS".to_string(), map);
    }

    // The part of run_command that actually dispatches on `cmd`'s own
    // variant, *after* its own redirects (if any) have already been
    // handled -- split out so run_in_child_shell's ChildSource::Parsed
    // case (run_compound_redirected's own in-process conversion) can run
    // `cmd`'s content directly without re-checking command_own_redirects,
    // which would just see the same still-attached redirects and call
    // run_compound_redirected right back into itself, forever.
    pub(crate) fn run_command_body(&mut self, cmd: &parser::Command, background: bool) -> ExecResult {
        match cmd {
            parser::Command::Simple(sc) => self.run_single(sc, background),
            parser::Command::If { branches, else_branch, .. } => self.run_if(branches, else_branch),
            parser::Command::While { cond, body, until, .. } => self.run_while(cond, body, *until),
            parser::Command::For { var, words, body, .. } => {
                let var = var.clone();
                let words = words.clone();
                self.run_for(&var, words.as_deref(), body)
            }
            parser::Command::CFor { init, cond, step, body, .. } => {
                let init = init.clone();
                let cond = cond.clone();
                let step = step.clone();
                self.run_cfor(&init, &cond, &step, body)
            }
            parser::Command::Select { var, words, body, .. } => {
                let var = var.clone();
                let words = words.clone();
                self.run_select(&var, words.as_deref(), body)
            }
            parser::Command::Case { word, arms, .. } => self.run_case(word, arms),
            parser::Command::Group(prog, _redirects) => self.run_program(prog),
            parser::Command::FuncDef { name, body } => {
                self.function_sources.insert(name.clone(), self.script_name.clone());
                self.functions.insert(name.clone(), (**body).clone());
                ExecResult::Status(0)
            }
            parser::Command::Subshell(raw, _redirects) => ExecResult::Status(self.run_subshell(raw, background)),
            parser::Command::Arith(raw, _redirects) => match self.eval_arith(raw) {
                Ok(v) => ExecResult::Status(if v != 0 { 0 } else { 1 }),
                Err(e) => {
                    sh_eprintln!(self, "bish: (({})): {}", raw, e);
                    ExecResult::Status(1)
                }
            },
            parser::Command::Test(atoms, _redirects) => crate::builtins::io::run_test(self, atoms),
            parser::Command::Coproc { name, body } => {
                let name = name.clone();
                ExecResult::Status(self.run_coproc(name, body))
            }
        }
    }

    // `read` is a builtin, so it can't go through the normal Stdio-based
    // redirect machinery (that's built for handing stdio to a *child*
    // process, not reading in-process). Special-cased here since `read x
    // <<< "..."` is too common a pattern to leave broken.
    fn read_input_source(&mut self, cmd: &SimpleCommand) -> Box<dyn std::io::BufRead> {
        for r in cmd.redirects.iter().rev() {
            match r {
                Redirect::HereString(w) => {
                    let mut content = self.expand_word(w);
                    content.push('\n');
                    return Box::new(std::io::Cursor::new(content.into_bytes()));
                }
                Redirect::HereDoc(w) => {
                    let content = self.expand_word(w);
                    return Box::new(std::io::Cursor::new(content.into_bytes()));
                }
                Redirect::In(w) => {
                    let p = self.expand_word(w);
                    return match self.open_in(&p) {
                        Ok(f) => Box::new(std::io::BufReader::new(PumpingFile { file: f, coroutines: Rc::clone(&self.background_coroutines) })),
                        Err(e) => {
                            sh_eprintln!(self, "bish: {}", e);
                            Box::new(std::io::Cursor::new(Vec::new()))
                        }
                    };
                }
                // `read <&3` -- fd 3 is one this shell opened with
                // `exec 3<file`, and it is a real process fd, so the
                // builtin can read it directly. Without this the
                // redirect was ignored and `read` took the terminal.
                Redirect::FdDup { fd: 0, target } => {
                    return Box::new(UnbufferedFd::new(*target as i32));
                }
                Redirect::FdDupWord { fd: 0, word } => {
                    let target = self.expand_word(word);
                    return match target.trim().parse::<i32>() {
                        Ok(n) => Box::new(UnbufferedFd::new(n)),
                        Err(_) => {
                            sh_eprintln!(self, "bish: {}: ambiguous redirect", target);
                            Box::new(std::io::Cursor::new(Vec::new()))
                        }
                    };
                }
                _ => continue,
            }
        }
        self.current_stdin_reader()
    }

    // Where a builtin reads when the command itself carries no `<` of
    // its own: the enclosing construct's stdin override if there is one
    // (see StdioOverride's doc comment) -- a `while read` loop's
    // `< file` sits on the *loop*, not on the `read`, so the read's own
    // redirect list is empty even though stdin is not the terminal --
    // and otherwise the real fd 0.
    //
    // `select` reads through here too. It used to go straight to
    // `std::io::stdin()`, so `select ... done <<< "1"` never saw the
    // here-string: it read the real stdin, hit end-of-file, and gave up
    // without ever running the body.
    fn current_stdin_reader(&self) -> Box<dyn std::io::BufRead> {
        if let Some(o) = &self.stdio_override {
            if let Some(state) = &o.borrow().stdin {
                return Box::new(SharedStdinReader { state: state.clone(), local: Vec::new() });
            }
        }
        // One byte per syscall, deliberately -- see UnbufferedFd.
        Box::new(UnbufferedFd::new(0))
    }

    // Shared by `eval`, `source`/`.`, and main.rs's own config-file loading
    // at interactive startup -- all three run source text in the CURRENT
    // shell (unlike command substitution/subshells, which self-exec a
    // child process), so `eval`/sourced scripts can set variables,
    // functions, or cwd in the calling shell. pub(crate) rather than
    // private: main.rs lives in a sibling module and needs this exact
    // "read then run in place" semantics for $HOME/.config/bish/
    // config.bash, not a subprocess.
    pub(crate) fn run_source_here(&mut self, src: &str, label: &str) -> ExecResult {
        // A file that sources itself is the same crash as a function
        // that calls itself, reached without ever making a function
        // call -- so it needs its own check rather than
        // `call_function`'s. No `nesting_unwind` here: each level's
        // `source` returns as soon as the one below it fails, so this
        // unwinds on its own. Real bash dumps core on this one.
        if crate::stackguard::nearly_exhausted() {
            sh_eprintln!(self, "bish: {}: maximum source nesting level exceeded", label);
            if !self.interactive {
                self.pending_exit = Some(1);
            }
            return ExecResult::Status(1);
        }
        match crate::lexer::Lexer::new(src).tokenize() {
            Ok(toks) => match crate::parser::Parser::new(self.expand_aliases(toks)).parse_program() {
                Ok(prog) => self.run_program(&prog),
                Err(e) => {
                    sh_eprintln!(self, "bish: {}: syntax error: {}", label, e);
                    ExecResult::Status(2)
                }
            },
            Err(e) => {
                sh_eprintln!(self, "bish: {}: syntax error: {}", label, e);
                ExecResult::Status(2)
            }
        }
    }

    // Functions and `local` variables live only in this process's memory, so
    // a self-exec'd child (see run_subshell/run_command_substitution below)
    // starts with no knowledge of them -- unlike a real fork, which
    // duplicates the whole process (this is exactly why e.g. `$(fib
    // $((n-1)))` works in bash even when `n` is a local var: the forked
    // child inherits it for free). Re-declaring visible locals and every
    // currently-known function at the top of the child's script closes that
    // gap without needing unsafe fork(2) or a separate IPC channel.
    pub(crate) fn functions_preamble(&self) -> String {
        let mut s = String::new();
        let mut flattened: HashMap<&str, &str> = HashMap::new();
        // Unexported globals. An exported one is in the child's real
        // environment already and needs no replay; an unexported one
        // used to travel that way too, before variables stopped living
        // in the environment (see the `globals` field) -- without this
        // a pipeline stage started with none of them.
        for (k, v) in &self.globals {
            if !self.exported_names.contains(k) {
                flattened.insert(k.as_str(), v.as_str());
            }
        }
        // Locals over globals: the stage is a child of *this* frame.
        for scope in &self.var_scopes {
            for (k, v) in scope {
                // A declared-but-unset local has no assignment to
                // replay; emitting `k=` would make it *set* to empty in
                // the child, which is the distinction being kept.
                match v {
                    Some(v) => flattened.insert(k.as_str(), v.as_str()),
                    None => flattened.remove(k.as_str()),
                };
            }
        }
        for (k, v) in &flattened {
            s.push_str(k);
            s.push('=');
            s.push_str(&crate::serialize::quote_literal(v));
            s.push('\n');
        }
        for (name, items) in &self.arrays {
            // The child seeds this one itself, from the same constant,
            // and it is readonly there -- so re-declaring it is both
            // redundant and refused, which turned every re-exec'd
            // construct into a "readonly variable" error.
            if name == "BASH_VERSINFO" {
                continue;
            }
            s.push_str(name);
            s.push_str("=(");
            for item in items.values() {
                s.push_str(&crate::serialize::quote_literal(item));
                s.push(' ');
            }
            s.push_str(")\n");
        }
        for (name, map) in &self.assoc_arrays {
            s.push_str("declare -A ");
            s.push_str(name);
            s.push('\n');
            for (k, v) in map.iter() {
                s.push_str(name);
                s.push('[');
                s.push_str(&crate::serialize::quote_literal(k));
                s.push_str("]=");
                s.push_str(&crate::serialize::quote_literal(v));
                s.push('\n');
            }
        }
        if let Some(frame) = self.arg_frames.last() {
            s.push_str("set --");
            for a in frame {
                s.push(' ');
                s.push_str(&crate::serialize::quote_literal(a));
            }
            s.push('\n');
        }
        for name in &self.readonly_names {
            s.push_str("readonly ");
            s.push_str(name);
            s.push('\n');
        }
        for (name, body) in &self.functions {
            let def = parser::Command::FuncDef { name: name.clone(), body: Box::new(body.clone()) };
            s.push_str(&crate::serialize::serialize_program(&[ListItem {
                and_or: AndOr { first: Pipeline { commands: vec![def], negate: false, timed: None }, rest: Vec::new() },
                sep: Sep::Seq,
                line: 0,
            }]));
        }
        // The directory stack, traps and completion specifications --
        // the three things a re-exec'd construct could not see.
        //
        // This is the list the roadmap called one that "grows and is
        // never finished", and that was the right objection to it for
        // *pipeline stages*, which are hot and now do not re-exec at
        // all. What is left re-execing -- a co-process, a backgrounded
        // subshell -- starts once and lives, so the list is three items
        // and the cost of replaying them is paid a single time. The
        // objection was to the price, and the price is different here.
        //
        // Deliberately not `jobs`: bash's own co-process cannot see the
        // job table either, and inventing entries for processes this
        // child does not own would be worse than the gap.
        //
        // In reverse, because each `pushd -n` goes on the front.
        for dir in self.dir_stack.iter().rev() {
            s.push_str("pushd -n ");
            s.push_str(&crate::serialize::quote_literal(dir));
            s.push_str(" >/dev/null\n");
        }
        for (signal, action) in &self.traps {
            let code = match action {
                TrapAction::Ignore => String::new(),
                TrapAction::Run(code) => code.clone(),
            };
            s.push_str("trap -- ");
            s.push_str(&crate::serialize::quote_literal(&code));
            s.push_str(" SIG");
            s.push_str(&signal_name(*signal));
            s.push('\n');
        }
        for (name, spec) in &self.completions {
            s.push_str(&crate::compgen::format_spec(spec, name));
            s.push('\n');
        }
        // Whatever `shopt -s`/`-u` has been changed from its default.
        for (name, on) in &self.shopt_options {
            if shopt_default_on(name) == Some(*on) {
                continue;
            }
            s.push_str(if *on { "shopt -s " } else { "shopt -u " });
            s.push_str(name);
            s.push('\n');
        }
        // The `set` options go *last*: `set -e` applied any earlier
        // would make the preamble's own assignments the thing that
        // aborts the stage, and `set -u` would trip over a name the
        // preamble has not reached yet.
        for name in SET_O_OPTIONS {
            if self.shell_option_enabled(name) == Some(true) {
                s.push_str("set -o ");
                s.push_str(name);
                s.push('\n');
            }
        }
        s
    }

    // Runs `source` in a fresh, in-process Shell built via
    // new_virtual_child (see its own doc comment) instead of self-exec'ing
    // a real OS process -- the shared primitive behind every *foreground*
    // (...)/`$(...)`/`<(...)`/`>(...)` construct below (run_coproc and
    // run_multi's own pipeline stages still self-exec for real: they need
    // genuine OS-level concurrency this single-threaded interpreter
    // doesn't have, or -- for a backgrounded subshell -- to keep running
    // after this call returns, which an in-process call can't do either).
    // Since new_virtual_child already deep-clones vars/arrays/functions/
    // etc as real Rust data, no functions_preamble()-style
    // serialize-then-reparse round-trip is needed here at all.
    //
    // `stdio.stdout`, if given, is where *this* construct's own output
    // goes (a redirect/capture belonging to this call specifically, e.g.
    // command substitution's own capture file); if `None`, output flows
    // to wherever the *parent* shell's own output currently goes,
    // matching a real fork (which shares fd 1 exactly as inherited) -- a
    // bare `(cmd)` with no `>` of its own, nested inside `$(...)`, must
    // still land in the outer capture. Same reasoning for
    // `stdio.stdin`/the parent's own current stdin override. `stdio.stderr`/
    // the dup flags are only ever populated by run_compound_redirected
    // (`{ ...; } 2> file`/`&>`/`2>&1`) -- every other caller uses
    // ChildStdio::default() plus a stdout/stdin override.
    fn run_in_child_shell(&mut self, raw: &str, stdio: ChildStdio) -> ExecResult {
        self.run_body_in_child_shell(ChildBody::Source(raw), stdio)
    }

    /// `run_in_child_shell` for something that has already been parsed.
    ///
    /// A pipeline stage arrives as a `parser::Command`, not as text.
    /// Serializing it back to a string just so this could re-lex and
    /// re-parse it would be work for nothing, and worse than nothing
    /// where the round trip is not exact.
    fn run_command_in_child_shell(&mut self, cmd: &parser::Command, stdio: ChildStdio) -> ExecResult {
        self.run_body_in_child_shell(ChildBody::Parsed(cmd), stdio)
    }

    /// A virtual child wired to `stdio`, handed back rather than run.
    ///
    /// `run_body_in_child_shell` builds one and runs it immediately. A
    /// pipeline stage needs the two halves apart: every stage has to be
    /// built and connected before *any* of them runs, because each one
    /// blocks on a pipe the others are on the far end of.
    fn child_for_stdio(&mut self, mut stdio: ChildStdio) -> Shell {
        let mut child = self.new_virtual_child();

        let effective_stdin: Option<Rc<RefCell<SharedReaderState>>> = match stdio.stdin.take() {
            Some(f) => {
                Some(Rc::new(RefCell::new(SharedReaderState { file: f, pending: Vec::new(), coroutines: Rc::clone(&self.background_coroutines) })))
            }
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdin.clone()),
        };
        let effective_stdout: Option<std::fs::File> = match &stdio.stdout {
            Some(f) => f.try_clone().ok(),
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdout.as_ref().and_then(|f| f.try_clone().ok())),
        };
        let effective_stderr = self.effective_stderr(&stdio, &effective_stdout);
        child.stdio_override = if effective_stdin.is_some() || effective_stdout.is_some() || effective_stderr.is_some() {
            Some(Rc::new(RefCell::new(StdioOverride { stdin: effective_stdin, stdout: effective_stdout, stderr: effective_stderr })))
        } else {
            None
        };
        // No explicit stdout/stderr override => this construct has no
        // redirect of its own, so its sink should be *exactly* the
        // parent's current one (already fully resolved -- Real/Grid/
        // Capture/Builtin, whatever it is), not a fresh wrapper around it.
        child.sink = if stdio.redirects_output() {
            OutputSink::Builtin { previous: Box::new(self.sink.clone()), stdout: stdio.sink_stdout(), stderr: stdio.sink_stderr() }
        } else {
            self.sink.clone()
        };
        child
    }

    fn run_body_in_child_shell(&mut self, body: ChildBody<'_>, stdio: ChildStdio) -> ExecResult {
        let mut child = self.child_for_stdio(stdio);
        // A *subshell* resets the traps it caught -- `$( )` and `( )`
        // arrive here as source text -- so a DEBUG trap does not fire
        // once per command inside a command substitution, and an ERR
        // trap does not fire both inside `( false )` and again for the
        // subshell itself. bash does the same, and the EXIT trap has
        // its own version of this rule (see run_exit_trap).
        //
        // A pipeline stage arrives already parsed and does *not* reset
        // them: bash fires DEBUG for each stage of `echo a | cat`, and
        // so does this. Neither does a redirected group, which is not a
        // subshell at all.
        if matches!(body, ChildBody::Source(_)) {
            child.debug_trap = None;
            child.err_trap = None;
            child.return_trap = None;
        }

        // The real OS cwd is process-wide, shared with the real parent,
        // even though `child` is otherwise a fully independent Shell -- a
        // `cd` inside this construct (`$(cd /tmp && pwd)`) must not leak
        // back out to the real shell once this call returns.
        let real_cwd_before = std::env::current_dir().ok();
        // Variables need nothing here any more, and the absence is
        // deliberate. A plain `x=2` inside this construct used to write
        // straight to the real process environment, which is shared
        // with the parent -- so isolating it meant snapshotting the
        // whole environment on the way in and replaying it on the way
        // out, O(env) to take and O(env^2) to restore. `child` owns its
        // own `globals`, and a spawned process is handed those rather
        // than reading `environ` (see `Shell::command`), so the
        // isolation is now a property of the data rather than something
        // reconstructed around every construct.
        // Same reasoning, for `umask` (a real process-wide syscall, not
        // Shell-owned state either -- see current_umask's own doc
        // comment).
        let umask_before = current_umask();
        // Same reasoning again, for fd 0/1/2 -- a bare `exec > file`/
        // `exec 2>&1`/`exec < file` (no command word, applies its
        // redirect persistently to *this process's own* fds rather than
        // spawning anything -- see the "exec" builtin arm's own doc
        // comment) used to be safely contained by the old re-exec'd
        // design (a real separate process, discarded along with whatever
        // it did to its own fds once it exited); in-process, it would
        // otherwise silently repoint the real shell's own stdin/stdout/
        // stderr forever, confirmed the hard way: `(exec > file; echo hi)`
        // left *everything* printed after that subshell -- including in
        // the real parent script -- redirected into `file` too.
        let saved_fd012 = save_fd012();

        let result = match body {
            ChildBody::Source(raw) => child.run_source_here(raw, "subshell"),
            ChildBody::Parsed(cmd) => crate::builtins::shell::run_command(&mut child, cmd, false),
        };
        // Real bash fires a subshell's own EXIT trap when it finishes
        // normally too, not just on an explicit `exit`/errexit (confirmed:
        // `(trap "echo bye" EXIT; echo hi)` prints both). The re-exec'd
        // design this replaces got this for free (main.rs's run_source
        // unconditionally runs it after a real child process's own
        // run_program returns) -- ExecResult::Exit's own producer already
        // ran it before bubbling, so only run it again here for every
        // *other* outcome, to avoid firing it twice.
        if !matches!(result, ExecResult::Exit(_)) {
            child.run_exit_trap();
        }

        if let Some(d) = real_cwd_before {
            let _ = std::env::set_current_dir(d);
        }
        unsafe { umask(umask_before) };
        restore_fd012(saved_fd012);

        match result {
            // A subshell's own `exit`/`set -e`/`set -u` must not kill the
            // real enclosing shell -- matches real bash's fork isolation
            // for `(exit 3)`. The exit trap already ran wherever this was
            // produced (see ExecResult::Exit's own doc comment).
            ExecResult::Exit(code) => ExecResult::Status(code),
            other => other,
        }
    }

    // run_compound_redirected's own in-process primitive -- deliberately
    // NOT run_in_child_shell/new_virtual_child, since a redirected
    // compound command (`{ ...; } > file`, `while ...; done < file`) is
    // *not* a subshell: unlike `(...)`/`$(...)`, real bash shares every
    // bit of state (variables, cwd, functions, everything) between a
    // `{ }` group and its enclosing shell -- only stdio is different for
    // its own duration. So this runs `cmd` directly on `self`, with
    // `self.sink`/`self.stdio_override` temporarily swapped for exactly
    // this call and restored after -- the same "push a temporary
    // override, run, pop it back" shape push_builtin_output_sink/
    // pop_builtin_output_sink already use for a single builtin's own
    // redirects, generalized here to a whole compound command.
    fn run_with_redirected_stdio(&mut self, cmd: &parser::Command, stdio: ChildStdio) -> ExecResult {
        let saved = self.install_redirected_stdio(stdio);
        let result = self.run_command_body(cmd, false);
        self.restore_redirected_stdio(saved);
        result
    }

    // The install/restore halves of run_with_redirected_stdio, split out
    // so a *function call* can borrow them: `f > out` redirects the
    // whole body, exactly as `{ ...; } > out` does, and routing it
    // through the compound path is the only way a builtin inside the
    // body and an external inside it end up in the same place.
    fn install_redirected_stdio(&mut self, mut stdio: ChildStdio) -> (OutputSink, Option<Rc<RefCell<StdioOverride>>>) {
        let saved_sink = self.sink.clone();
        let saved_stdio_override = self.stdio_override.clone();

        let effective_stdin: Option<Rc<RefCell<SharedReaderState>>> = match stdio.stdin.take() {
            Some(f) => {
                Some(Rc::new(RefCell::new(SharedReaderState { file: f, pending: Vec::new(), coroutines: Rc::clone(&self.background_coroutines) })))
            }
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdin.clone()),
        };
        let effective_stdout: Option<std::fs::File> = match &stdio.stdout {
            Some(f) => f.try_clone().ok(),
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdout.as_ref().and_then(|f| f.try_clone().ok())),
        };
        let effective_stderr = self.effective_stderr(&stdio, &effective_stdout);
        self.stdio_override = if effective_stdin.is_some() || effective_stdout.is_some() || effective_stderr.is_some() {
            Some(Rc::new(RefCell::new(StdioOverride { stdin: effective_stdin, stdout: effective_stdout, stderr: effective_stderr })))
        } else {
            None
        };
        if stdio.redirects_output() {
            self.sink = OutputSink::Builtin { previous: Box::new(saved_sink.clone()), stdout: stdio.sink_stdout(), stderr: stdio.sink_stderr() };
        }

        (saved_sink, saved_stdio_override)
    }

    fn restore_redirected_stdio(&mut self, saved: (OutputSink, Option<Rc<RefCell<StdioOverride>>>)) {
        (self.sink, self.stdio_override) = saved;
    }

    // (...) subshells run in-process now (see run_in_child_shell's own
    // doc comment) for the foreground case; the backgrounded case still
    // self-execs the bish binary on the raw captured source, since it
    // needs to keep running concurrently with whatever the parent does
    // next, which nothing in this single-threaded interpreter can do
    // in-process.
    fn run_subshell(&mut self, raw: &str, background: bool) -> i32 {
        if !background {
            return self.run_in_child_shell(raw, ChildStdio::default()).status();
        }
        self.spawn_background_script(raw, format!("({})", raw))
    }

    /// Runs `raw` as a background job: a real child, with this shell's
    /// functions in front of it, its own pty, and an entry in the job
    /// table so `$!`, `jobs` and `wait` can all see it.
    ///
    /// What `( ) &` has always done, and what `&` on anything else that
    /// runs in this shell -- a group, a loop, a builtin, a function --
    /// now does too. `label` is what `jobs` shows.
    fn spawn_background_script(&mut self, raw: &str, label: String) -> i32 {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                sh_eprintln!(self, "bish: subshell: {}", e);
                return 1;
            }
        };
        let script = self.functions_preamble() + raw;
        let mut command = self.command(exe);
        command.arg("-c").arg(script).current_dir(&self.cwd);
        // Attached to its own pty, for exactly the reason run_single's own
        // background spawn already is (see `use_pty` there): inherited
        // stdio writes straight onto whatever the real screen happens to
        // show, and the compositor's next redraw -- painted from the
        // session's grid, which never saw a byte of it -- silently wipes
        // it. Nothing here needs the process-group isolation that would
        // conflict with spawn_attached's own setsid, so this gets the
        // full treatment rather than just the fds.
        let spawned = match self.background_pty() {
            Some(p) => pty::spawn_attached(command, &p.slave_path).map(|child| (child, Some(p.master))),
            None => command.spawn().map(|child| (child, None)),
        };
        match spawned {
            Ok((child, master)) => {
                self.push_job_with_pty(vec![child], label, master);
                0
            }
            Err(e) => {
                sh_eprintln!(self, "bish: subshell: {}", e);
                1
            }
        }
    }

    // `coproc [NAME] command` -- see the Command::Coproc doc comment for
    // the (scoped) grammar this accepts. Runs `body` as a background
    // process wired to two pipes: NAME[0] is an fd the shell can read the
    // coprocess's stdout from, NAME[1] an fd it can write to the
    // coprocess's stdin. NAME_PID gets the coprocess's PID. The coprocess
    // is also registered as an ordinary background job (visible in
    // `jobs`/`wait`, same as real bash).
    fn run_coproc(&mut self, name: Option<String>, body: &parser::Command) -> i32 {
        use std::os::fd::AsRawFd;
        let name = name.unwrap_or_else(|| "COPROC".to_string());
        let (out_r, out_w) = match std::io::pipe() {
            Ok(p) => p,
            Err(e) => {
                sh_eprintln!(self, "bish: coproc: {}", e);
                return 1;
            }
        };
        let (in_r, in_w) = match std::io::pipe() {
            Ok(p) => p,
            Err(e) => {
                sh_eprintln!(self, "bish: coproc: {}", e);
                return 1;
            }
        };
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                sh_eprintln!(self, "bish: coproc: {}", e);
                return 1;
            }
        };
        let script = self.functions_preamble() + &crate::serialize::serialize_command(body);
        let mut command = self.command(exe);
        command.arg("-c").arg(script);
        command.current_dir(&self.cwd);
        command.stdin(Stdio::from(in_r));
        command.stdout(Stdio::from(out_w));
        match command.spawn() {
            Ok(child) => {
                let out_r_fd = out_r.as_raw_fd();
                let in_w_fd = in_w.as_raw_fd();
                self.coproc_fds.insert(out_r_fd, KeptFd::Read(std::io::BufReader::new(out_r)));
                self.coproc_fds.insert(in_w_fd, KeptFd::Write(in_w));
                let mut map = std::collections::BTreeMap::new();
                map.insert(0, out_r_fd.to_string());
                map.insert(1, in_w_fd.to_string());
                self.arrays.insert(name.clone(), map);
                self.assign_var(&format!("{}_PID", name), child.id().to_string());
                let cmd_text = crate::serialize::serialize_command(body);
                self.push_job(vec![child], cmd_text);
                0
            }
            Err(e) => {
                sh_eprintln!(self, "bish: coproc: {}", e);
                1
            }
        }
    }

    // Compound commands (if/while/for/case/group) with a trailing redirect
    // (`{ ...; } > file`, `done < file`) run in-process now (via
    // run_in_child_shell) for the foreground, plain-fd-0/1/2 case --
    // avoids the re-exec round-trip and, as a side effect, keeps variable/
    // cwd mutations inside the block visible to the rest of the script
    // (matching a real bash `{ }` group, which is *not* a subshell).
    // Falls back to the old self-exec'd path for anything this shell has
    // no in-process model for: a numbered-fd redirect (no real child
    // process here to dup2 it onto -- see resolve_simple_redirects_for_
    // compound), or a *backgrounded* run, which needs to keep going after
    // this call returns (nothing in this single-threaded interpreter can
    // do that in-process).
    pub(crate) fn run_compound_redirected(&mut self, cmd: &parser::Command, redirects: &[Redirect], background: bool) -> ExecResult {
        if !background && Self::compound_redirects_are_simple(redirects) {
            return match self.resolve_simple_redirects_for_compound(redirects) {
                Ok(stdio) => self.run_with_redirected_stdio(cmd, stdio),
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    ExecResult::Status(1)
                }
            };
        }
        let redirs = match self.resolve_redirect_list(redirects) {
            Ok(r) => r,
            Err(e) => {
                sh_eprintln!(self, "bish: {}", e);
                return ExecResult::Status(1);
            }
        };
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                sh_eprintln!(self, "bish: {}", e);
                return ExecResult::Status(1);
            }
        };
        let script = self.functions_preamble() + &crate::serialize::serialize_command(cmd);
        let mut command = self.command(exe);
        command.arg("-c").arg(script);
        command.current_dir(&self.cwd);
        // Whichever of this job's own streams the redirects *didn't*
        // claim goes to a pty of its own, for the same reason run_multi's
        // backgrounded pipelines do (see background_pty): redirecting one
        // stream says nothing about where the others should land, and
        // inherited they write straight onto the real screen for the next
        // repaint to wipe -- `{ cmd; } 2>err &` lost every line of stdout
        // exactly that way. Wired as plain fds rather than through
        // spawn_attached, whose setsid would undo the process-group
        // isolation set up just below.
        let bg_pty = if background { self.background_pty() } else { None };
        let bg_slave = |pty: &Option<pty::Pty>| -> Option<Stdio> {
            let p = pty.as_ref()?;
            std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok().map(Stdio::from)
        };
        // Only the foreground case (reached here for a numbered-fd
        // redirect this shell has no in-process model for -- see
        // compound_redirects_are_simple's own doc comment) should honor a
        // converted enclosing capture.
        if background {
            command.stdin(bg_slave(&bg_pty).unwrap_or_else(Stdio::inherit));
            command.stdout(bg_slave(&bg_pty).unwrap_or_else(Stdio::inherit));
        } else {
            command.stdin(self.spawn_stdin_stdio());
            command.stdout(self.spawn_stdout_stdio());
        }
        command.stderr(bg_slave(&bg_pty).unwrap_or_else(|| self.spawn_stderr_stdio()));
        apply_fd_redirects(&mut command, redirs.actions);
        // Real job control isolation for a *backgrounded* redirected
        // compound command only -- same reasoning/pattern as run_single's
        // own pre_exec hook (set from both the child, here, and the
        // parent right after spawn() returns, to avoid the classic job-
        // control race) and run_multi's own identical treatment of a
        // backgrounded pipeline: `kill %N`/`bg`'s own SIGCONT then
        // targets this whole (single-process) group correctly, and a
        // *foreground* run of this same construct is left with today's
        // "no isolation, inherits bish's own group" behavior, since there's
        // no tcsetpgrp/stop-handling machinery here to keep a foreground
        // Ctrl-C routing to it once isolated.
        if background && self.opt_monitor {
            unsafe {
                command.pre_exec(|| {
                    setpgid(0, 0);
                    sigaction_raw(crate::term::SIGTTIN, SIG_DFL);
                    sigaction_raw(crate::term::SIGTTOU, SIG_DFL);
                    Ok(())
                });
            }
        }
        match command.spawn() {
            Ok(child) => {
                if background {
                    let cmd_text = crate::serialize::serialize_command(cmd);
                    let pgid = if self.opt_monitor {
                        let pid = child.id() as i32;
                        unsafe { setpgid(pid, pid) };
                        Some(pid as u32)
                    } else {
                        None
                    };
                    self.push_job_full(vec![child], cmd_text, bg_pty.map(|p| p.master), pgid);
                    ExecResult::Status(0)
                } else {
                    let mut child = child;
                    match child.wait() {
                        Ok(status) => ExecResult::Status(exit_code_from_status(status)),
                        Err(e) => {
                            sh_eprintln!(self, "bish: {}", e);
                            ExecResult::Status(1)
                        }
                    }
                }
            }
            Err(e) => {
                sh_eprintln!(self, "bish: {}", e);
                ExecResult::Status(127)
            }
        }
    }

    /// Drops every NUL byte from a command substitution's output,
    /// saying so once if there were any -- bash's own handling, message
    /// included.
    ///
    /// The byte cannot survive into a shell word whatever is done with
    /// it: a word ends up as an argument to `execve`, which takes C
    /// strings and stops at the first NUL. So the choice is between
    /// dropping it quietly and dropping it out loud, and a command that
    /// produced binary output where a string was expected is worth a
    /// line on stderr. Roadmap 11 took NUL bytes out of the variable
    /// path; this is the same byte arriving through a different door,
    /// where it used to reach the user as a Rust `CString` error.
    fn strip_nuls_from_substitution(&mut self, text: String) -> String {
        if !text.contains('\0') {
            return text;
        }
        sh_eprintln!(self, "bish: line {}: warning: command substitution: ignored null byte in input", self.current_line);
        text.replace('\0', "")
    }

    fn run_command_substitution(&mut self, raw: &str) -> String {
        // `$(<file)`: bash's no-fork file read, and the idiomatic way a
        // script reads a pidfile or something out of /proc. Parsed as a
        // command substitution containing only a redirect, which ran
        // nothing and produced nothing.
        if let Some(path) = raw.trim().strip_prefix('<') {
            let path = self.expand_raw(path.trim());
            let mut text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) => {
                    sh_eprintln!(self, "bish: {}: {}", path, os_message(&e));
                    self.last_subst_status = Some(1);
                    return String::new();
                }
            };
            // The same trailing-newline stripping every substitution
            // does.
            while text.ends_with('\n') {
                text.pop();
            }
            self.last_subst_status = Some(0);
            return self.strip_nuls_from_substitution(text);
        }
        // No temp file when the kernel can give an anonymous one -- see
        // capture_file's own doc comment. `path` stays None in that case
        // and nothing below has a name to read back or delete.
        let mem = capture_file();
        let path = if mem.is_some() { None } else { Some(proc_sub_temp_path()) };
        let file = match &path {
            None => mem.as_ref().and_then(|f| f.try_clone().ok()),
            Some(p) => std::fs::File::create(p).ok(),
        };
        let Some(file) = file else { return String::new() };
        // `set -e` does not reach inside `$( )` unless
        // `shopt -s inherit_errexit` says so -- that option exists
        // precisely because it does not. A `( )` subshell is different
        // and does inherit it, so this is scoped to command
        // substitution rather than to run_in_child_shell.
        let errexit = self.opt_errexit;
        if !self.shopt_is_on("inherit_errexit") {
            self.opt_errexit = false;
        }
        let result = self.run_in_child_shell(raw, ChildStdio { stdout: Some(file), ..Default::default() });
        self.opt_errexit = errexit;
        self.last_subst_status = Some(result.status());
        let mut s = match (&path, mem) {
            (None, Some(f)) => read_capture(f),
            (Some(p), _) => {
                let text = std::fs::read_to_string(p).unwrap_or_default();
                let _ = std::fs::remove_file(p);
                text
            }
            _ => String::new(),
        };
        while s.ends_with('\n') {
            s.pop();
        }
        self.strip_nuls_from_substitution(s)
    }

    // `<(cmd)`: runs cmd to completion now, capturing its stdout into a
    // temp file, and substitutes that file's path. Real bash streams this
    // concurrently through a FIFO; see the ProcSubIn/ProcSubOut doc comment
    // in lexer.rs for why this shell uses a temp file instead. The path is
    // queued for cleanup (self.proc_sub_cleanup) once the enclosing command
    // has finished reading it.
    /// The single external command a `<( )` body is, if that is all it
    /// is.
    ///
    /// `<(yes)`, `<(seq 1 10)`, `<(cat f)` -- the overwhelmingly common
    /// shapes -- need no shell at all, and giving them one is what made
    /// them hard: a coroutine that spawns a process has to wait for it,
    /// and waiting is the one thing a coroutine cannot do. Spawned
    /// directly they are exactly what bash produces, a real process
    /// writing into the pipe, which ends by itself when the reader goes.
    fn proc_sub_plain_command(&mut self, raw: &str) -> Option<Vec<String>> {
        let toks = crate::lexer::Lexer::new(raw).tokenize().ok()?;
        let prog = crate::parser::Parser::new(self.expand_aliases(toks)).parse_program().ok()?;
        let [item] = prog.as_slice() else { return None };
        if !item.and_or.rest.is_empty() || item.and_or.first.negate || item.and_or.first.timed.is_some() {
            return None;
        }
        let [command] = item.and_or.first.commands.as_slice() else { return None };
        if self.stage_needs_interpreter(command) {
            return None;
        }
        let parser::Command::Simple(sc) = command else { return None };
        // A redirect or an assignment prefix is the shell's business,
        // not a bare spawn's.
        if !sc.redirects.is_empty() || !sc.assigns.is_empty() {
            return None;
        }
        let argv = self.expand_words(&sc.words);
        (!argv.is_empty()).then_some(argv)
    }

    fn run_proc_sub_in(&mut self, raw: &str) -> String {
        let (read_end, write_end) = match make_pipe() {
            Ok(pair) => pair,
            Err(e) => {
                sh_eprintln!(self, "bish: process substitution: {}", e);
                return String::new();
            }
        };
        match self.proc_sub_plain_command(raw) {
            // A real command: a real process, as in bash. Nothing to
            // schedule, and it ends on its own when the reader goes.
            Some(argv) => {
                let mut command = self.command(&argv[0]);
                command.args(&argv[1..]);
                command.stdout(Stdio::from(write_end));
                command.stdin(self.spawn_stdin_stdio());
                match command.spawn() {
                    Ok(child) => self.proc_sub_children.push((child, true)),
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}: {}", argv[0], os_message(&e));
                        return String::new();
                    }
                }
            }
            // Anything that needs a shell runs as a coroutine, given
            // time wherever this shell would otherwise be idle.
            None => {
                let mut child = self.new_virtual_child();
                let body = raw.to_string();
                let started = self.background_coroutines.borrow_mut().add_with_fds(
                    move || {
                        // Armed for the same reason a pipeline stage
                        // arms it: this body writes into a pipe whose
                        // reader can go away, and `EPIPE` is the only
                        // way it can find out. Without it an unbounded
                        // producer never stops -- and since the drain
                        // now gives bodies turns *before* cancelling
                        // any (which a `>( )` consumer needs to write
                        // its answer), one of those turns never
                        // returned: the loop spun inside a single
                        // resume with the whole thread in it.
                        arm_broken_pipe();
                        child.run_source_here(&body, "process substitution");
                        child.run_exit_trap();
                        disarm_broken_pipe();
                    },
                    None,
                    Some(write_end),
                    true,
                );
                if let Err(e) = started {
                    sh_eprintln!(self, "bish: process substitution: {}", e);
                    return String::new();
                }
            }
        }

        // `/dev/fd/N` rather than a FIFO. Opening a FIFO blocks until
        // the other end is opened too, and with one thread the other
        // end is a coroutine that cannot run while this one is blocked
        // in the kernel -- the deadlock is structural. `/dev/fd` names
        // the pipe that already exists, so there is nothing to wait for.
        // The consuming command opens the name below, and for an
        // external one that means inheriting this descriptor across
        // `exec` -- so this end, unlike every other pipe this shell
        // makes, must not close on it. *After* the producer is started,
        // never before: it would otherwise inherit the read end of its
        // own output pipe and hold it open against itself.
        clear_cloexec(std::os::fd::AsRawFd::as_raw_fd(&read_end));
        let name = format!("/dev/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&read_end));
        self.proc_sub_pipes.push(read_end);
        name
    }

    /// `child.wait()`, giving any live coroutine the time this shell
    /// would otherwise spend blocked in the kernel.
    ///
    /// This is the whole reason `<( )` can stream: the command reading
    /// the substitution is what this is waiting for, and the producer
    /// filling it is a coroutine that has to run *while* that happens.
    /// With nothing live it is a plain wait -- one `try_wait` more than
    /// before, and no polling.
    fn wait_pumping(&self, child: &mut std::process::Child) -> std::io::Result<std::process::ExitStatus> {
        // Inside a coroutine there is no such thing as waiting: this
        // thread is shared, and blocking in the kernel takes every
        // other stage down with it -- including whichever one would
        // have let this child finish. `<(yes)` deadlocked exactly
        // there: the producer blocked waiting for `yes`, so the shell
        // never noticed `head` had exited, so nothing ever closed the
        // pipe `yes` was writing to.
        if crate::coroutine::in_coroutine() {
            loop {
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
                crate::scheduler::park_ready();
            }
        }
        if !self.has_live_coroutines() {
            return child.wait();
        }
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if !self.has_live_coroutines() {
                // Nothing left to give time to, so go back to blocking
                // rather than spinning on `try_wait`.
                return child.wait();
            }
            self.pump_coroutines();
        }
    }

    /// Gives any coroutine that outlives its command a turn.
    ///
    /// Called where the shell would otherwise be idle in the kernel --
    /// waiting for a child, waiting for input. That is the whole of the
    /// scheduling policy for these: they run in the gaps, which is
    /// exactly when the thing they are feeding is not running either.
    pub(crate) fn pump_coroutines(&self) {
        let Ok(mut scheduler) = self.background_coroutines.try_borrow_mut() else {
            // Already inside a step: a coroutine reached a point that
            // pumps. It is already running; there is nothing to do.
            return;
        };
        scheduler.step();
    }

    /// Whether anything is waiting for a turn.
    pub(crate) fn has_live_coroutines(&self) -> bool {
        self.background_coroutines.try_borrow().is_ok_and(|s| !s.is_idle())
    }

    // `>(cmd)`: substitutes a temp file path immediately (so the enclosing
    // command can write to it like any other file), and queues cmd to run
    // reading that file back once the enclosing command finishes -- correct
    // data flow, but sequential rather than concurrent (see lexer.rs).
    /// `>(cmd)`: the mirror of `<(cmd)`. The enclosing command writes
    /// to the name and `cmd` reads it, concurrently -- rather than the
    /// body being queued to run over a temp file once the command has
    /// finished.
    ///
    /// The old shape had the right data flow at the wrong time:
    /// `echo hi > >(cat); echo done` printed `hi done` where bash
    /// prints `done hi`, and a body that needed to see output as it was
    /// produced never could.
    fn run_proc_sub_out(&mut self, raw: &str) -> String {
        let (read_end, write_end) = match make_pipe() {
            Ok(pair) => pair,
            Err(e) => {
                sh_eprintln!(self, "bish: process substitution: {}", e);
                return String::new();
            }
        };
        match self.proc_sub_plain_command(raw) {
            // A real command reading the pipe, as in bash. It ends on
            // its own when the writing end closes.
            Some(argv) => {
                let mut command = self.command(&argv[0]);
                command.args(&argv[1..]);
                command.stdin(Stdio::from(read_end));
                command.stdout(self.spawn_stdout_stdio());
                match command.spawn() {
                    Ok(child) => self.proc_sub_children.push((child, false)),
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}: {}", argv[0], os_message(&e));
                        return String::new();
                    }
                }
            }
            None => {
                let mut child = self.new_virtual_child();
                let body = raw.to_string();
                let started = self.background_coroutines.borrow_mut().add_with_fds(
                    move || {
                        // Armed for the same reason a pipeline stage
                        // arms it: this body writes into a pipe whose
                        // reader can go away, and `EPIPE` is the only
                        // way it can find out. Without it an unbounded
                        // producer never stops -- and since the drain
                        // now gives bodies turns *before* cancelling
                        // any (which a `>( )` consumer needs to write
                        // its answer), one of those turns never
                        // returned: the loop spun inside a single
                        // resume with the whole thread in it.
                        arm_broken_pipe();
                        child.run_source_here(&body, "process substitution");
                        child.run_exit_trap();
                        disarm_broken_pipe();
                    },
                    Some(read_end),
                    None,
                    // Not cancellable: see `Task::cancellable`.
                    false,
                );
                if let Err(e) = started {
                    sh_eprintln!(self, "bish: process substitution: {}", e);
                    return String::new();
                }
            }
        }

        // As in `run_proc_sub_in`, and after the body is started for
        // the same reason -- more sharply here: a body that inherited
        // the write end of its own *input* pipe holds it open against
        // itself and never reaches end-of-input at all. `>(cat)` hides
        // that by echoing as it reads; `>(wc -l)`, which answers only
        // at end-of-input, answers nothing.
        clear_cloexec(std::os::fd::AsRawFd::as_raw_fd(&write_end));
        let name = format!("/dev/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&write_end));
        self.proc_sub_pipes.push(write_end);
        name
    }

    // Runs any `>(cmd)` substitutions queued by the command that just
    // finished, then deletes every proc-sub temp file used this round.
    fn drain_proc_subs(&mut self) {
        for path in self.proc_sub_cleanup.drain(..) {
            let _ = std::fs::remove_file(path);
        }
        // Closing this shell's own ends is what tells both kinds of
        // substitution that the command is over: a `<( )` producer's
        // next write fails, and a `>( )` consumer sees end-of-input.
        self.proc_sub_pipes.clear();
        // Then let them finish. A consumer *has* to be given this --
        // `printf x > >(wc -l)` has only just been handed its input --
        // and a producer takes a turn or two to notice and stop.
        for _ in 0..PROC_SUB_WINDDOWN_TURNS {
            if !self.has_live_coroutines() {
                break;
            }
            self.pump_coroutines();
        }
        // Whatever is still going is a producer with nobody listening.
        if let Ok(mut scheduler) = self.background_coroutines.try_borrow_mut() {
            scheduler.cancel_running();
            scheduler.retire_finished();
        }
        // And the ones that are real processes: a producer nobody is
        // reading is stopped, a consumer is left alone and reaped when
        // it finishes. Waiting for a consumer would make the shell
        // block on something it deliberately started in the background,
        // and bash does not.
        let mut still_running = Vec::new();
        for (mut child, may_stop) in std::mem::take(&mut self.proc_sub_children) {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => continue,
                Ok(None) => {}
            }
            if may_stop {
                let _ = child.kill();
                let _ = child.wait();
                continue;
            }
            still_running.push((child, may_stop));
        }
        self.proc_sub_children = still_running;
    }

    /// `FUNCNEST`, if a script has set it to something meaningful.
    ///
    /// Deliberately not `lookup_var`: that ends in a `std::env::var`
    /// fallback, i.e. a linear scan of `environ`, and this runs on every
    /// single function call. A `FUNCNEST` inherited from the environment
    /// is already in `globals` (seeded there at startup), so nothing is
    /// lost by stopping short of the real environment.
    fn funcnest_limit(&self) -> Option<usize> {
        let raw = self
            .var_scopes
            .iter()
            .rev()
            .find_map(|s| s.get("FUNCNEST"))
            .map(|v| v.as_deref().unwrap_or_default())
            .or_else(|| self.globals.get("FUNCNEST").map(String::as_str))?;
        match raw.trim().parse::<usize>() {
            Ok(0) | Err(_) => None,
            Ok(n) => Some(n),
        }
    }

    /// Is this call to `name` provably a repeat of one already on the
    /// stack, with nothing at all having happened in between?
    ///
    /// Not a heuristic and not a depth guess. `Shell::effects` counts every
    /// builtin, every external command, every assignment and every read of
    /// a volatile variable, and is deliberately *not* bumped by a function
    /// call itself. So if some frame for this same function recorded the
    /// same `effects` value this call sees, then between that entry and
    /// this one the interpreter executed nothing observable -- it only
    /// pushed frames. If the positional parameters also match, the whole
    /// reachable state is identical to what it was at that earlier entry,
    /// and the program is at a fixed point: it will do this forever.
    ///
    /// O(1) in practice. The scan walks back from the innermost frame and
    /// stops at the first frame whose `effects_at_entry` differs, which in
    /// any program that does anything is the very first frame examined.
    /// It only walks far in exactly the case it is looking for -- a run of
    /// frames entered with no effect between them, i.e. the cycle itself,
    /// which is why mutual recursion (`a() { b; }; b() { a; }`) is caught
    /// too without the caller having to say how deep to look.
    ///
    /// Returns the number of frames in the cycle when it fires.
    fn nonproductive_recursion_depth(&self, name: &str, args_hash: u64) -> Option<usize> {
        let here = self.effects;
        for (back, frame) in self.call_stack.iter().rev().enumerate() {
            if frame.effects_at_entry != here {
                return None;
            }
            if frame.called == name && frame.args_hash == args_hash {
                return Some(back + 1);
            }
        }
        None
    }

    /// `Some` when this call must not happen: either `FUNCNEST` says
    /// so, or there is not enough stack left to survive it.
    ///
    /// bash only has the first of those, and dies on a signal without
    /// it -- `bish -c 'f() { f; }; f'` used to abort and dump core, and
    /// so does bash. The stack check is what makes the limit real when
    /// nobody has set `FUNCNEST`, which is nearly always; see
    /// stackguard's own doc comment for why it measures the stack
    /// rather than counting frames.
    ///
    /// The message is bash's, including the line number and the
    /// parenthesised limit. With no `FUNCNEST` set there is no
    /// configured number to report, so it reports the depth actually
    /// reached, which is the limit that applied.
    fn refuse_deeper_nesting(&mut self, name: &str, args_hash: u64) -> Option<ExecResult> {
        // Already unwinding out of a refusal further down: every call
        // met on the way out is refused too, silently -- the message
        // has been given once, which is what bash's jump to the top
        // level amounts to.
        if self.nesting_unwind {
            return Some(ExecResult::Status(1));
        }
        // bash reads FUNCNEST as a number and treats anything else --
        // unset, zero, negative, `abc` -- as "no limit".
        //
        // Checked ahead of the non-terminating-recursion proof below,
        // even though the proof is the better diagnosis and fires
        // immediately where this waits for a depth: a script that sets
        // `FUNCNEST` is asking for bash's mechanism by name, and gets
        // it, message and depth included. Every script that does not --
        // which is nearly all of them -- gets the proof.
        let funcnest = self.funcnest_limit();
        let proven = funcnest.is_none().then(|| self.nonproductive_recursion_depth(name, args_hash)).flatten();
        let message = match (funcnest, proven) {
            (Some(limit), _) if self.function_depth >= limit => {
                format!("bish: line {}: {name}: maximum function nesting level exceeded ({limit})\n", self.current_line)
            }
            (_, Some(1)) => format!(
                "bish: line {}: {name}: called itself with no command run and no argument changed -- this cannot terminate\n",
                self.current_line
            ),
            (_, Some(cycle)) => format!(
                "bish: line {}: {name}: re-entered through a cycle of {cycle} calls with no command run and no argument changed -- this cannot terminate\n",
                self.current_line
            ),
            // The backstop under both of the above, and the reason a
            // `FUNCNEST` too large to ever be reached is not a way back
            // to a core dump. Reports the depth actually reached, since
            // the limit that applied here is the stack rather than a
            // configured number.
            _ if crate::stackguard::nearly_exhausted() => {
                format!("bish: line {}: {name}: maximum function nesting level exceeded ({})\n", self.current_line, self.function_depth)
            }
            _ => return None,
        };
        self.sink_err(&message);
        self.nesting_unwind = true;
        // bash stops the script outright -- nothing after the offending
        // call runs, and the shell exits 1. It does not do that to a
        // session someone is typing at, and neither does this: there
        // the unwind flag alone gets back to the prompt. Same line the
        // readonly-assignment error draws.
        if !self.interactive {
            self.pending_exit = Some(1);
        }
        Some(ExecResult::Status(1))
    }

    fn call_function(&mut self, name: &str, body: &parser::Command, call_args: Vec<String>) -> ExecResult {
        let args_hash = args_fingerprint(&call_args);
        if let Some(refused) = self.refuse_deeper_nesting(name, args_hash) {
            return refused;
        }
        // Recorded before the body runs, so `current_line` is still the
        // line of the call rather than of whatever the body reaches
        // first.
        self.call_stack.push(CallFrame {
            called: name.to_string(),
            call_line: self.current_line,
            source: self.script_name.clone(),
            effects_at_entry: self.effects,
            args_hash,
        });
        self.refresh_call_arrays();
        self.arg_frames.push(call_args);
        self.function_depth += 1;
        self.var_scopes.push(HashMap::new());
        self.array_local_stack.push(Vec::new());
        self.assoc_local_stack.push(Vec::new());
        self.nameref_local_stack.push(Vec::new());
        let result = crate::builtins::shell::run_command(self, body, false);
        self.call_stack.pop();
        self.refresh_call_arrays();
        self.var_scopes.pop();
        if let Some(frame) = self.nameref_local_stack.pop() {
            for (name, was_nameref) in frame.into_iter().rev() {
                if !was_nameref {
                    self.nameref_names.remove(&name);
                }
            }
        }
        if let Some(frame) = self.array_local_stack.pop() {
            for (name, prev, was_declared) in frame.into_iter().rev() {
                match prev {
                    Some(v) => {
                        self.arrays.insert(name.clone(), v);
                    }
                    None => {
                        self.arrays.remove(&name);
                    }
                }
                if was_declared {
                    self.array_names.insert(name);
                } else {
                    self.array_names.remove(&name);
                }
            }
        }
        if let Some(frame) = self.assoc_local_stack.pop() {
            for (name, prev, was_declared) in frame.into_iter().rev() {
                match prev {
                    Some(v) => {
                        self.assoc_arrays.insert(name.clone(), v);
                    }
                    None => {
                        self.assoc_arrays.remove(&name);
                    }
                }
                if was_declared {
                    self.assoc_names.insert(name);
                } else {
                    self.assoc_names.remove(&name);
                }
            }
        }
        self.arg_frames.pop();
        let returning_from = self.function_depth;
        self.function_depth -= 1;
        // Back at the top: whatever was being unwound out of is fully
        // unwound, and the shell can run commands again.
        if self.function_depth == 0 {
            self.nesting_unwind = false;
        }
        // RETURN, after the frame is gone so the trap runs in the
        // caller's scope. It fires for the function that set it and for
        // every one it returns *into* -- not for calls made from there,
        // which is what "not inherited" means and what `functrace`
        // turns off. Four cases pin the comparison down:
        //
        //   trap RETURN; f() { :; }; f          set at 0, f returns at 1: no
        //   f() { trap RETURN; :; }; f          set at 1, f returns at 1: yes
        //   f() { trap RETURN; g; }; g          set at 1, g returns at 2: no
        //   g() { trap RETURN; :; }; f() { g; } set at 2, f returns at 1: yes
        //
        // This used to be gated on `functrace` alone, so the second --
        // the ordinary way anyone writes a RETURN trap -- never ran.
        if self.opt_functrace || returning_from <= self.pseudo_trap_depth[PseudoTrap::Return as usize] {
            self.run_pseudo_trap(PseudoTrap::Return);
        }
        match result {
            ExecResult::Return(code) => ExecResult::Status(code),
            other => other,
        }
    }

    fn run_if(&mut self, branches: &[(Program, Program)], else_branch: &Option<Program>) -> ExecResult {
        for (cond, body) in branches {
            self.suppress_errexit += 1;
            let cond_result = self.run_program(cond);
            self.suppress_errexit -= 1;
            if cond_result.is_signal() {
                return cond_result;
            }
            if cond_result.status() == 0 {
                return self.run_program(body);
            }
        }
        if let Some(else_body) = else_branch {
            return self.run_program(else_body);
        }
        ExecResult::Status(0)
    }

    fn run_while(&mut self, cond: &Program, body: &Program, until: bool) -> ExecResult {
        let mut ran_body = false;
        // Tracked separately from self.last_status: evaluating `cond` runs
        // a real command through run_program, which unconditionally
        // overwrites self.last_status as a side effect (needed so `$?`
        // reads right *during* that command) -- including on the final,
        // loop-ending check. Without this, a loop's reported exit status
        // would reflect that failing condition check instead of the body's
        // last status, which is what bash actually reports.
        let mut last_body_status = 0;
        loop {
            self.suppress_errexit += 1;
            let cond_result = self.run_program(cond);
            self.suppress_errexit -= 1;
            if cond_result.is_signal() {
                return cond_result;
            }
            let keep_going = if until { cond_result.status() != 0 } else { cond_result.status() == 0 };
            if !keep_going {
                break;
            }
            ran_body = true;
            match self.run_program(body) {
                ExecResult::Break(n) => {
                    if n > 1 {
                        return ExecResult::Break(n - 1);
                    }
                    break;
                }
                ExecResult::Continue(n) => {
                    if n > 1 {
                        return ExecResult::Continue(n - 1);
                    }
                    continue;
                }
                ExecResult::Status(s) => {
                    self.last_status = s;
                    last_body_status = s;
                }
                ret @ (ExecResult::Return(_) | ExecResult::Window(_) | ExecResult::Fg | ExecResult::Edit | ExecResult::Exit(_)) => return ret,
            }
        }
        if ran_body {
            self.last_status = last_body_status;
            ExecResult::Status(last_body_status)
        } else {
            ExecResult::Status(0)
        }
    }

    fn run_for(&mut self, var: &str, words: Option<&[Word]>, body: &Program) -> ExecResult {
        let values = match words {
            Some(words) => self.expand_words(words),
            None => self.arg_frames.last().cloned().unwrap_or_default(),
        };
        let mut ran_body = false;
        for val in values {
            ran_body = true;
            self.assign_var(var, val);
            match self.run_program(body) {
                ExecResult::Break(n) => {
                    if n > 1 {
                        return ExecResult::Break(n - 1);
                    }
                    break;
                }
                ExecResult::Continue(n) => {
                    if n > 1 {
                        return ExecResult::Continue(n - 1);
                    }
                    continue;
                }
                ExecResult::Status(s) => self.last_status = s,
                ret @ (ExecResult::Return(_) | ExecResult::Window(_) | ExecResult::Fg | ExecResult::Edit | ExecResult::Exit(_)) => return ret,
            }
        }
        if ran_body { ExecResult::Status(self.last_status) } else { ExecResult::Status(0) }
    }

    // `select var [in words]; do body; done`. Displays a numbered menu to
    // stderr (never stdout, matching real bash) and reads a choice from
    // stdin, looping until `break` or EOF. Real bash's exact behavior,
    // confirmed empirically:
    //  - the menu is (re)printed before the very first prompt, and again
    //    after any BLANK line of input, but not otherwise
    //  - a blank input line reprompts without running the body at all
    //  - a non-blank but out-of-range/non-numeric choice still runs the
    //    body, with `var` set to empty (REPLY still gets the raw text)
    //  - EOF on stdin ends the loop entirely (like an implicit break),
    //    without running the body, and the loop's status is 1
    fn run_select(&mut self, var: &str, words: Option<&[Word]>, body: &Program) -> ExecResult {
        let items: Vec<String> = match words {
            Some(words) => self.expand_words(words),
            None => self.arg_frames.last().cloned().unwrap_or_default(),
        };
        let print_menu = |this: &Self, items: &[String]| {
            for (i, item) in items.iter().enumerate() {
                sh_eprintln!(this, "{}) {}", i + 1, item);
            }
        };
        // Nothing to choose from is not a prompt with no options: bash
        // does not print the menu, does not prompt, and leaves the loop
        // at once with status 0.
        if items.is_empty() {
            return ExecResult::Status(0);
        }
        print_menu(self, &items);
        loop {
            let ps3 = {
                let v = self.lookup_var("PS3");
                if v.is_empty() { "#? ".to_string() } else { v }
            };
            sh_eprint!(self, "{}", ps3);
            let _ = std::io::Write::flush(&mut std::io::stderr());
            let mut line = String::new();
            match std::io::BufRead::read_line(&mut self.current_stdin_reader(), &mut line) {
                // Confirmed against real bash: on EOF, `select` prints a
                // bare newline to *stdout* (not stderr, where the menu/
                // prompt otherwise live) before giving up -- reproducible
                // even non-interactively, not just an interactive-terminal
                // courtesy newline.
                Ok(0) => {
                    sh_println!(self);
                    return ExecResult::Status(1);
                }
                Ok(_) => {}
                Err(_) => return ExecResult::Status(1),
            }
            let line = line.trim_end_matches(['\n', '\r']).to_string();
            self.assign_var("REPLY", line.clone());
            if line.trim().is_empty() {
                print_menu(self, &items);
                continue;
            }
            let choice = line.trim().parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|i| items.get(i));
            self.assign_var(var, choice.cloned().unwrap_or_default());
            match self.run_program(body) {
                ExecResult::Break(n) => {
                    if n > 1 {
                        return ExecResult::Break(n - 1);
                    }
                    return ExecResult::Status(self.last_status);
                }
                ExecResult::Continue(n) => {
                    if n > 1 {
                        return ExecResult::Continue(n - 1);
                    }
                    continue;
                }
                ExecResult::Status(s) => self.last_status = s,
                ret @ (ExecResult::Return(_) | ExecResult::Window(_) | ExecResult::Fg | ExecResult::Edit | ExecResult::Exit(_)) => return ret,
            }
        }
    }

    // `for ((init; cond; step)); do body; done`. Unlike run_for/run_while,
    // `continue` must still run `step` before re-checking `cond` -- so the
    // Continue arm falls through to the step evaluation below instead of
    // looping straight back like a plain `while`'s `continue` does.
    fn run_cfor(&mut self, init: &str, cond: &str, step: &str, body: &Program) -> ExecResult {
        if !init.is_empty() {
            if let Err(e) = arith::eval(init, self) {
                sh_eprintln!(self, "bish: (({})): {}", init, e);
                return ExecResult::Status(1);
            }
        }
        let mut ran_body = false;
        loop {
            let keep_going = if cond.is_empty() {
                true
            } else {
                match arith::eval(cond, self) {
                    Ok(v) => v != 0,
                    Err(e) => {
                        sh_eprintln!(self, "bish: (({})): {}", cond, e);
                        return ExecResult::Status(1);
                    }
                }
            };
            if !keep_going {
                break;
            }
            ran_body = true;
            match self.run_program(body) {
                ExecResult::Break(n) => {
                    if n > 1 {
                        return ExecResult::Break(n - 1);
                    }
                    break;
                }
                ExecResult::Continue(n) => {
                    if n > 1 {
                        return ExecResult::Continue(n - 1);
                    }
                }
                ExecResult::Status(s) => self.last_status = s,
                ret @ (ExecResult::Return(_) | ExecResult::Window(_) | ExecResult::Fg | ExecResult::Edit | ExecResult::Exit(_)) => return ret,
            }
            if !step.is_empty() {
                if let Err(e) = arith::eval(step, self) {
                    sh_eprintln!(self, "bish: (({})): {}", step, e);
                    return ExecResult::Status(1);
                }
            }
        }
        if ran_body { ExecResult::Status(self.last_status) } else { ExecResult::Status(0) }
    }

    fn run_case(&mut self, word: &Word, arms: &[(Vec<Word>, Program, parser::CaseTerm)]) -> ExecResult {
        let val = self.expand_word(word);
        let mut i = 0;
        // Set by `;&`: run the next arm's body unconditionally, skipping
        // its pattern test entirely (unlike `;;&`, which just resumes
        // normal pattern testing at the next arm without forcing a run).
        let mut force_run = false;
        let mut last_body_status = 0;
        while i < arms.len() {
            let (patterns, body, term) = &arms[i];
            // `shopt -s nocasematch` covers `case` as well as
            // `[[ =~ ]]` -- it was only wired into the latter.
            let fold_case = self.shopt_is_on("nocasematch");
            let should_run = force_run
                || patterns.iter().any(|p| {
                    // A *quoted* part of a pattern is literal text, not
                    // a pattern: `case abc in "$p")` with `p='*'` looks
                    // for a literal asterisk, and so does `'*'`. The
                    // whole pattern was expanded as plain text and then
                    // matched as a glob, so both matched everything --
                    // the same mistake `[[ =~ ]]` avoids by building
                    // its operand chunk by chunk, which is what this
                    // does now.
                    let pat = self.expand_glob_pattern_operand(p);
                    glob::matches_with_case(&pat, &val, fold_case)
                });
            if should_run {
                match self.run_program(body) {
                    ExecResult::Status(s) => {
                        self.last_status = s;
                        last_body_status = s;
                    }
                    other => return other,
                }
                match term {
                    parser::CaseTerm::Stop => return ExecResult::Status(last_body_status),
                    parser::CaseTerm::FallThrough => force_run = true,
                    parser::CaseTerm::Continue => force_run = false,
                }
            } else {
                force_run = false;
            }
            i += 1;
        }
        ExecResult::Status(last_body_status)
    }

    pub(crate) fn eval_test_or(&mut self, atoms: &[parser::TestAtom], pos: &mut usize) -> Result<bool, String> {
        let mut result = self.eval_test_and(atoms, pos)?;
        while matches!(atoms.get(*pos), Some(parser::TestAtom::Or)) {
            *pos += 1;
            let rhs = self.eval_test_and(atoms, pos)?;
            result = result || rhs;
        }
        Ok(result)
    }

    fn eval_test_and(&mut self, atoms: &[parser::TestAtom], pos: &mut usize) -> Result<bool, String> {
        let mut result = self.eval_test_unary(atoms, pos)?;
        while matches!(atoms.get(*pos), Some(parser::TestAtom::And)) {
            *pos += 1;
            let rhs = self.eval_test_unary(atoms, pos)?;
            result = result && rhs;
        }
        Ok(result)
    }

    fn eval_test_unary(&mut self, atoms: &[parser::TestAtom], pos: &mut usize) -> Result<bool, String> {
        if matches!(atoms.get(*pos), Some(parser::TestAtom::Not)) {
            *pos += 1;
            return Ok(!self.eval_test_unary(atoms, pos)?);
        }
        self.eval_test_primary(atoms, pos)
    }

    fn eval_test_primary(&mut self, atoms: &[parser::TestAtom], pos: &mut usize) -> Result<bool, String> {
        match atoms.get(*pos) {
            Some(parser::TestAtom::Group(inner)) => {
                *pos += 1;
                let mut ipos = 0;
                self.eval_test_or(inner, &mut ipos)
            }
            Some(parser::TestAtom::Word(_)) => {
                let mut word_atoms: Vec<&Word> = Vec::new();
                while let Some(parser::TestAtom::Word(w)) = atoms.get(*pos) {
                    word_atoms.push(w);
                    *pos += 1;
                }
                // `[[ a == b == c ]]` -- no form of `[[ ]]` takes four
                // operands, and reading it as "non-empty, so true" is
                // how a stray word passed silently.
                if word_atoms.len() > 3 {
                    let text = word_atoms.iter().map(|w| self.expand_word(w)).collect::<Vec<_>>();
                    return Err(format!("syntax error in conditional expression: unexpected token `{}'", text[3]));
                }
                Ok(self.eval_simple_test(&word_atoms))
            }
            other => Err(format!("syntax error near {:?}", other)),
        }
    }

    // `[[ -v NAME ]]`, including `[[ -v arr[0] ]]`: is this name --
    // scalar, array, or one element of one -- actually set, as opposed
    // to reading back empty because it never was.
    // What `[ -v NAME ]` and `[ -o OPTNAME ]` need answering, for the
    // operands that actually appear -- see builtins::ShellFacts.
    fn shell_test_answers(&mut self, args: &[String]) -> (HashMap<String, bool>, HashMap<String, bool>) {
        let mut vars = HashMap::new();
        let mut opts = HashMap::new();
        for pair in args.windows(2) {
            let (op, operand) = (pair[0].as_str(), pair[1].clone());
            match op {
                "-v" => {
                    let answer = self.name_is_set(&operand);
                    vars.insert(operand, answer);
                }
                "-o" => {
                    let answer = self.shell_option_enabled(&operand).unwrap_or(false);
                    opts.insert(operand, answer);
                }
                _ => {}
            }
        }
        (vars, opts)
    }

    /// `${!name}`: the variable *named by* `name`'s value.
    ///
    /// bash refuses this three ways, each of them fatal the way
    /// `${x:?}` is -- `name` unset, its value empty, and its value not
    /// a parameter name. Expanding to nothing instead meant a typo in
    /// the indirection quietly became an empty string, which is the
    /// one thing an indirection cannot afford.
    ///
    /// The value may name an array element (`x=a[1]`), a positional
    /// (`x=1`) or a special (`x=@`); bash follows all of them, and the
    /// element form was reaching `lookup_var("a[1]")` and finding
    /// nothing.
    fn indirect_var(&mut self, name: &str) -> String {
        // A nameref is the one case where `${!x}` is not an
        // indirection at all: it gives the *name* the reference points
        // at, following a chain of them to the end. `$r` is already the
        // target's value, so reading `${!r}` as "indirect through that
        // value" made it a second hop -- which lands on whatever the
        // value happens to spell, and usually on nothing.
        if self.nameref_names.contains(name) {
            return self.resolve_nameref(name);
        }
        if !self.var_is_set(name) {
            sh_eprintln!(self, "bish: {}: invalid indirect expansion", name);
            self.expansion_failed = true;
            return String::new();
        }
        let target = self.lookup_var(name);
        match split_subscript(&target) {
            Some((base, index)) if is_parameter_name(base) => {
                let base = base.to_string();
                self.array_element(&base, &index)
            }
            None if is_parameter_name(&target) => self.lookup_var(&target),
            _ => {
                sh_eprintln!(self, "bish: {}: invalid variable name", target);
                self.expansion_failed = true;
                String::new()
            }
        }
    }

    fn name_is_set(&mut self, name: &str) -> bool {
        match name.split_once('[').and_then(|(base, rest)| rest.strip_suffix(']').map(|index| (base, index))) {
            Some((base, index)) => self.array_element_is_set(base, index),
            None => self.var_is_set(name) || !self.array_all(name).is_empty(),
        }
    }

    fn eval_simple_test(&mut self, words: &[&Word]) -> bool {
        match words {
            [] => false,
            [s] => !self.expand_word(s).is_empty(),
            [op, a] => {
                let op = self.expand_word(op);
                let a = self.expand_word(a);
                // `-v` asks about a *name*, where every other unary
                // test asks about the text one expands to -- so it
                // cannot go through `builtins::unary` with the rest,
                // which has no shell to ask.
                match op.as_str() {
                    "-v" => self.name_is_set(&a),
                    // Same reason as `-v`: it asks the shell, not the
                    // filesystem. An option this shell does not gate
                    // anything on is not on.
                    "-o" => self.shell_option_enabled(&a).unwrap_or(false),
                    _ => builtins::unary(&op, &a),
                }
            }
            [a, op, b] => {
                let op = self.expand_word(op);
                let a = self.expand_word(a);
                if op == "=~" {
                    let pattern = self.expand_regex_operand(b);
                    // `shopt -s nocasematch` -- registered since the
                    // shopt namespace was, and until regex.rs could fold
                    // case at all there was nothing for it to do.
                    match crate::regex::match_captures(&a, &pattern, self.shopt_is_on("nocasematch")) {
                        Some(groups) => {
                            let map: std::collections::BTreeMap<usize, String> = groups.into_iter().enumerate().collect();
                            self.arrays.insert("BASH_REMATCH".to_string(), map);
                            true
                        }
                        None => {
                            self.arrays.remove("BASH_REMATCH");
                            false
                        }
                    }
                } else {
                    // `==`/`!=`/`=` take a *pattern* on the right, so
                    // the quoted parts of it have to be escaped; every
                    // other operator compares plain text.
                    let b = match op.as_str() {
                        "=" | "==" | "!=" => self.expand_glob_pattern_operand(b),
                        _ => self.expand_word(b),
                    };
                    let fold_case = self.shopt_is_on("nocasematch");
                    builtins::binary_with_case(&a, &op, &b, true, fold_case)
                }
            }
            // Four or more is rejected by the caller before reaching
            // here; this arm is unreachable in practice.
            _ => !words.is_empty(),
        }
    }

    // The RHS of `[[ str =~ pattern ]]`. Quoting (or backslash-escaping)
    // any part of `pattern` in the source forces that part to match
    // literally instead of as regex syntax -- real bash's documented
    // behavior for `=~`. Mirrors expand_word's chunk loop, but escapes
    // each chunk's own text for the regex engine when that chunk is
    // individually quoted (Chunk::LiteralStr, or any other chunk's own
    // `quoted` flag), leaving unquoted chunks as raw regex syntax -- so
    // e.g. `^[0-9]+\.[0-9]+$` keeps `^`/`[0-9]`/`+`/`$` as regex metachars
    // while the backslash-escaped `.` matches only a literal dot.
    fn expand_regex_operand(&mut self, w: &Word) -> String {
        let mut out = String::new();
        for c in &w.chunks {
            match c {
                Chunk::Tilde { name } => {
                    let name = name.clone();
                    out.push_str(&crate::regex::escape(&self.expand_tilde(&name)));
                }
                Chunk::Str(t) => out.push_str(t),
                Chunk::LiteralStr(t) => out.push_str(&crate::regex::escape(t)),
                Chunk::Var { name, quoted } => {
                    let name = name.clone();
                    self.check_nounset(&name);
                    let v = self.lookup_var(&name);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::Sub { raw, quoted } => {
                    let v = self.run_command_substitution(raw);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::Arith { raw, quoted } => match self.eval_arith(raw) {
                    Ok(v) => {
                        let v = v.to_string();
                        out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                    }
                    Err(e) => {
                        sh_eprintln!(self, "bish: (({})): {}", raw, e);
                        // Fatal, unlike the `(( ))` *command* -- see
                        // expansion_failed.
                        self.expansion_failed = true;
                    }
                },
                Chunk::VarExpand { name, op, quoted } => {
                    let name = name.clone();
                    let op = op.clone();
                    let v = match self.list_slice(&name, None, &op) {
                        Some(sliced) => self.joined_slice(sliced),
                        None => self.eval_var_op(&name, &op),
                    };
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::ArrayVar { name, index, quoted } => {
                    let name = name.clone();
                    let index = index.clone();
                    let v = self.array_element(&name, &index);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::ArrayLength { name, index } => {
                    let name = name.clone();
                    let index = index.clone();
                    out.push_str(&self.array_length(&name, &index).to_string());
                }
                Chunk::ArrayVarExpand { name, index, op, quoted } => {
                    let name = name.clone();
                    let index = index.clone();
                    let op = op.clone();
                    let v = match self.list_slice(&name, Some(&index), &op) {
                        Some(sliced) => self.joined_slice(sliced),
                        None => self.eval_array_var_op(&name, &index, &op),
                    };
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::Indirect { name, quoted } => {
                    let v = self.indirect_var(name);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::ArrayKeys { name, quoted } => {
                    let name = name.clone();
                    let sep = self.ifs_join_char();
                    let v = self.array_keys(&name).join(&sep);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::VarNamesMatchingPrefix { prefix, quoted, .. } => {
                    let prefix = prefix.clone();
                    let sep = self.ifs_join_char();
                    let v = self.var_names_with_prefix(&prefix).join(&sep);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::ProcSubIn { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_in(&raw);
                    out.push_str(&crate::regex::escape(&v));
                }
                Chunk::ProcSubOut { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_out(&raw);
                    out.push_str(&crate::regex::escape(&v));
                }
            }
        }
        out
    }

    // The same, for `[[ x == PATTERN ]]`. Quoting any part of the
    // right-hand side makes that part a literal instead of pattern
    // syntax -- `[[ abc == "a*" ]]` is false, and `[[ abc == a* ]]` is
    // true. Expanding it as one plain word lost the distinction, so a
    // quoted `*` still globbed.
    fn expand_glob_pattern_operand(&mut self, w: &Word) -> String {
        let mut out = String::new();
        for c in &w.chunks {
            match c {
                Chunk::Tilde { name } => {
                    let name = name.clone();
                    out.push_str(&crate::glob::escape(&self.expand_tilde(&name)));
                }
                Chunk::Str(t) => out.push_str(t),
                Chunk::LiteralStr(t) => out.push_str(&crate::glob::escape(t)),
                Chunk::Var { name, quoted } => {
                    let name = name.clone();
                    self.check_nounset(&name);
                    let v = self.lookup_var(&name);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v });
                }
                Chunk::Sub { raw, quoted } => {
                    let v = self.run_command_substitution(raw);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v });
                }
                Chunk::Arith { raw, quoted } => match self.eval_arith(raw) {
                    Ok(v) => {
                        let v = v.to_string();
                        out.push_str(&if *quoted { crate::glob::escape(&v) } else { v });
                    }
                    Err(e) => {
                        sh_eprintln!(self, "bish: (({})): {}", raw, e);
                        // Fatal, unlike the `(( ))` *command* -- see
                        // expansion_failed.
                        self.expansion_failed = true;
                    }
                },
                Chunk::VarExpand { name, op, quoted } => {
                    let name = name.clone();
                    let op = op.clone();
                    let v = match self.list_slice(&name, None, &op) {
                        Some(sliced) => self.joined_slice(sliced),
                        None => self.eval_var_op(&name, &op),
                    };
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v });
                }
                Chunk::ArrayVar { name, index, quoted } => {
                    let name = name.clone();
                    let index = index.clone();
                    let v = self.array_element(&name, &index);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v });
                }
                Chunk::ArrayLength { name, index } => {
                    let name = name.clone();
                    let index = index.clone();
                    out.push_str(&self.array_length(&name, &index).to_string());
                }
                Chunk::ArrayVarExpand { name, index, op, quoted } => {
                    let name = name.clone();
                    let index = index.clone();
                    let op = op.clone();
                    let v = match self.list_slice(&name, Some(&index), &op) {
                        Some(sliced) => self.joined_slice(sliced),
                        None => self.eval_array_var_op(&name, &index, &op),
                    };
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v });
                }
                Chunk::Indirect { name, quoted } => {
                    let v = self.indirect_var(name);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v });
                }
                Chunk::ArrayKeys { name, quoted } => {
                    let name = name.clone();
                    let sep = self.ifs_join_char();
                    let v = self.array_keys(&name).join(&sep);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v });
                }
                Chunk::VarNamesMatchingPrefix { prefix, quoted, .. } => {
                    let prefix = prefix.clone();
                    let sep = self.ifs_join_char();
                    let v = self.var_names_with_prefix(&prefix).join(&sep);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v });
                }
                Chunk::ProcSubIn { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_in(&raw);
                    out.push_str(&crate::glob::escape(&v));
                }
                Chunk::ProcSubOut { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_out(&raw);
                    out.push_str(&crate::glob::escape(&v));
                }
            }
        }
        out
    }

    fn run_single(&mut self, cmd: &SimpleCommand, background: bool) -> ExecResult {
        if cmd.words.is_empty() {
            // An assignment-only command is a command, and bash names
            // it in `$BASH_COMMAND` like any other.
            // Serialised, not expanded. The value has not been
            // evaluated yet -- the DEBUG trap fires *before* the
            // assignment -- and expanding it here to name it would run
            // any `$( )` in it a second time.
            let named: Vec<String> = cmd
                .assigns
                .iter()
                .map(|(name, mode, val)| {
                    let op = if *mode == AssignMode::Append { "+=" } else { "=" };
                    format!("{}{}{}", name, op, crate::serialize::serialize_word(val))
                })
                .collect();
            self.bash_command = named.join(" ");
            // DEBUG, before the command rather than after: the whole
            // use of it is to see what is about to run.
            self.run_pseudo_trap(PseudoTrap::Debug);
            // An assignment-only command is still a command, so `&`
            // makes it a job -- one whose whole effect is discarded
            // with the child, which is exactly what bash does with
            // `x=1 &`. Left running here it set the variable in this
            // shell and registered nothing.
            if background && cmd.array_assigns.is_empty() {
                let mut script = String::new();
                for (name, mode, val) in &cmd.assigns {
                    let v = self.expand_word(val);
                    let op = if *mode == AssignMode::Append { "+=" } else { "=" };
                    script.push_str(&format!("{}{}{} ", name, op, crate::serialize::quote_literal(&v)));
                }
                let label = script.trim_end().to_string();
                return ExecResult::Status(self.spawn_background_script(&script, label));
            }
            // A refused write (readonly name) is the whole command's
            // failure, not a silent no-op -- `set -e` and an explicit
            // `|| exit` both depend on seeing it.
            let mut ok = true;
            // Cleared before the expansions, so what is left afterwards
            // is this command's own -- see last_subst_status.
            self.last_subst_status = None;
            // A bare assignment is a command, and bash traces it:
            // `set -x; x=1` prints `+ x=1`, with the *expanded* value
            // and quotes only where they are needed.
            let mut traced: Vec<String> = Vec::new();
            // An assignment-only command is an effect too.
            self.effects += 1;
            for (name, mode, val) in &cmd.assigns {
                let v = self.expand_word(val);
                if self.opt_xtrace {
                    let op = if *mode == AssignMode::Append { "+=" } else { "=" };
                    traced.push(format!("{}{}{}", name, op, xtrace_quote(&v)));
                }
                ok &= match mode {
                    AssignMode::Set => self.assign_var(name, v),
                    AssignMode::Append => {
                        let cur = self.appended_value(name, &v);
                        self.assign_var(name, cur)
                    }
                };
            }
            for (name, mode, items) in &cmd.array_assigns {
                if self.opt_xtrace {
                    traced.push(self.array_literal_display(name, *mode, items));
                }
                ok &= self.apply_array_literal(name, *mode, items);
            }
            for (name, index, mode, val) in &cmd.index_assigns {
                // `a[]=1` names no element at all. bash calls that a
                // bad subscript and stops; accepting it wrote to index
                // 0, which is not what anyone typing it meant.
                if index.trim().is_empty() {
                    sh_eprintln!(self, "bish: {}[]: bad array subscript", name);
                    ok = false;
                    continue;
                }
                // array_set_index prints the refusal itself; this only
                // needs to know that one is coming.
                ok &= !self.name_is_readonly(name);
                let v = self.expand_word(val);
                if self.opt_xtrace {
                    let op = if *mode == AssignMode::Append { "+=" } else { "=" };
                    traced.push(format!("{}[{}]{}{}", name, index, op, xtrace_quote(&v)));
                }
                let v = match mode {
                    AssignMode::Set => v,
                    // `m[k]+=v` appends to whatever the element holds,
                    // the same way `x+=v` does for a scalar.
                    AssignMode::Append => self.array_element(name, index) + &v,
                };
                self.array_set_index(name, index, v);
            }
            if !traced.is_empty() {
                let ps4 = self.xtrace_prefix();
                sh_eprintln!(self, "{}{}", ps4, traced.join(" "));
            }
            if !cmd.redirects.is_empty() {
                // side effect only: create/truncate/append the target files
                let _ = self.resolve_redirects(cmd);
            }
            // bash treats a refused assignment as *fatal* in a
            // non-interactive shell -- the script stops, which is the
            // only thing that makes `readonly` worth writing. It does
            // not do that to a session someone is typing at, and
            // neither does this.
            if !ok && !self.interactive {
                self.pending_exit = Some(1);
            }
            if let Some(result) = self.take_expansion_failure() {
                return result;
            }
            if let Some(exit) = self.take_pending_exit() {
                return exit;
            }
            if !ok {
                return ExecResult::Status(1);
            }
            return ExecResult::Status(self.last_subst_status.take().unwrap_or(0));
        }

        let saved_stderr_target = self.current_stderr_target.take();
        self.current_stderr_target = self.peek_stderr_target(&cmd.redirects);
        let first_word_literal = match cmd.words[0].chunks.as_slice() {
            // Quoting a builtin's own name doesn't stop bash from
            // recognizing it (`"export" FOO=bar` still runs export), so
            // this must match a fully-quoted name too, not just a bare one.
            [Chunk::Str(s)] | [Chunk::LiteralStr(s)] => Some(s.as_str()),
            _ => None,
        };
        // Only populated for the same assignment-builtin names as the argv
        // branch just below (an ordinary command's cmd.array_word_assigns
        // is always empty -- see parser.rs's is_declare_family_command).
        // Position `p` here means "the array literal that would sit at
        // argv[p]" -- run_declare/run_local's own arg loops splice it back
        // in at that index instead of re-parsing a plain string there.
        let mut array_literal_args: Vec<(usize, String, AssignMode, Vec<ArrayLiteralItem>)> = Vec::new();
        let argv: Vec<String> = if matches!(first_word_literal, Some("local") | Some("export") | Some("declare") | Some("typeset") | Some("readonly"))
        {
            // Assignment-builtins: `NAME=value` arguments must not be
            // word-split on the expanded value (bash treats them like any
            // other assignment), unlike a normal builtin's arguments.
            let mut v = vec![first_word_literal.unwrap().to_string()];
            let mut pending = cmd.array_word_assigns.iter().peekable();
            for (i, w) in cmd.words[1..].iter().enumerate() {
                let word_index = i + 1;
                while let Some((pos, name, mode, items)) = pending.peek() {
                    if *pos != word_index {
                        break;
                    }
                    array_literal_args.push((v.len(), name.clone(), *mode, items.clone()));
                    v.push(self.array_literal_display(name, *mode, items));
                    pending.next();
                }
                if let Some((name, _mode, val_word)) = parser::word_as_assignment(w) {
                    v.push(format!("{}={}", name, self.expand_word(&val_word)));
                } else {
                    v.push(self.expand_word(w));
                }
            }
            for (_, name, mode, items) in pending {
                array_literal_args.push((v.len(), name.clone(), *mode, items.clone()));
                v.push(self.array_literal_display(name, *mode, items));
            }
            v
        } else {
            self.expand_words(&cmd.words)
        };
        self.current_stderr_target = saved_stderr_target;
        if let Some(result) = self.take_expansion_failure() {
            return result;
        }
        if let Some(exit) = self.take_pending_exit() {
            return exit;
        }
        // Named before it runs, so a DEBUG trap sees the command it is
        // firing for and an ERR trap sees the one that failed.
        self.bash_command = argv.join(" ");
        self.run_pseudo_trap(PseudoTrap::Debug);
        if argv.is_empty() {
            // Every word vanished (e.g. the command was just an unquoted
            // empty/unset variable) -- matches bash: nothing runs.
            return ExecResult::Status(0);
        }
        if self.opt_xtrace {
            let ps4 = self.xtrace_prefix();
            // Each word quoted if it needs it, the same way an
            // assignment's value already was: the trace is meant to be
            // readable *as* the command that ran, and unquoted it is
            // not. `echo "a b"` traced as `+ echo a b`, which reads as
            // two arguments, and `[ x = x ]` as `+ [ x = x ]`, where
            // bash writes `+ '[' x = x ']'`.
            let words: Vec<String> = argv.iter().map(|w| xtrace_quote_word(w)).collect();
            sh_eprintln!(self, "{}{}", ps4, words.join(" "));
        }
        let name = argv[0].clone();

        // `&` on a builtin or a function is a job, which means a child.
        // Only an external took any notice of it here: everything else
        // ran in this shell, synchronously, registering no job -- so
        // `echo a &` left `$!` unset and `wait` with nothing to collect,
        // and `f() { exit 5; }; f &` took the whole shell down with it.
        //
        // The child is handed the words this shell has *already
        // expanded*, quoted, rather than the source text: the
        // expansions have happened, and a `$( )` among them must not
        // run a second time. Its redirects go as written, since nothing
        // has expanded those yet.
        //
        // A `declare`-family array literal is the exception. Its display
        // text is not the source it was parsed from, so there is
        // nothing honest to hand a child -- and backgrounding a
        // declaration has no observable effect anyway.
        let runs_in_this_shell = self.is_active_builtin(&name) || (!self.restrict_to_builtins && self.functions.contains_key(&name));
        if background && runs_in_this_shell && cmd.array_assigns.is_empty() && cmd.array_word_assigns.is_empty() {
            let mut script = String::new();
            for (n, mode, val) in &cmd.assigns {
                let v = self.expand_word(val);
                let op = if *mode == AssignMode::Append { "+=" } else { "=" };
                script.push_str(&format!("{}{}{} ", n, op, crate::serialize::quote_literal(&v)));
            }
            script.push_str(&argv.iter().map(|w| crate::serialize::quote_literal(w)).collect::<Vec<_>>().join(" "));
            for r in &cmd.redirects {
                script.push(' ');
                script.push_str(&crate::serialize::serialize_redirect(r));
            }
            let label = script.clone();
            return ExecResult::Status(self.spawn_background_script(&script, label));
        }

        // Functions shadow builtins, matching real bash (confirmed: even
        // POSIX "special" builtins like `export`/`return`/`break` are
        // overridable by a same-named function there) -- `builtin NAME`
        // (its own arm inside dispatch_builtin_or_external below) is the
        // explicit bypass. `restrict_to_builtins` (command mode's
        // colon-line) skips this entirely: its own contract is "only
        // real builtins run here", not "functions still apply".
        if !self.restrict_to_builtins
            && let Some(body) = self.functions.get(&name).cloned()
        {
            // A redirect on the *call* applies to the whole body:
            // `f > out` sends everything the function prints to the
            // file, builtins and external commands alike. It used to be
            // dropped entirely -- the file was never even created.
            if cmd.redirects.is_empty() {
                return self.call_function(&name, &body, argv[1..].to_vec());
            }
            if !Self::compound_redirects_are_simple(&cmd.redirects) {
                // A numbered-fd redirect on a function call has no
                // in-process model (see run_compound_redirected); run
                // the body the way a redirected compound command is
                // run, which does.
                let group = parser::Command::Group(
                    vec![ListItem {
                        and_or: AndOr { first: Pipeline { commands: vec![body.clone()], negate: false, timed: None }, rest: Vec::new() },
                        sep: Sep::Seq,
                        line: self.current_line,
                    }],
                    Vec::new(),
                );
                self.arg_frames.push(argv[1..].to_vec());
                let result = self.run_compound_redirected(&group, &cmd.redirects, background);
                self.arg_frames.pop();
                return result;
            }
            let stdio = match self.resolve_simple_redirects_for_compound(&cmd.redirects) {
                Ok(stdio) => stdio,
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    return ExecResult::Status(1);
                }
            };
            let saved = self.install_redirected_stdio(stdio);
            let result = self.call_function(&name, &body, argv[1..].to_vec());
            self.restore_redirected_stdio(saved);
            return result;
        }
        // A prefix assignment (`IFS=: read a b`) is in scope for the
        // command it prefixes and gone afterwards. The external path
        // already handled this by passing them as environment -- see
        // run_external's own `command.env` loop -- but a builtin reads
        // the *shell's* variables, so nothing reached it: `IFS=: read`
        // split on the old IFS and `HISTTIMEFORMAT=x history` printed no
        // times.
        //
        // Applied here, around the dispatch, rather than inside it: this
        // is the one place that knows the command is about to run and
        // that it is not a function call (bash lets an assignment
        // outlive a function, which is its own rule and not this one).
        // Every builtin and every external command is an effect. A shell
        // *function* call is not -- it returned above, before this point.
        self.effects += 1;
        let restore = self.apply_prefix_assigns(cmd);
        let result = self.dispatch_builtin_or_external(&argv, name, cmd, background, false, &array_literal_args);
        self.restore_prefix_assigns(restore);
        // Every simple command returns through here, which is the
        // point: the drains inside the dispatch above are all on the
        // *external* paths, so a `>( )` used by a builtin was queued and
        // never run at all. `echo hi > >(cat)` produced nothing while
        // `/bin/echo hi > >(cat)` worked. Cheap when nothing is queued,
        // and a no-op when one of those inner drains has already run.
        self.drain_proc_subs();
        result
    }

    // Applies `cmd`'s prefix assignments to the shell's own variables,
    // handing back what was there before so it can be put back.
    //
    // `None` for a name that was unset, which has to be restored as
    // *unset* rather than as empty -- `FOO=x cmd` must not leave `FOO`
    // behind set to nothing, which `${FOO-default}` and `-u` both
    // notice.
    fn apply_prefix_assigns(&mut self, cmd: &SimpleCommand) -> Vec<(String, Option<String>)> {
        let mut saved = Vec::new();
        for (name, mode, val) in &cmd.assigns {
            let v = self.expand_word(val);
            let v = match mode {
                AssignMode::Set => v,
                AssignMode::Append => self.appended_value(name, &v),
            };
            // Only what was actually written needs putting back -- a
            // refused write (readonly name) left nothing to restore,
            // and restoring it would print the same refusal twice.
            let previous = self.var_is_set(name).then(|| self.lookup_var(name));
            if self.assign_var(name, v) {
                saved.push((name.clone(), previous));
            }
        }
        saved
    }

    fn restore_prefix_assigns(&mut self, saved: Vec<(String, Option<String>)>) {
        for (name, previous) in saved.into_iter().rev() {
            match previous {
                Some(v) => {
                    self.assign_var(&name, v);
                }
                // Restored as *unset*, the same way `unset` does it:
                // walk the local scopes first, then the real
                // environment.
                None => self.remove_var(&name),
            }
        }
    }

    // Thin wrapper around dispatch_builtin_or_external_impl (the actual
    // dozens-of-arms match, below) that installs a per-command output-
    // redirect override around the whole call -- see push_builtin_output_
    // sink's own doc comment for exactly what it covers and why a single
    // push/pop pair here (rather than threading a restore through every
    // one of that match's many `return` points) is enough: every path
    // through it either returns straight from the match (builtin output)
    // or falls through into the external-spawn tail, whose own child
    // process writes straight to real fds regardless of self.sink -- only
    // the shell's *own* diagnostics along that tail (e.g. a failed spawn)
    // are affected, which is exactly the intended, correct behavior.
    fn dispatch_builtin_or_external(
        &mut self,
        argv: &[String],
        name: String,
        cmd: &SimpleCommand,
        background: bool,
        builtin_only: bool,
        array_literal_args: &[(usize, String, AssignMode, Vec<ArrayLiteralItem>)],
    ) -> ExecResult {
        // Only pop if this call's own push actually installed a new layer
        // (`Ok(true)`) -- `self.sink` can legitimately already be
        // `OutputSink::Builtin` when this command starts (e.g. this
        // command runs inside a converted foreground subshell/command-
        // substitution/proc-sub, whose own capture is exactly a Builtin
        // sink left in place for its *whole* run, not just one command --
        // see run_in_child_shell). Popping unconditionally would strip
        // that unrelated, still-needed layer the moment a command with no
        // redirect of its own (push returning `Ok(false)`, doing nothing)
        // ran inside it.
        // Only for a command that will run *in* this shell. An
        // external's redirects are applied to the spawned process by
        // `resolve_redirects` further down, and expanding them here as
        // well expanded every redirect target twice: `/bin/echo x >
        // $(echo side >&2; echo f)` ran that substitution twice, so the
        // file it created and the file it wrote could be different
        // ones. Invisible until `>( )` started a process per expansion
        // and a second `wc -l` answered `0`.
        let runs_here = self.is_active_builtin(&name) || self.functions.contains_key(&name);
        let pushed = if runs_here {
            match self.push_builtin_output_sink(&cmd.redirects, &name) {
                Ok(pushed) => pushed,
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    return ExecResult::Status(1);
                }
            }
        } else {
            false
        };
        let result = self.dispatch_builtin_or_external_impl(argv, name, cmd, background, builtin_only, array_literal_args);
        if pushed {
            self.pop_builtin_output_sink();
        }
        result
    }

    // Split out from run_single so `builtin NAME...` (its own arm below)
    // can re-enter just the builtin-dispatch-and-external-spawn part,
    // bypassing run_single's own function-shadowing check above -- that
    // bypass is the entire point of `builtin`. `builtin_only` is true
    // only for that recursive call: on no match, it reports "not a
    // shell builtin" instead of falling through to
    // restrict_to_builtins/external-spawn like the ordinary top-level
    // path does. `array_literal_args` is the same side-channel argv's
    // own doc comment above describes -- empty for the `builtin` arm's
    // own recursive call (`builtin declare -A m=(...)` isn't supported,
    // an accepted narrow gap given how rare combining the two is).
    fn dispatch_builtin_or_external_impl(
        &mut self,
        argv: &[String],
        name: String,
        cmd: &SimpleCommand,
        background: bool,
        builtin_only: bool,
        array_literal_args: &[(usize, String, AssignMode, Vec<ArrayLiteralItem>)],
    ) -> ExecResult {
        // `enable -n NAME` takes a builtin out of service so the external
        // of the same name runs instead. Done by handing the dispatch a
        // name no arm can match, rather than by jumping past it: the
        // external path is the code immediately after this match, and
        // this is the one place that chooses between the two.
        //
        // `builtin NAME` does not get an exemption. It looks as though
        // it should -- `builtin` is how you reach past a *function* of
        // the same name -- but bash refuses a disabled builtin there
        // too, with "not a shell builtin", which is exactly what
        // falling through this match produces.
        let dispatch_name: &str = if self.disabled_builtins.contains(&name) { "\u{0}disabled" } else { name.as_str() };
        match dispatch_name {
            // POSIX special builtin: does nothing, exits 0. Its arguments
            // are still expanded (already done via argv above) for side
            // effects like `: ${x:=default}`. Its own redirects (e.g.
            // `: > file`) still create/truncate the target, same as real
            // bash -- a side effect of the output-sink override the outer
            // dispatch_builtin_or_external wrapper installs regardless of
            // which builtin ends up running, not anything special-cased
            // here.
            ":" => return ExecResult::Status(0),
            // command [-v|-V] name [args...]. -v/-V (by far the dominant
            // real-world use, e.g. `command -v git >/dev/null`) report
            // what `name` resolves to without running it. Bare `command
            // name args...` runs it, bypassing shell functions -- the
            // other thing `command` is for -- by spawning directly as an
            // external process; unlike real bash this also bypasses
            // builtins of the same name, a simplification accepted since
            // this shell doesn't have many commonly-shadowed builtins.
            "command" => {
                let mut i = 1;
                let mut mode_v = false;
                let mut mode_vv = false;
                let mut mode_p = false;
                while i < argv.len() {
                    match argv[i].as_str() {
                        "-v" => {
                            mode_v = true;
                            i += 1;
                        }
                        "-V" => {
                            mode_vv = true;
                            i += 1;
                        }
                        "-p" => {
                            mode_p = true;
                            i += 1;
                        }
                        _ => break,
                    }
                }
                if mode_v || mode_vv {
                    return ExecResult::Status(match argv.get(i) {
                        Some(n) => self.command_v(n, mode_vv),
                        None => 1,
                    });
                }
                if i >= argv.len() {
                    return ExecResult::Status(0);
                }
                if self.opt_restricted && mode_p {
                    sh_eprintln!(self, "bish: command: -p: restricted");
                    return ExecResult::Status(1);
                }
                if self.check_restricted_command_name(&argv[i]) {
                    return ExecResult::Status(1);
                }
                let mut ext = self.command(&argv[i]);
                ext.args(&argv[i + 1..]);
                ext.current_dir(&self.cwd);
                // Without these, `command foo` would always inherit the
                // real process's own stdio, invisible to a converted
                // foreground subshell/command-substitution/proc-sub's own
                // capture (see spawn_stdin_stdio/spawn_stdout_stdio's own
                // doc comment) -- confirmed the hard way: `mise`'s own
                // activation script shadows itself with a `mise()`
                // function that calls `command "$__MISE_EXE" ...` to reach
                // the real binary, so `$(mise hook-env ...)` from inside
                // it went straight through this path, silently landing on
                // the real terminal instead of being captured.
                ext.stdin(self.spawn_stdin_stdio());
                ext.stdout(self.spawn_stdout_stdio());
                // See apply_fd_redirects' pre_exec comment: without this,
                // `command foo` would inherit bish's own ignored SIGINT
                // and never respond to Ctrl-C.
                unsafe {
                    ext.pre_exec(|| {
                        sigaction_raw(2, SIG_DFL);
                        Ok(())
                    });
                }
                self.note_external_spawn();
                return match ext.status() {
                    Ok(status) => ExecResult::Status(exit_code_from_status(status)),
                    Err(e) => {
                        // Same three answers and two statuses as an
                        // ordinary command word -- see the Err arm in
                        // spawn_external.
                        let name = &argv[i];
                        let not_found = e.kind() == std::io::ErrorKind::NotFound;
                        let (text, status) = match (not_found, name.contains('/')) {
                            (true, false) => ("command not found".to_string(), 127),
                            (true, true) => (os_message(&e), 127),
                            (false, _) if std::path::Path::new(name).is_dir() => ("Is a directory".to_string(), 126),
                            (false, _) => (os_message(&e), 126),
                        };
                        sh_eprintln!(self, "bish: {}: {}", name, text);
                        ExecResult::Status(status)
                    }
                };
            }
            // builtin NAME [args...]: forces the real builtin even when a
            // same-named function is defined -- the explicit bypass for
            // the function-shadowing run_single now does by default (see
            // its own doc comment). Unlike an ordinary command that falls
            // back to spawning an external program when nothing matches,
            // `builtin` never does that (builtin_only: true below) --
            // matching bash's own "not a shell builtin" error instead.
            // Bare `builtin` (no name) is a silent no-op, matching bash.
            "builtin" => {
                if argv.len() < 2 {
                    return ExecResult::Status(0);
                }
                let inner_name = argv[1].clone();
                return self.dispatch_builtin_or_external(&argv[1..], inner_name, cmd, background, true, &[]);
            }
            "type" => return ExecResult::Status(crate::builtins::shell::run_type(self, &argv[1..])),
            // No command-path cache exists to manage -- every exec
            // re-resolves PATH via Command::status() itself -- so this is
            // a documented no-op rather than a real cache: `hash -r`
            // (clear) and bare `hash cmd` (remember) both just succeed.
            // Bare `hash` with no args normally lists the cache; ours is
            // always empty, so that's the one output bash-compat requires.
            "hash" => {
                if let Some(bad) = first_unknown_option(&argv[1..], "lrpdt") {
                    let usage = "hash [-lr] [-p pathname] [-dt] [name ...]";
                    return ExecResult::Status(bad_option_status(self, "hash", &bad, usage));
                }
                if argv.len() == 1 {
                    sh_println!(self, "hash: hash table empty");
                }
                return ExecResult::Status(0);
            }
            // Real builtins, as they are in bash: a `while true` loop
            // should not spawn a process per iteration.
            "true" => return ExecResult::Status(0),
            "false" => return ExecResult::Status(1),
            // Job control belongs to the terminal, and a shell without
            // it cannot hand itself back -- bash says exactly this.
            "suspend" => {
                if !self.opt_monitor {
                    sh_eprintln!(self, "bish: suspend: cannot suspend: no job control");
                    return ExecResult::Status(1);
                }
                unsafe extern "C" {
                    fn getpid() -> i32;
                }
                send_signal(unsafe { getpid() } as u32, 19);
                return ExecResult::Status(0);
            }
            "cd" => return ExecResult::Status(crate::builtins::dirs::run_cd(self, &argv[1..])),
            // A builtin, as it is in bash, and not for tidiness: it has
            // to report the *shell's* idea of where it is, it has to
            // work when PATH is empty, and `type pwd` has to say so.
            // Resolving to /usr/bin/pwd meant all three were wrong.
            "pwd" => {
                // `-P` is the resolved path and `-L` the one the shell
                // holds; they are the same here, because `cd` resolves
                // symlinks as it goes (see the divergence recorded for
                // that in bashdiff).
                let physical = argv[1..].iter().any(|a| a == "-P");
                let here = match physical {
                    true => std::fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.cwd.clone()),
                    false => self.cwd.clone(),
                };
                sh_println!(self, "{}", here.display());
                return ExecResult::Status(0);
            }
            // `e [ARG...]`: bubbles up via ExecResult::Edit -- see its
            // own doc comment, and Fg's, for why this can't just be
            // driven from here directly. The arguments are passed on
            // unparsed for the same reason: deciding what they mean
            // needs the editor, not the shell.
            "e" => {
                self.pending_edit = Some(argv[1..].to_vec());
                return ExecResult::Edit;
            }
            // Real builtins (not just something the external /bin/echo,
            // /bin/printf would cover) -- see run_echo/run_printf's own
            // doc comments. Matters beyond ordinary interactive use:
            // command mode (restrict_to_builtins, below) disallows
            // externals outright, and echo/printf are two of the most
            // reached-for commands for producing quick output there.
            "echo" => return ExecResult::Status(crate::builtins::io::run_echo(self, &argv[1..])),
            "printf" => return ExecResult::Status(crate::builtins::io::run_printf(self, &argv[1..])),
            "umask" => return ExecResult::Status(crate::builtins::limits::run_umask(self, &argv[1..])),
            "times" => return ExecResult::Status(crate::builtins::limits::run_times(self, &argv[1..])),
            "enable" => return ExecResult::Status(crate::builtins::shell::run_enable(self, &argv[1..])),
            "help" => return ExecResult::Status(crate::builtins::shell::run_help(self, &argv[1..])),
            "caller" => return ExecResult::Status(crate::builtins::shell::run_caller(self, &argv[1..])),
            "ulimit" => return ExecResult::Status(crate::builtins::limits::run_ulimit(self, &argv[1..])),
            // alias/unalias: store and query only, no expansion when a
            // command runs -- see the comment on the `aliases` field for
            // why.
            "alias" => {
                // `-p` prints them all, the same as no arguments --
                // bash's flag for "give me something re-runnable", and
                // it used to be read as an alias *name* and reported
                // as not found.
                if argv.len() == 1 || argv[1..].iter().all(|a| a == "-p") {
                    let mut sorted: Vec<&(String, String)> = self.aliases.iter().collect();
                    sorted.sort_by(|a, b| a.0.cmp(&b.0));
                    for (n, v) in sorted {
                        sh_println!(self, "alias {}={}", n, crate::serialize::quote_literal(v));
                    }
                    return ExecResult::Status(0);
                }
                let mut status = 0;
                for a in &argv[1..] {
                    match a.find('=') {
                        Some(eq) => {
                            let name = a[..eq].to_string();
                            let val = a[eq + 1..].to_string();
                            match self.aliases.iter_mut().find(|(n, _)| *n == name) {
                                Some(existing) => existing.1 = val,
                                None => self.aliases.push((name, val)),
                            }
                        }
                        None => match self.aliases.iter().find(|(n, _)| n == a) {
                            Some((n, v)) => sh_println!(self, "alias {}={}", n, crate::serialize::quote_literal(v)),
                            None => {
                                sh_eprintln!(self, "bish: alias: {}: not found", a);
                                status = 1;
                            }
                        },
                    }
                }
                return ExecResult::Status(status);
            }
            "unalias" => {
                if argv[1..].iter().any(|a| a == "-a") {
                    self.aliases.clear();
                    return ExecResult::Status(0);
                }
                let mut status = 0;
                for a in &argv[1..] {
                    match self.aliases.iter().position(|(n, _)| n == a) {
                        Some(pos) => {
                            self.aliases.remove(pos);
                        }
                        None => {
                            sh_eprintln!(self, "bish: unalias: {}: not found", a);
                            status = 1;
                        }
                    }
                }
                return ExecResult::Status(status);
            }
            "abbr" => return ExecResult::Status(crate::builtins::bish::run_abbr(self, &argv[1..])),
            "history" => return ExecResult::Status(crate::builtins::history::run_history(self, &argv[1..])),
            "fc" => return ExecResult::Status(crate::builtins::history::run_fc(self, &argv[1..])),
            // `= EXPR`: evaluate and print. The other half of the inline
            // calculator whose answer the prompt already shows as ghost
            // text while it is being typed (bishedit::suggestion::
            // ArithSuggestionProvider) -- pressing Enter runs this and
            // makes the answer real output, so it can be piped, captured
            // or scrolled back to.
            "=" => return ExecResult::Status(self.run_arith_print(&argv[1..])),
            "shopt" => return ExecResult::Status(crate::builtins::shell::run_shopt(self, &argv[1..])),
            "bishopt" => return ExecResult::Status(crate::builtins::bish::run_bishopt(self, &argv[1..], KNOWN_BISHOPTS)),
            // `::bish SUBCOMMAND...`: a dedicated namespace for bish-
            // specific commands that don't belong as an ordinary top-
            // level builtin name (see run_bish's own doc comment for
            // why) -- `theme` (begin/end a theme declaration) is the
            // first subcommand.
            "::bish" => return crate::builtins::bish::run_bish(self, &argv[1..]),
            "compgen" => return ExecResult::Status(crate::builtins::completion::run_compgen(self, &argv[1..])),
            "complete" => return ExecResult::Status(crate::builtins::completion::run_complete(self, &argv[1..])),
            "compopt" => return ExecResult::Status(crate::builtins::completion::run_compopt(self, &argv[1..])),
            // Available in command mode, and at any prompt a real window
            // manager is behind. It used to be
            // command-mode-exclusive and is again, but for a better
            // reason than before: `::bish window` is the canonical
            // spelling everywhere else. A top-level builtin called
            // `window` shadows any real `window` on `$PATH` for every
            // script run under bish, which is a lot to charge for a
            // shorter name. The colon line is the one place the short
            // form costs nothing -- it runs builtins exclusively, so
            // there is no external there for it to shadow.
            // "w" deliberately isn't an alias here (unlike "win") -- it's
            // reserved for a future vim-style `:w` write command instead,
            // matching bishedit's normal-mode Ctrl-W leader now covering
            // window management directly (see vimkeys.rs's WindowCmd).
            "window" | "win" if self.restrict_to_builtins => {
                return crate::builtins::bish::run_window(self, &argv[1..]);
            }
            "pushd" => return ExecResult::Status(crate::builtins::dirs::run_pushd(self, &argv[1..])),
            "popd" => return ExecResult::Status(crate::builtins::dirs::run_popd(self, &argv[1..])),
            "dirs" => return ExecResult::Status(crate::builtins::dirs::run_dirs(self, &argv[1..])),
            // `export` is equivalent to `declare -x` (real bash documents
            // it that way) -- routing through run_declare means it shares
            // -x's local-variable-mirroring behavior instead of the
            // simpler, env-only handling this had before.
            "export" => {
                // `export -f NAME` marks a *function* for export.
                // bash carries it to a child through the environment;
                // bish's own children already inherit the function
                // table wholesale (see `new_virtual_child`), so there
                // is nothing to arrange -- but it has to be quiet and
                // it has to notice a name that is not a function,
                // rather than falling through to `declare -f`, which
                // prints the body.
                if argv.get(1).map(String::as_str) == Some("-f") {
                    let mut status = 0;
                    for name in &argv[2..] {
                        if !self.functions.contains_key(name) {
                            sh_eprintln!(self, "bish: export: {name}: not a function");
                            status = 1;
                        }
                    }
                    return ExecResult::Status(status);
                }
                // `export -n NAME` takes the export attribute away and
                // leaves the value. It cannot go through run_declare:
                // `-n` there means *nameref*, so this used to turn the
                // variable into one and lose its value entirely.
                if argv.get(1).map(String::as_str) == Some("-n") {
                    for name in &argv[2..] {
                        self.exported_names.remove(name);
                    }
                    return ExecResult::Status(0);
                }
                let mut declare_args = vec!["-x".to_string()];
                declare_args.extend(argv[1..].iter().cloned());
                // Dropping argv[0] ("export") shifts every recorded
                // position back by one; prepending "-x" here shifts them
                // forward by one again -- net zero, so array_literal_args
                // (itself indexed into the *original* argv) already lines
                // up with declare_args unchanged.
                return ExecResult::Status(crate::builtins::vars::run_declare(self, &name, &declare_args, array_literal_args));
            }
            "let" => {
                let mut last = 0i64;
                for a in &argv[1..] {
                    match self.eval_arith(a) {
                        Ok(v) => last = v,
                        Err(e) => {
                            sh_eprintln!(self, "bish: let: {}", e);
                            return ExecResult::Status(2);
                        }
                    }
                }
                return ExecResult::Status(if last != 0 { 0 } else { 1 });
            }
            "break" => return builtins::break_loop(&argv[1..]),
            "continue" => return builtins::continue_loop(&argv[1..]),
            "test" => {
                let (vars, opts) = self.shell_test_answers(&argv[1..]);
                let outcome = builtins::test(&argv[1..], false, &builtins::ShellFacts { var_is_set: &vars, option_on: &opts });
                return ExecResult::Status(match outcome {
                    Ok(status) => status,
                    Err(e) => {
                        sh_eprintln!(self, "bish: test: {}", e);
                        2
                    }
                });
            }
            // `[[` is a keyword (see lexer.rs/parser.rs Command::Test), not
            // a plain command name, so it never reaches this dispatch --
            // only bracket-style `[ ... ]` (the `test` alias) does.
            "[" => {
                let mut a = argv[1..].to_vec();
                if a.last().map(|s| s.as_str()) == Some("]") {
                    a.pop();
                } else {
                    sh_eprintln!(self, "bish: [: missing closing ]");
                    return ExecResult::Status(2);
                }
                let (vars, opts) = self.shell_test_answers(&a);
                let outcome = builtins::test(&a, false, &builtins::ShellFacts { var_is_set: &vars, option_on: &opts });
                return ExecResult::Status(match outcome {
                    Ok(status) => status,
                    Err(e) => {
                        sh_eprintln!(self, "bish: [: {}", e);
                        2
                    }
                });
            }
            "return" => {
                let code = argv.get(1).and_then(|s| s.parse::<i32>().ok()).unwrap_or(self.last_status);
                if self.var_scopes.is_empty() {
                    sh_eprintln!(self, "bish: return: can only 'return' from a function");
                    return ExecResult::Status(code);
                }
                return ExecResult::Return(code);
            }
            "shift" => {
                if argv.len() > 2 {
                    sh_eprintln!(self, "bish: shift: too many arguments");
                    return ExecResult::Status(1);
                }
                let n = match argv.get(1) {
                    Some(a) => match a.parse::<usize>() {
                        Ok(n) => n,
                        Err(_) => {
                            sh_eprintln!(self, "bish: shift: {}: numeric argument required", a);
                            return ExecResult::Status(2);
                        }
                    },
                    None => 1,
                };
                // Shifting past the end is a no-op that *reports* --
                // `while shift; do ...; done` is how a script walks its
                // arguments, and it never stops if this always says 0.
                let available = self.arg_frames.last().map(Vec::len).unwrap_or(0);
                if n > available {
                    return ExecResult::Status(1);
                }
                if let Some(frame) = self.arg_frames.last_mut() {
                    frame.drain(0..n);
                }
                return ExecResult::Status(0);
            }
            "local" => {
                if self.var_scopes.is_empty() {
                    sh_eprintln!(self, "bish: local: can only be used inside a function");
                    return ExecResult::Status(1);
                }
                // `-a`/`-A` name an array rather than a scalar. `arrays`/
                // `assoc_arrays` themselves stay flat maps (see the
                // comments on those fields), but the pre-local value is
                // snapshotted onto array_local_stack/assoc_local_stack so
                // call_function can restore it when the function returns --
                // giving `local -a`/`-A` real scoping without needing every
                // array read/write site to walk a scope chain.
                let mut array_mode: Option<bool> = None;
                let mut integer_flag = false;
                let mut nameref_flag = false;
                // -u/-l/-x attribute membership (like -i above) isn't
                // unwound when the function returns -- a narrower, already-
                // accepted gap than the array/nameref leak this function
                // otherwise guards against, since getting a case-fold or
                // export attribute stuck past its function is cosmetic
                // rather than silently wrong data.
                let mut upper_flag = false;
                let mut lower_flag = false;
                let mut export_flag = false;
                // `-g`: force the write to the true global scope instead
                // of a local shadow -- confirmed against real bash
                // (`f() { local -g x=5; }; f; echo "$x"` prints "5"),
                // even though `local`'s whole point is otherwise to
                // localize. Only meaningful for the scalar (non-array,
                // non-nameref) case below.
                let mut global_flag = false;
                // array_literal_args is indexed into the *original* argv
                // (which still has argv[0] == "local"), so every position
                // shifts back by one to line up with argv[1..]'s own
                // enumeration below.
                let shifted_array_literals: Vec<_> =
                    array_literal_args.iter().filter_map(|(p, n, m, i)| p.checked_sub(1).map(|p2| (p2, n.clone(), *m, i.clone()))).collect();
                // `-r` was not handled at all, and the letters did not
                // cluster: `local -ir v=1` set neither attribute, so
                // the readonly was silently not a readonly. Same shape
                // as `declare`'s own list -- every flag here is a
                // single letter with no argument of its own.
                let mut readonly_flag = false;
                for (i, a) in argv[1..].iter().enumerate() {
                    if a.len() > 1 && a.starts_with('-') && a != "--" {
                        for c in a.chars().skip(1) {
                            match c {
                                'a' => array_mode = Some(false),
                                'A' => array_mode = Some(true),
                                'i' => integer_flag = true,
                                'n' => nameref_flag = true,
                                'u' => upper_flag = true,
                                'l' => lower_flag = true,
                                'x' => export_flag = true,
                                'g' => global_flag = true,
                                'r' => readonly_flag = true,
                                _ => {}
                            }
                        }
                        continue;
                    }
                    if a == "--" {
                        continue;
                    }
                    // `local -A m=([a]=1 [b]=2)` -- this position is
                    // actually an array literal (`a` here is just its
                    // xtrace-only display text). The table-snapshot/
                    // reset dance below is identical to the plain-name
                    // array case; only the population step (via
                    // apply_array_literal) is new.
                    if let Some((_, name, mode, items)) = shifted_array_literals.iter().find(|(pos, ..)| *pos == i) {
                        match array_mode {
                            Some(true) => {
                                let prev = self.assoc_arrays.remove(name);
                                let was = self.assoc_names.contains(name);
                                self.assoc_local_stack.last_mut().unwrap().push((name.clone(), prev, was));
                                self.assoc_names.insert(name.clone());
                                self.assoc_arrays.insert(name.clone(), OrderedMap::default());
                            }
                            Some(false) => {
                                let prev = self.arrays.remove(name);
                                let was = self.array_names.contains(name);
                                self.array_local_stack.last_mut().unwrap().push((name.clone(), prev, was));
                                self.array_names.insert(name.clone());
                                self.arrays.insert(name.clone(), std::collections::BTreeMap::new());
                            }
                            None => {}
                        }
                        self.apply_array_literal(name, *mode, items);
                        for (set, table) in [
                            (integer_flag, &mut self.integer_names),
                            (upper_flag, &mut self.upper_names),
                            (lower_flag, &mut self.lower_names),
                            (export_flag, &mut self.exported_names),
                            (readonly_flag, &mut self.readonly_names),
                        ] {
                            if set {
                                table.insert(name.clone());
                            }
                        }
                        continue;
                    }
                    let (n, v) = match a.find('=') {
                        Some(eq) => (a[..eq].to_string(), Some(a[eq + 1..].to_string())),
                        None => (a.clone(), None),
                    };
                    if nameref_flag {
                        let was_nameref = self.nameref_names.contains(&n);
                        self.nameref_local_stack.last_mut().unwrap().push((n.clone(), was_nameref));
                        self.nameref_names.insert(n.clone());
                        self.var_scopes.last_mut().unwrap().insert(n.clone(), Some(v.unwrap_or_default()));
                        continue;
                    }
                    if integer_flag {
                        self.integer_names.insert(n.clone());
                    }
                    if upper_flag {
                        self.upper_names.insert(n.clone());
                    }
                    if lower_flag {
                        self.lower_names.insert(n.clone());
                    }
                    if export_flag {
                        self.exported_names.insert(n.clone());
                    }
                    match array_mode {
                        // The bare `local -a x` form declares without
                        // assigning: the attribute is local, and there
                        // is no value until something writes one.
                        Some(true) => {
                            let prev = self.assoc_arrays.remove(&n);
                            let was = self.assoc_names.contains(&n);
                            self.assoc_local_stack.last_mut().unwrap().push((n.clone(), prev, was));
                            self.assoc_names.insert(n);
                        }
                        Some(false) => {
                            let prev = self.arrays.remove(&n);
                            let was = self.array_names.contains(&n);
                            self.array_local_stack.last_mut().unwrap().push((n.clone(), prev, was));
                            self.array_names.insert(n);
                        }
                        None => {
                            if global_flag {
                                self.assign_var_global(&n, v.unwrap_or_default());
                                continue;
                            }
                            // `local x` with no `=` declares the name
                            // and leaves it *unset* -- see var_scopes.
                            // Storing an empty string here is what made
                            // `${x-default}` inside a function unable
                            // to tell "my caller did not set this".
                            let Some(v) = v else {
                                self.var_scopes.last_mut().unwrap().insert(n, None);
                                continue;
                            };
                            let v = if integer_flag { arith::eval(&v, self).unwrap_or(0).to_string() } else { v };
                            let v = if upper_flag {
                                v.to_uppercase()
                            } else if lower_flag {
                                v.to_lowercase()
                            } else {
                                v
                            };
                            if export_flag {
                                self.exported_names.insert(n.clone());
                                self.export_to_environment(&n, &v);
                            }
                            if readonly_flag {
                                self.readonly_names.insert(n.clone());
                            }
                            self.var_scopes.last_mut().unwrap().insert(n, Some(v));
                        }
                    }
                }
                return ExecResult::Status(0);
            }
            "exit" => {
                let code = match argv.get(1) {
                    Some(a) => match a.parse::<i32>() {
                        Ok(n) => n,
                        // Still exits -- bash does too, with 2, rather
                        // than carrying on as if `exit` had not been
                        // written.
                        Err(_) => {
                            sh_eprintln!(self, "bish: exit: {}: numeric argument required", a);
                            self.run_exit_trap();
                            return ExecResult::Exit(2);
                        }
                    },
                    None => self.last_status,
                };
                self.run_exit_trap();
                return ExecResult::Exit(code);
            }
            "read" => {
                let mut array_name: Option<&str> = None;
                let mut names: Vec<&str> = Vec::new();
                let mut prompt: Option<&str> = None;
                let mut nchars: Option<usize> = None;
                let mut delim: u8 = b'\n';
                let mut read_u_flag: Option<&str> = None;
                // -s (silent/no-echo): suppresses terminal echo for the
                // duration of this one read, via term::NoEchoGuard (same
                // hand-rolled-against-glibc termios layout RawGuard already
                // uses, no libc crate). Only meaningful against a real,
                // interactive terminal -- gated the same way `-p`'s own
                // prompt already is below (is_real_stdin && stdin_is_tty()),
                // and only when this read isn't actually coming from a
                // coproc fd (-u) instead of the terminal.
                let mut silent = false;
                // Without `-r`, a backslash escapes the next character
                // -- and the delimiter, which continues the line.
                let mut raw = false;
                let mut timeout_arg: Option<&str> = None;
                // Short options cluster, the way every other shell
                // builtin's do: `-ra arr` is `-r -a arr`, and `-rn2` is
                // `-r -n 2`. Matching whole argument strings meant
                // `read -ra p` took `-ra` for a *variable name*, so the
                // commonest spelling of the commonest idiom silently
                // read into nothing.
                let mut i = 1;
                'args: while i < argv.len() {
                    let a = argv[i].as_str();
                    if a == "--" {
                        i += 1;
                        while i < argv.len() {
                            names.push(argv[i].as_str());
                            i += 1;
                        }
                        break;
                    }
                    if a.len() < 2 || !a.starts_with('-') {
                        names.push(a);
                        i += 1;
                        continue;
                    }
                    let bytes = a.as_bytes();
                    let mut ci = 1;
                    while ci < bytes.len() {
                        let c = bytes[ci] as char;
                        // The options that take a value take the rest of
                        // the cluster if there is one, and the next
                        // argument otherwise.
                        if matches!(c, 'a' | 'd' | 'i' | 'n' | 'N' | 'p' | 't' | 'u') {
                            let value: Option<&str> = if ci + 1 < bytes.len() {
                                Some(&a[ci + 1..])
                            } else {
                                i += 1;
                                argv.get(i).map(|s| s.as_str())
                            };
                            match c {
                                'a' => array_name = value,
                                'p' => prompt = value,
                                'n' | 'N' => nchars = value.and_then(|v| v.parse::<usize>().ok()),
                                // `-d ''` is bash's spelling of "delimit
                                // on NUL", which is what `find -print0`
                                // and `git ls-files -z` produce -- the
                                // empty string has no first byte, so it
                                // cannot simply fall back to a newline.
                                'd' => delim = value.map(|v| v.bytes().next().unwrap_or(0)).unwrap_or(b'\n'),
                                // Enforced below, and only against real
                                // stdin (see is_real_stdin).
                                't' => timeout_arg = value,
                                'u' => read_u_flag = value,
                                // `-i text` seeds the line editor, which
                                // this read has none of: accepted and
                                // ignored, like bash does with no tty.
                                _ => {}
                            }
                            break;
                        }
                        match c {
                            'r' => raw = true,
                            's' => silent = true,
                            // `-e`/`-E`: read through the line editor.
                            // Accepted; there is nothing to edit with.
                            'e' | 'E' => {}
                            _ => {
                                let usage = "read [-Eers] [-a array] [-d delim] [-i text] [-n nchars] [-N nchars] [-p prompt] [-t timeout] [-u fd] [name ...]";
                                return ExecResult::Status(bad_option_status(self, "read", &format!("-{c}"), usage));
                            }
                        }
                        ci += 1;
                    }
                    i += 1;
                    let _ = &mut i;
                    continue 'args;
                }
                let timeout_secs = timeout_arg.and_then(|s| s.parse::<f64>().ok());
                let is_real_stdin = !cmd.redirects.iter().any(|r| matches!(r, Redirect::In(_) | Redirect::HereString(_) | Redirect::HereDoc(_)));
                if let Some(p) = prompt {
                    if is_real_stdin && stdin_is_tty() {
                        sh_eprint!(self, "{}", p);
                        let _ = std::io::Write::flush(&mut std::io::stderr());
                    }
                }
                if let Some(secs) = timeout_secs {
                    // `-t 0` is a question, not a read: has the source
                    // got something to give right now? It consumes
                    // nothing and assigns nothing, which is what makes
                    // it useful as a "would this block?" test.
                    if secs == 0.0 {
                        // A file, here-doc or here-string is always
                        // ready -- there is nothing to wait for.
                        let ready = !is_real_stdin || stdin_ready(0);
                        return ExecResult::Status(i32::from(!ready));
                    }
                    if is_real_stdin {
                        let ms = (secs * 1000.0).max(0.0) as i32;
                        if !stdin_ready(ms) {
                            return ExecResult::Status(1);
                        }
                    }
                    // A non-stdin source (file/heredoc/here-string redirect)
                    // has no pollable fd for a meaningful timeout check, so
                    // -t against those is treated as immediately ready.
                }

                // `-u FD` reads from a fd this shell already has open (most
                // commonly a coproc's read end, coproc_fds[NAME[0]]) instead
                // of the command's own stdin/redirect -- borrowed tightly in
                // its own block so the borrow of self.coproc_fds ends before
                // the assign_var calls below need `&mut self` again.
                let ufd = read_u_flag.and_then(|s| s.parse::<i32>().ok());
                let _silent_guard =
                    if silent && ufd.is_none() && is_real_stdin && stdin_is_tty() { crate::term::NoEchoGuard::enable(0).ok() } else { None };
                let (got, clean): (Option<String>, bool) = if let Some(fd) = ufd {
                    match self.coproc_fds.get_mut(&fd) {
                        Some(KeptFd::Read(r)) => read_line_or_chars(r, nchars, delim, raw),
                        Some(KeptFd::Write(_)) => {
                            sh_eprintln!(self, "bish: read: {}: invalid file descriptor", fd);
                            return ExecResult::Status(1);
                        }
                        // Not a coproc's end: an ordinary descriptor
                        // this shell holds open, `exec 3<file`'s being
                        // the usual one. Unbuffered for the same reason
                        // stdin is -- see UnbufferedFd.
                        None if !UnbufferedFd::is_open(fd) => {
                            sh_eprintln!(self, "bish: read: {}: invalid file descriptor: Bad file descriptor", fd);
                            return ExecResult::Status(1);
                        }
                        None => {
                            let mut reader = UnbufferedFd::new(fd);
                            read_line_or_chars(&mut reader, nchars, delim, raw)
                        }
                    }
                } else {
                    let mut reader = self.read_input_source(cmd);
                    read_line_or_chars(&mut *reader, nchars, delim, raw)
                };

                return match got {
                    None => ExecResult::Status(1),
                    Some(line) => {
                        let ifs = self.get_ifs();
                        // Without `-r` the escapes come off, and what
                        // they protected is remembered so a separator
                        // one of them covered stays part of its field.
                        let (line, mask) = if raw { (line, Vec::new()) } else { unescape_read_line(&line) };
                        let chars: Vec<char> = line.chars().collect();
                        let mask = if raw { vec![false; chars.len()] } else { mask };
                        if let Some(arr) = array_name {
                            let parts = ifs_tokenize_masked(&chars, &mask, &ifs);
                            let map: std::collections::BTreeMap<usize, String> = parts.into_iter().enumerate().collect();
                            self.arrays.insert(arr.to_string(), map);
                            self.array_names.insert(arr.to_string());
                        } else if names.is_empty() {
                            self.assign_var("REPLY", line.clone());
                        } else {
                            let is_ifs_ws = |i: usize| !mask[i] && chars[i].is_whitespace() && ifs.contains(chars[i]);
                            let mut at = 0;
                            while at < chars.len() && is_ifs_ws(at) {
                                at += 1;
                            }
                            for (i, n) in names.iter().enumerate() {
                                if i == names.len() - 1 {
                                    let mut end = chars.len();
                                    while end > at && is_ifs_ws(end - 1) {
                                        end -= 1;
                                    }
                                    let rest: String = chars[at..end].iter().collect();
                                    self.assign_var(n, rest);
                                } else {
                                    match ifs_next_field_masked(&chars, &mask, &ifs, at) {
                                        Some((field, next)) => {
                                            self.assign_var(n, field);
                                            at = next;
                                        }
                                        None => {
                                            let rest: String = chars[at..].iter().collect();
                                            self.assign_var(n, rest);
                                            at = chars.len();
                                        }
                                    }
                                }
                            }
                        }
                        ExecResult::Status(if clean { 0 } else { 1 })
                    }
                };
            }
            // mapfile/readarray [-t] [name]. Reads lines from stdin (or
            // this command's own `<` redirect, via the same
            // read_input_source machinery `read` uses) into an indexed
            // array, one element per line -- default array name MAPFILE.
            // Only `-t` (strip trailing newlines) is recognized; other
            // real bash flags (-n/-O/-s/-C/-c/-u/-d) are accepted and
            // ignored rather than erroring, a scoped subset covering the
            // overwhelmingly common `mapfile -t arr < file` usage.
            "mapfile" | "readarray" => {
                let mut strip_newline = false;
                let mut array_name = "MAPFILE".to_string();
                // The counted options, each acted on now rather than
                // only validated: `-n` how many lines to keep, `-s` how
                // many to skip first, `-O` the index to start writing
                // at, `-u` the descriptor to read instead of stdin.
                // (`-c`/`-C` are the progress callback, still accepted
                // and ignored -- there is nothing to call back into.)
                let mut max_lines = 0u64;
                let mut skip = 0u64;
                let mut origin = 0u64;
                let mut from_fd: Option<i32> = None;
                let mut bad_count: Option<(String, String)> = None;
                // What separates one element from the next. `-d` was
                // parsed and then thrown away, so `mapfile -d, arr`
                // read *lines* -- and the option did not cluster
                // either, so `-d,` was rejected outright as `-,`.
                let mut delim = b'\n';
                let usage = format!("{} [-d delim] [-n count] [-O origin] [-s count] [-t] [-u fd] [-C callback] [-c quantum] [array]", name);
                let mut i = 1;
                while i < argv.len() {
                    let a = argv[i].as_str();
                    if a == "--" {
                        if let Some(n) = argv.get(i + 1) {
                            array_name = n.clone();
                        }
                        break;
                    }
                    if a.len() < 2 || !a.starts_with('-') {
                        array_name = a.to_string();
                        i += 1;
                        continue;
                    }
                    let bytes = a.as_bytes();
                    let mut ci = 1;
                    while ci < bytes.len() {
                        let c = bytes[ci] as char;
                        if matches!(c, 'd' | 'n' | 'O' | 's' | 'u' | 'C' | 'c') {
                            let value: Option<&str> = if ci + 1 < bytes.len() {
                                Some(&a[ci + 1..])
                            } else {
                                i += 1;
                                argv.get(i).map(|s| s.as_str())
                            };
                            let Some(value) = value else {
                                sh_eprintln!(self, "bish: {}: -{}: option requires an argument", name, c);
                                sh_eprintln!(self, "{}: usage: {}", name, usage);
                                return ExecResult::Status(2);
                            };
                            match c {
                                // An empty `-d ''` delimits on NUL, as
                                // it does for `read`.
                                'd' => delim = value.bytes().next().unwrap_or(0),
                                'n' | 's' | 'O' | 'u' => match value.parse::<u64>() {
                                    Ok(v) => match c {
                                        'n' => max_lines = v,
                                        's' => skip = v,
                                        'O' => origin = v,
                                        _ => from_fd = Some(v as i32),
                                    },
                                    Err(_) => {
                                        let what = match c {
                                            'O' => "invalid array origin",
                                            'u' => "invalid file descriptor specification",
                                            _ => "invalid line count",
                                        };
                                        bad_count = Some((value.to_string(), what.to_string()));
                                    }
                                },
                                // `-C callback` and `-c quantum`: the
                                // progress hook, accepted and ignored --
                                // there is nothing to call back into.
                                _ => {}
                            }
                            break;
                        }
                        match c {
                            't' => strip_newline = true,
                            _ => {
                                return ExecResult::Status(bad_option_status(self, &name, &format!("-{c}"), &usage));
                            }
                        }
                        ci += 1;
                    }
                    i += 1;
                }
                if let Some((value, what)) = bad_count {
                    sh_eprintln!(self, "bish: {}: {}: {}", name, value, what);
                    return ExecResult::Status(1);
                }
                let mut reader: Box<dyn std::io::BufRead> = match from_fd {
                    Some(fd) => Box::new(UnbufferedFd::new(fd)),
                    None => self.read_input_source(cmd),
                };
                let mut map = std::collections::BTreeMap::new();
                let mut idx = origin as usize;
                let mut kept = 0u64;
                let mut seen = 0u64;
                loop {
                    if max_lines > 0 && kept >= max_lines {
                        break;
                    }
                    let mut raw: Vec<u8> = Vec::new();
                    match std::io::BufRead::read_until(&mut *reader, delim, &mut raw) {
                        Ok(0) => break,
                        Ok(_) => {
                            let mut line = String::from_utf8_lossy(&raw).into_owned();
                            seen += 1;
                            if seen <= skip {
                                continue;
                            }
                            // `-t` takes off the delimiter, whatever it
                            // is -- not "trailing whitespace". With
                            // `-d,` on `a,b,c` bash keeps the `\n` that
                            // ends the last field.
                            if strip_newline && raw.last() == Some(&delim) {
                                line.pop();
                            }
                            map.insert(idx, line);
                            idx += 1;
                            kept += 1;
                        }
                        Err(_) => break,
                    }
                }
                self.arrays.insert(array_name, map);
                return ExecResult::Status(0);
            }
            // `json [-r] [PATH] [FILE]` -- a `jq`-lite convenience
            // builtin (bash has nothing like this at all): parse JSON,
            // optionally navigate a dotted-path query (`.foo.bar[2]`,
            // default `.` -- identity), pretty-print the result. `-r`
            // prints a string result raw/unquoted (everything else
            // still pretty-printed) -- the common case for pulling one
            // value straight into a shell variable (`name=$(json -r
            // .name config.json)`), matching real jq's own `-r` flag.
            // `PATH` mirrors jq's own convention exactly, including its
            // own well-known gotcha: with exactly one positional
            // argument, that argument is *always* the path, never a
            // filename (`json file.json` tries to query a path literally
            // named "file.json", it does not read that file) -- `json .
            // file.json` is what reads a file with the identity query.
            // See src/json.rs for the actual parser/query engine.
            "json" => {
                let mut raw = false;
                let mut positional: Vec<&str> = Vec::new();
                for a in &argv[1..] {
                    match a.as_str() {
                        "-r" | "--raw" => raw = true,
                        other => positional.push(other),
                    }
                }
                let path = positional.first().copied().unwrap_or(".");
                let text = if let Some(file) = positional.get(1) {
                    match std::fs::read_to_string(file) {
                        Ok(t) => t,
                        Err(e) => {
                            sh_eprintln!(self, "bish: json: {}: {}", file, e);
                            return ExecResult::Status(1);
                        }
                    }
                } else {
                    let mut reader = self.read_input_source(cmd);
                    let mut buf = String::new();
                    if let Err(e) = std::io::Read::read_to_string(&mut *reader, &mut buf) {
                        sh_eprintln!(self, "bish: json: {}", e);
                        return ExecResult::Status(1);
                    }
                    buf
                };
                let value = match crate::json::parse(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        sh_eprintln!(self, "bish: json: {}", e);
                        return ExecResult::Status(1);
                    }
                };
                let result = match crate::json::query(&value, path) {
                    Ok(v) => v,
                    Err(e) => {
                        sh_eprintln!(self, "bish: json: {}", e);
                        return ExecResult::Status(1);
                    }
                };
                let output = match (raw, result) {
                    (true, crate::json::Value::Str(s)) => s.clone(),
                    _ => crate::json::pretty_print(result),
                };
                sh_println!(self, "{}", output);
                return ExecResult::Status(0);
            }
            "eval" => {
                let src = argv[1..].join(" ");
                return self.run_source_here(&src, "eval");
            }
            "source" | "." => {
                let path = match argv.get(1) {
                    Some(p) => p.clone(),
                    None => {
                        sh_eprintln!(self, "bish: {}: filename argument required", name);
                        return ExecResult::Status(2);
                    }
                };
                if self.opt_restricted && path.contains('/') {
                    sh_eprintln!(self, "bish: {}: {}: restricted", name, path);
                    return ExecResult::Status(1);
                }
                match std::fs::read_to_string(self.resolve_path(&path)) {
                    Ok(src) => {
                        // For the duration, this file *is* the script:
                        // a function defined here records it, and a
                        // frame made here names it. Restored afterwards,
                        // since `$0` and every later frame belong to the
                        // outer script again.
                        let outer_script = std::mem::replace(&mut self.script_name, path.clone());
                        // `. file arg...` gives the sourced file its own
                        // positional parameters, and puts the caller's
                        // back afterwards. With none given it keeps the
                        // caller's, which is what makes `. lib.sh`
                        // inside a function still see `$1`.
                        let outer_args = match argv.len() > 2 {
                            true => Some(std::mem::replace(self.arg_frames.last_mut().expect("a positional frame"), argv[2..].to_vec())),
                            false => None,
                        };
                        let result = self.run_source_here(&src, &path);
                        if let Some(args) = outer_args
                            && let Some(frame) = self.arg_frames.last_mut()
                        {
                            *frame = args;
                        }
                        self.script_name = outer_script;
                        // A sourced script fires RETURN when it
                        // finishes, `functrace` or not -- unlike a
                        // function, and unlike `eval`, which is why this
                        // is here rather than inside `run_source_here`.
                        self.run_pseudo_trap(PseudoTrap::Return);
                        return result;
                    }
                    Err(e) => {
                        // A directory gets the builtin's own name in
                        // front of it and bash's lowercase wording;
                        // everything else is the bare path and the
                        // system's message. Checked against bash 5.3.
                        if std::path::Path::new(&path).is_dir() {
                            sh_eprintln!(self, "bish: {}: {}: is a directory", name, path);
                        } else {
                            sh_eprintln!(self, "bish: {}: {}", path, os_message(&e));
                        }
                        return ExecResult::Status(1);
                    }
                }
            }
            "trap" => {
                if let Some(bad) = first_unknown_option(&argv[1..], "Plp") {
                    let usage = "trap [-Plp] [[action] signal_spec ...]";
                    return ExecResult::Status(bad_option_status(self, "trap", &bad, usage));
                }
                // `--` ends the options, and this is not a nicety: it is
                // the form `trap -p` itself prints, so without it the
                // builtin could not read its own output back. `trap --
                // 'echo t' USR1` set a trap whose action was `--` and
                // then rejected `echo t` as a signal name.
                let argv: Vec<String> = match argv.iter().position(|a| a == "--") {
                    Some(at) => argv[..at].iter().chain(argv[at + 1..].iter()).cloned().collect(),
                    None => argv.to_vec(),
                };
                if argv.len() == 1 || argv.get(1).map(|s| s == "-p").unwrap_or(false) {
                    // `trap -p NAME...` reports only those. Without
                    // names every trap is reported, in bash's order:
                    // EXIT, then the signals by number, then the three
                    // pseudo-signals. Those three were missing from the
                    // listing entirely, so a script could set a DEBUG
                    // or RETURN trap and then not find it.
                    let wanted: Vec<String> = argv[2.min(argv.len())..].to_vec();
                    let mut bad = 0;
                    for w in &wanted {
                        let name = w.strip_prefix("SIG").unwrap_or(w);
                        if !matches!(name, "EXIT" | "DEBUG" | "ERR" | "RETURN") && signal_number(name).is_none() {
                            sh_eprintln!(self, "bish: trap: {}: invalid signal specification", w);
                            bad = 1;
                        }
                    }
                    if bad != 0 {
                        return ExecResult::Status(bad);
                    }
                    let asked =
                        |label: &str| wanted.is_empty() || wanted.iter().any(|w| w.strip_prefix("SIG").unwrap_or(w).eq_ignore_ascii_case(label));
                    if let Some(code) = &self.exit_trap
                        && asked("EXIT")
                    {
                        sh_println!(self, "trap -- {} EXIT", crate::serialize::quote_literal(code));
                    }
                    let mut entries: Vec<(i32, TrapAction)> = self.traps.iter().map(|(k, v)| (*k, v.clone())).collect();
                    entries.sort_by_key(|(n, _)| *n);
                    for (n, action) in entries {
                        if !asked(&signal_name(n)) && !asked(&n.to_string()) {
                            continue;
                        }
                        match action {
                            TrapAction::Run(code) => {
                                sh_println!(self, "trap -- {} SIG{}", crate::serialize::quote_literal(&code), signal_name(n))
                            }
                            TrapAction::Ignore => sh_println!(self, "trap -- '' SIG{}", signal_name(n)),
                        }
                    }
                    for (label, code) in [("DEBUG", self.debug_trap.clone()), ("ERR", self.err_trap.clone()), ("RETURN", self.return_trap.clone())] {
                        if let Some(code) = code
                            && asked(label)
                        {
                            sh_println!(self, "trap -- {} {}", crate::serialize::quote_literal(&code), label);
                        }
                    }
                    return ExecResult::Status(0);
                }
                if argv.len() < 3 {
                    return ExecResult::Status(0);
                }
                let cmd_str = argv[1].clone();
                // A signal this shell cannot name is the command
                // failing, not a line of output to scroll past.
                let mut status = 0;
                for sig in &argv[2..] {
                    if sig == "EXIT" || sig == "0" {
                        self.exit_trap = if cmd_str == "-" { None } else { Some(cmd_str.clone()) };
                        self.exit_trap_depth = self.subshell_depth;
                        continue;
                    }
                    // The pseudo-signals: not signals, fired by the
                    // interpreter itself. `-` clears one, same as for a
                    // real signal.
                    if let Some(which) = match sig.as_str() {
                        "DEBUG" => Some(PseudoTrap::Debug),
                        "ERR" => Some(PseudoTrap::Err),
                        "RETURN" => Some(PseudoTrap::Return),
                        _ => None,
                    } {
                        let depth = self.function_depth;
                        self.pseudo_trap_depth[which as usize] = depth;
                        let slot = match which {
                            PseudoTrap::Debug => &mut self.debug_trap,
                            PseudoTrap::Err => &mut self.err_trap,
                            PseudoTrap::Return => &mut self.return_trap,
                        };
                        *slot = if cmd_str == "-" { None } else { Some(cmd_str.clone()) };
                        continue;
                    }
                    // KILL and STOP first: they are real signals with
                    // real numbers, deliberately absent from
                    // SIGNAL_NAMES so that list can be trap's own
                    // answer to "may I catch this?" (see its doc
                    // comment). bash accepts and records a trap for
                    // them silently, and it simply never fires; saying
                    // so is more use than a status a script would have
                    // to have expected.
                    if crate::exec::UNCATCHABLE_SIGNALS.iter().any(|(n, num)| sig.strip_prefix("SIG").unwrap_or(sig) == *n || sig == &num.to_string())
                    {
                        sh_eprintln!(self, "bish: trap: {}: cannot trap", sig);
                        continue;
                    }
                    let num = match signal_number(sig) {
                        Some(n) => n,
                        None => {
                            sh_eprintln!(self, "bish: trap: {}: invalid signal specification", sig);
                            status = 1;
                            continue;
                        }
                    };
                    if cmd_str == "-" {
                        self.traps.remove(&num);
                        sigaction_raw(num, SIG_DFL);
                    } else if cmd_str.is_empty() {
                        self.traps.insert(num, TrapAction::Ignore);
                        sigaction_raw(num, SIG_IGN);
                    } else {
                        self.traps.insert(num, TrapAction::Run(cmd_str.clone()));
                        sigaction_raw(num, record_pending_signal as *const () as usize);
                    }
                }
                return ExecResult::Status(status);
            }
            "jobs" => return ExecResult::Status(crate::builtins::jobs::run_jobs(self, &argv[1..])),
            "disown" => return ExecResult::Status(crate::builtins::jobs::run_disown(self, &argv[1..])),
            "fg" => return crate::builtins::jobs::run_fg(self, &argv[1..]),
            "bg" => return ExecResult::Status(crate::builtins::jobs::run_bg(self, &argv[1..])),
            "wait" => return ExecResult::Status(crate::builtins::jobs::run_wait(self, &argv[1..])),
            "kill" => return ExecResult::Status(crate::builtins::jobs::run_kill(self, &argv[1..])),
            "getopts" => return crate::builtins::shell::run_getopts(self, &argv[1..]),
            "unset" => {
                let target = self.peek_stderr_target(&cmd.redirects);
                return ExecResult::Status(crate::builtins::vars::run_unset(self, &argv[1..], &target));
            }
            "set" => return ExecResult::Status(crate::builtins::shell::run_set(self, &argv[1..])),
            "declare" | "typeset" => {
                // array_literal_args is indexed into the *original* argv
                // (which still has argv[0] == "declare"/"typeset"), so
                // every position shifts back by one to line up with
                // argv[1..].
                let shifted: Vec<_> =
                    array_literal_args.iter().filter_map(|(p, n, m, i)| p.checked_sub(1).map(|p2| (p2, n.clone(), *m, i.clone()))).collect();
                return ExecResult::Status(crate::builtins::vars::run_declare(self, &name, &argv[1..], &shifted));
            }
            // `readonly` is `declare -r`, the same way `export` is
            // `declare -x` -- and for the same reason: run_declare is
            // what knows about array literals, so the standalone
            // version silently dropped the value of `readonly a=(1)`.
            // The position bookkeeping is identical; see `export`.
            "readonly" => {
                let mut declare_args = vec!["-r".to_string()];
                declare_args.extend(argv[1..].iter().cloned());
                return ExecResult::Status(crate::builtins::vars::run_declare(self, &name, &declare_args, array_literal_args));
            }
            // exec CMD [args...] replaces this process image entirely (no
            // fork, no return on success) -- exactly what real bash does,
            // and available here as safe std (CommandExt::exec wraps
            // execvp, distinct from the fork() this shell avoids).
            "exec" if !matches!(parse_exec_opts(&argv), ExecOpts::RedirectsOnly) => {
                let opts = match parse_exec_opts(&argv) {
                    ExecOpts::Run(o) => o,
                    ExecOpts::BadOption(msg) => {
                        sh_eprintln!(self, "bish: exec: {}", msg);
                        sh_eprintln!(self, "exec: usage: exec [-cl] [-a name] [command [argument ...]] [redirection ...]");
                        return ExecResult::Status(2);
                    }
                    // Excluded by the guard.
                    ExecOpts::RedirectsOnly => unreachable!(),
                };
                if self.opt_restricted {
                    sh_eprintln!(self, "bish: exec: restricted");
                    return ExecResult::Status(1);
                }
                // argv[0] as the new process will see it: `-a NAME`
                // wins, `-l` prefixes a dash (what a login shell looks
                // for), and otherwise it is the command word itself.
                let argv0 = {
                    let base = opts.arg0.clone().unwrap_or_else(|| argv[opts.first].clone());
                    if opts.login { format!("-{}", base) } else { base }
                };
                let (prog, rest) = (&argv[opts.first], &argv[opts.first + 1..]);
                let redirs = match self.resolve_redirects(cmd) {
                    Ok(r) => r,
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}", e);
                        return ExecResult::Status(1);
                    }
                };
                // Inside a converted foreground subshell/command-
                // substitution/proc-sub, or a window-pane's own virtual-
                // child Shell (subshell_depth > 0 -- see new_virtual_
                // child's own doc comment: it's incremented for either),
                // a literal execve would replace the one real process
                // every sibling session/construct shares, not just "this
                // subshell" -- a real, severe regression from the old
                // re-exec'd design, where this call really did only ever
                // replace that subshell's own separate child process.
                // Falls back to spawning CMD as a real, separate child
                // instead, matching what a genuine forked subshell's own
                // exec would actually replace; since real `exec` never
                // returns control to the rest of this script either way,
                // this unwinds via the same ExecResult::Exit run_in_
                // child_shell already uses to stop just this child
                // without touching the real process.
                if self.subshell_depth > 0 {
                    let mut command = self.command(prog);
                    command.arg0(&argv0);
                    if opts.clear_env {
                        command.env_clear();
                    }
                    command.args(rest);
                    command.current_dir(&self.cwd);
                    command.stdin(self.spawn_stdin_stdio());
                    command.stdout(self.spawn_stdout_stdio());
                    command.stderr(self.spawn_stderr_stdio());
                    apply_fd_redirects(&mut command, redirs.actions);
                    self.note_external_spawn();
                    let status = match command.status() {
                        Ok(status) => exit_code_from_status(status),
                        // Same "leave $? at whatever it already was"
                        // behavior as the real top-level case just below.
                        Err(e) => {
                            let text = match e.kind() == std::io::ErrorKind::NotFound {
                                true => "not found".to_string(),
                                false => os_message(&e),
                            };
                            sh_eprintln!(self, "bish: exec: {}: {}", prog, text);
                            self.last_status
                        }
                    };
                    self.run_exit_trap();
                    return ExecResult::Exit(status);
                }
                let mut command = self.command(prog);
                command.arg0(&argv0);
                if opts.clear_env {
                    command.env_clear();
                }
                command.args(rest);
                // The redirects can't go through apply_fd_redirects'
                // pre_exec hook -- CommandExt::exec below is a direct
                // execve of *this* process, no fork, so there is no
                // child to install a pre_exec closure into.
                if let Err(e) = apply_fds_to_self(redirs.actions) {
                    sh_eprintln!(self, "bish: exec: {}", e);
                    return ExecResult::Status(1);
                }
                let err = command.exec();
                // bash's own wording for this one is just "not found",
                // shorter than the message anywhere else.
                let text = match err.kind() == std::io::ErrorKind::NotFound {
                    true => "not found".to_string(),
                    false => os_message(&err),
                };
                sh_eprintln!(self, "bish: exec: {}: {}", prog, text);
                // A non-interactive shell exits immediately when exec
                // cannot run the command. An earlier comment here said
                // bash 5.0.17 left `$?` at whatever it already was;
                // bash 5.3 exits 127 for a name it could not find and
                // 126 for one it found and could not run, the same two
                // statuses an ordinary command word gets.
                let status = match err.kind() == std::io::ErrorKind::NotFound {
                    true => 127,
                    false => 126,
                };
                self.run_exit_trap();
                return ExecResult::Exit(status);
            }
            // Bare `exec` (no command word): persistently applies its
            // redirects -- numbered-fd (`exec 3>file`, `exec 3>&1`,
            // `exec 3>&-`) and plain fd 0/1/2 (`exec > file`,
            // `exec 2>>err.log`) alike -- to this shell's own process, so
            // subsequently spawned commands inherit them. Covers idioms
            // like `exec > logfile 2>&1` (whole-script output redirection)
            // and the `exec 3>&1 1>logfile; cmd; exec 1>&3 3>&-` fd-
            // juggling trick.
            "exec" => {
                let redirs = match self.resolve_redirects(cmd) {
                    Ok(r) => r,
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}", e);
                        return ExecResult::Status(1);
                    }
                };
                if let Err(e) = apply_fds_to_self(redirs.actions) {
                    sh_eprintln!(self, "bish: exec: {}", e);
                    return ExecResult::Status(1);
                }
                return ExecResult::Status(0);
            }
            _ => {}
        }

        if builtin_only {
            sh_eprintln!(self, "bish: builtin: {}: not a shell builtin", name);
            return ExecResult::Status(1);
        }

        if self.restrict_to_builtins {
            sh_eprintln!(self, "bish: {}: command mode only allows builtins -- use `command {}` to run it", name, name);
            return ExecResult::Status(127);
        }

        if self.check_restricted_command_name(&name) {
            return ExecResult::Status(1);
        }

        let redirs = match self.resolve_redirects(cmd) {
            Ok(r) => r,
            Err(e) => {
                sh_eprintln!(self, "bish: {}", e);
                return ExecResult::Status(1);
            }
        };

        let mut command = self.command(&name);
        command.args(&argv[1..]);
        command.current_dir(&self.cwd);
        for (k, mode, val) in &cmd.assigns {
            let v = self.expand_word(val);
            let v = match mode {
                AssignMode::Set => v,
                AssignMode::Append => self.appended_value(k, &v),
            };
            command.env(k, v);
        }
        // A job spawned while promoted, with no redirects of its own,
        // gets attached to a fresh pty instead of the real terminal's
        // inherited fds -- see Job::pty_master's doc comment. Without
        // this, its output would go straight onto whatever the real
        // screen happens to show, which the compositor's next redraw
        // (driven only by what's actually captured in the session's
        // grid) would just silently overwrite -- confirmed as a real,
        // easy-to-hit bug: *any* promoted foreground external command
        // (most commands -- echo/pwd/ls/... aren't builtins) had its
        // output vanish this way before this covered the foreground
        // case too, not just background. Every other case (not
        // promoted, or explicitly redirected) keeps today's inherited-
        // stdio behavior exactly -- including a *non*-promoted
        // foreground command, which gets M11's real-job-control
        // tcsetpgrp/waitpid_untraced treatment further below instead;
        // there's no compositor to interfere with there, so a pty would
        // just be unnecessary overhead.
        let use_pty = self.is_promoted() && redirs.actions.is_empty();

        if use_pty {
            if let Ok(p) = pty::open() {
                // Size the pty to match this session's actual on-screen
                // area *before* the child ever gets to query it -- see
                // Shell::pty_size's own doc comment for why an unset
                // pty otherwise renders a full-screen program tiny.
                let (rows, cols) = self.pty_size();
                let _ = p.resize(rows, cols);
                let slave_path = p.slave_path.clone();
                return match pty::spawn_attached(command, &slave_path) {
                    Ok(child) => {
                        let mut cmd_text = argv.join(" ");
                        for r in &cmd.redirects {
                            cmd_text.push(' ');
                            cmd_text.push_str(&crate::serialize::serialize_redirect(r));
                        }
                        if background {
                            self.push_job_with_pty(vec![child], cmd_text, Some(p.master));
                            ExecResult::Status(0)
                        } else {
                            // Foreground: bubble up exactly like a
                            // backgrounded-then-explicitly-`fg`'d pty job
                            // does (see ExecResult::Fg's doc comment) --
                            // repl.rs's drive_fg_job already handles
                            // rendering into the grid and forwarding
                            // stdin (Ctrl-C/Ctrl-Z included -- see
                            // FgJob::poll_untraced for why a Ctrl-Z here
                            // is correctly caught as a real stop, not
                            // silently ignored). `id: 0` is a throwaway
                            // placeholder: this job only needs a real
                            // job-table id if it ends up Stopped (see
                            // Shell::park_stopped_fg_job), not just to
                            // run and exit normally.
                            self.pending_fg = Some(Job {
                                id: 0,
                                pids: vec![child.id()],
                                children: vec![child],
                                cmd_text,
                                pty_master: Some(p.master),
                                // A foreground job: drive_fg_job reads its
                                // pty straight into the session's grid
                                // itself, so there's nothing for the
                                // background drain to do here.
                                sink_screen: None,
                                nonblocking: false,
                                pgid: None,
                                stopped: false,
                            });
                            ExecResult::Fg
                        }
                    }
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}: {}", name, e);
                        ExecResult::Status(127)
                    }
                };
            }
            // pty::open() failing (fd exhaustion, etc.) shouldn't stop the
            // job from running at all, just from being fg-renderable
            // later -- fall through to the ordinary inherited-stdio path.
        }

        // Reached only when at least one stream *was* redirected (that's
        // what ruled out the pty path above). Backgrounded, the ones that
        // weren't redirected still need somewhere to go that isn't the real
        // screen -- see run_compound_redirected's own identical handling
        // for why redirecting one stream says nothing about the others.
        let bg_pty = if background { self.background_pty() } else { None };
        let bg_slave = |pty: &Option<pty::Pty>| -> Option<Stdio> {
            let p = pty.as_ref()?;
            std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok().map(Stdio::from)
        };
        if background {
            command.stdin(bg_slave(&bg_pty).unwrap_or_else(Stdio::inherit));
            command.stdout(bg_slave(&bg_pty).unwrap_or_else(Stdio::inherit));
        } else {
            command.stdin(self.spawn_stdin_stdio());
            command.stdout(self.spawn_stdout_stdio());
        }
        command.stderr(bg_slave(&bg_pty).unwrap_or_else(|| self.spawn_stderr_stdio()));
        apply_fd_redirects(&mut command, redirs.actions);

        // Real job control (M11), gated on `set -m` like fg/bg already are
        // (see their own opt_monitor checks): give this single external
        // command its own process group, seeded from its own (eventual)
        // pid, so a signal the terminal driver generates -- Ctrl-C,
        // Ctrl-Z -- targets only it, never bish. Set from *both* sides
        // (here, in the child, before exec; and again from the parent
        // right after spawn() returns below) to avoid the classic job-
        // control race where either side might run first -- setpgid(pid,
        // pid) is idempotent, so redundantly calling it twice is safe.
        if self.opt_monitor {
            unsafe {
                command.pre_exec(|| {
                    setpgid(0, 0);
                    // bish ignores SIGTTIN/SIGTTOU for itself (see
                    // term::ignore_tty_signals), a disposition that
                    // survives exec() the same way SIGINT's does (see
                    // apply_fd_redirects' own comment on this) -- without
                    // resetting them back to default here, a child later
                    // moved to the background (`bg`) that tries to read
                    // the terminal would silently ignore SIGTTIN instead
                    // of correctly stopping.
                    sigaction_raw(crate::term::SIGTTIN, SIG_DFL);
                    sigaction_raw(crate::term::SIGTTOU, SIG_DFL);
                    Ok(())
                });
            }
        }

        self.note_external_spawn();
        match command.spawn() {
            Ok(child) => {
                let pid = child.id();
                if self.opt_monitor {
                    unsafe { setpgid(pid as i32, pid as i32) };
                }
                if background {
                    let mut cmd_text = argv.join(" ");
                    for r in &cmd.redirects {
                        cmd_text.push(' ');
                        cmd_text.push_str(&crate::serialize::serialize_redirect(r));
                    }
                    let pgid = if self.opt_monitor { Some(pid) } else { None };
                    self.push_job_full(vec![child], cmd_text, bg_pty.map(|p| p.master), pgid);
                    ExecResult::Status(0)
                } else if self.opt_monitor {
                    // Foreground, with real job control: hand the real
                    // terminal to the child, wait watching for it to stop
                    // (not just exit -- see waitpid_untraced), then
                    // reclaim the terminal for bish either way.
                    pty::tcsetpgrp(0, pid as i32).ok();
                    let outcome = waitpid_untraced(pid);
                    unsafe {
                        pty::tcsetpgrp(0, getpgrp()).ok();
                    }
                    self.drain_proc_subs();
                    match outcome {
                        JobWaitOutcome::Exited(status) => ExecResult::Status(status),
                        JobWaitOutcome::Stopped(_sig) => {
                            let mut cmd_text = argv.join(" ");
                            for r in &cmd.redirects {
                                cmd_text.push(' ');
                                cmd_text.push_str(&crate::serialize::serialize_redirect(r));
                            }
                            let id = {
                                let mut table = self.jobs.borrow_mut();
                                let id = table.next_job_id;
                                table.next_job_id += 1;
                                table.jobs.push(Job {
                                    id,
                                    pids: vec![pid],
                                    children: vec![child],
                                    cmd_text: cmd_text.clone(),
                                    pty_master: None,
                                    sink_screen: None,
                                    nonblocking: false,
                                    pgid: Some(pid),
                                    stopped: true,
                                });
                                id
                            };
                            // Bash convention: $? = 128+signum for a
                            // foreground job the shell caught stopping,
                            // same family as a signal-killed exit status.
                            sh_println!(self, "\n[{}]+  Stopped                 {}", id, cmd_text);
                            ExecResult::Status(148)
                        }
                    }
                } else {
                    let mut child = child;
                    let result = match self.wait_pumping(&mut child) {
                        Ok(status) => ExecResult::Status(exit_code_from_status(status)),
                        Err(e) => {
                            sh_eprintln!(self, "bish: {}", e);
                            ExecResult::Status(1)
                        }
                    };
                    self.drain_proc_subs();
                    result
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.has_command_not_found_handler() => {
                self.drain_proc_subs();
                self.run_command_not_found_handler(&argv)
            }
            Err(e) => {
                // A name that is nearly a builtin is worth saying so
                // about. Only for NotFound: an EACCES on a real file is
                // a different problem and a suggestion there would be
                // noise on top of it.
                let not_found = e.kind() == std::io::ErrorKind::NotFound;
                let hint = match not_found {
                    // A bash builtin this shell deliberately answers
                    // differently gets pointed at its equivalent; every
                    // other unknown name gets the nearest builtin.
                    true => match crate::suggest::instead_of(&name) {
                        advice if !advice.is_empty() => advice,
                        _ => crate::suggest::did_you_mean(&name, KNOWN_BUILTINS.iter().copied()),
                    },
                    false => String::new(),
                };
                // bash's three answers, and its two statuses: a bare
                // name nothing in PATH matched is "command not found";
                // a path that is not there says so; and a path that
                // *is* there but cannot be run is 126, not 127 -- the
                // distinction a script checks to tell "no such tool"
                // from "found it, could not run it".
                let (text, status) = match (not_found, name.contains('/')) {
                    (true, false) => ("command not found".to_string(), 127),
                    (true, true) => (os_message(&e), 127),
                    // execve on a directory reports EACCES on Linux,
                    // which is true and unhelpful; bash stats it and
                    // says what it actually is.
                    (false, _) if std::path::Path::new(&name).is_dir() => ("Is a directory".to_string(), 126),
                    (false, _) => (os_message(&e), 126),
                };
                let msg = format!("bish: {}: {}{}", name, text, hint);
                self.write_command_error(cmd, &msg);
                self.drain_proc_subs();
                ExecResult::Status(status)
            }
        }
    }

    // Every pipeline stage is a separate process by necessity (that's what
    // makes piping possible at all), so compound-command stages self-exec
    // just like Subshell already does -- this is actually the *correct*
    // bash semantics too: piped stages always fork, even in real bash.
    /// Whether this pipeline stage needs a shell to run it at all.
    ///
    /// Decided syntactically, without expanding anything: expansion has
    /// side effects (a `$( )` in a stage's own words runs a command),
    /// and doing it here would move those side effects earlier than the
    /// stage they belong to. A first word that is a plain literal can be
    /// checked against the builtin and function tables directly;
    /// anything else -- a variable, a substitution, a glob -- is assumed
    /// to need one, which is the safe direction to be wrong in, since
    /// running an external command through a shell still works and the
    /// reverse does not.
    fn stage_needs_interpreter(&self, cmd: &parser::Command) -> bool {
        let parser::Command::Simple(sc) = cmd else { return true };
        let Some(first) = sc.words.first() else { return true };
        match first.chunks.as_slice() {
            // A *quoted* name is the same name: `'wc' -l` runs wc, and
            // quoting a builtin's own name does not stop it being
            // recognised either (see run_single's first_word_literal,
            // which has always matched both). Reading only the unquoted
            // form meant a quoted external was classed as needing the
            // interpreter and run as a coroutine stage -- and a
            // pipeline of two such stages, one of them spawning a real
            // process on the far end of its own pipe, hangs.
            //
            // Which is not a hypothetical: the serializer quotes every
            // word, so this is the shape a re-exec'd construct arrives
            // in.
            [crate::lexer::Chunk::Str(name)] | [crate::lexer::Chunk::LiteralStr(name)] => {
                self.is_active_builtin(name) || self.functions.contains_key(name)
            }
            _ => true,
        }
    }

    /// A pipeline with two or more stages that need a shell, run without
    /// a single extra process.
    ///
    /// One shell stage can simply run here, between spawning the others
    /// and waiting for them -- the real processes on the far ends of its
    /// pipes are what keep it from deadlocking. Two cannot: each would
    /// block on a pipe the other is on the far end of, and there is only
    /// one thread. So they run as coroutines, and the scheduler hands
    /// the thread to whichever one can move (see `scheduler`).
    ///
    /// Stages that are real commands are still real processes. They need
    /// no help: the kernel runs them.
    fn run_multi_scheduled(&mut self, commands: &[parser::Command]) -> i32 {
        let n = commands.len();
        let is_shell_stage: Vec<bool> = commands.iter().map(|c| self.stage_needs_interpreter(c)).collect();

        // Pipe `i` carries stage `i`'s output to stage `i + 1`.
        let mut read_ends: Vec<Option<std::os::fd::OwnedFd>> = (0..n).map(|_| None).collect();
        let mut write_ends: Vec<Option<std::os::fd::OwnedFd>> = (0..n).map(|_| None).collect();
        for i in 0..n.saturating_sub(1) {
            match make_pipe() {
                Ok((read_end, write_end)) => {
                    // Left blocking. A stage must not *actually* block
                    // in the kernel -- it would take the thread and
                    // every other stage with it -- but the flag that
                    // prevents that is set only around this shell's own
                    // reads and writes, by `briefly_nonblocking`, so
                    // that a child spawned by a stage never inherits it.
                    write_ends[i] = Some(write_end);
                    read_ends[i + 1] = Some(read_end);
                }
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    return 1;
                }
            }
        }

        let mut scheduler = crate::scheduler::Scheduler::new();
        let mut children: Vec<(usize, std::process::Child)> = Vec::new();
        let mut codes: Vec<Rc<std::cell::Cell<i32>>> = (0..n).map(|_| Rc::new(std::cell::Cell::new(0))).collect();

        for (i, cmd) in commands.iter().enumerate() {
            let stdin_end = read_ends[i].take();
            let stdout_end = write_ends[i].take();
            if !is_shell_stage[i] {
                let parser::Command::Simple(sc) = cmd else { unreachable!("a non-simple command always needs a shell") };
                let argv: Vec<String> = self.expand_words(&sc.words);
                if argv.is_empty() {
                    continue;
                }
                let mut command = self.command(&argv[0]);
                command.args(&argv[1..]);
                for (k, mode, val) in &sc.assigns {
                    let v = self.expand_word(val);
                    let v = match mode {
                        AssignMode::Set => v,
                        AssignMode::Append => self.appended_value(k, &v),
                    };
                    command.env(k, v);
                }
                command.stdin(match stdin_end {
                    Some(fd) => Stdio::from(fd),
                    None => self.spawn_stdin_stdio(),
                });
                command.stdout(match stdout_end {
                    Some(fd) => Stdio::from(fd),
                    None => self.spawn_stdout_stdio(),
                });
                if i == n - 1 {
                    self.note_external_spawn();
                }
                match command.spawn() {
                    Ok(child) => children.push((i, child)),
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}", e);
                        kill_all(children);
                        return 127;
                    }
                }
                continue;
            }

            // A shell stage. Built and connected now, run later: every
            // stage has to exist before any of them starts, because each
            // blocks on a pipe another is on the far end of.
            //
            // An ordinary virtual child with no redirect of its own:
            // its pipes arrive as the real fd 0 and fd 1, installed by
            // the scheduler for the duration of each of its turns. That
            // is what a stage running as its own process gets, and it
            // is what keeps `exec {fd}<&0`, an external command
            // inheriting the rest of the input, and a builtin writing
            // to fd 1 all behaving the way they already did.
            let mut child = self.new_virtual_child();
            let body = cmd.clone();
            let code = Rc::clone(&codes[i]);
            let stage = move || {
                // Armed here rather than by the caller: the watch is
                // swapped per task by the scheduler, so this is the
                // first moment this stage's own is the thread's.
                arm_broken_pipe();
                let result = crate::builtins::shell::run_command(&mut child, &body, false);
                if !matches!(result, ExecResult::Exit(_)) {
                    child.run_exit_trap();
                }
                // A stage whose reader left is dead the same way a
                // separate process would have been, with the status
                // SIGPIPE would have given it.
                code.set(if disarm_broken_pipe() { 128 + 13 } else { result.status() });
            };
            // Pipeline stages are run to completion by `Scheduler::run`
            // before anything else happens, so cancellation never
            // reaches them either way.
            if let Err(e) = scheduler.add_with_fds(stage, stdin_end, stdout_end, true) {
                sh_eprintln!(self, "bish: {}", e);
                kill_all(children);
                return 1;
            }
        }
        // Every end this shell still holds is one the reader downstream
        // would wait on for ever.
        drop(read_ends);
        drop(write_ends);

        scheduler.run();

        let mut statuses: Vec<i32> = codes.drain(..).map(|c| c.get()).collect();
        for (stage, mut child) in children {
            statuses[stage] = match child.wait() {
                Ok(s) => exit_code_from_status(s),
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    1
                }
            };
        }
        self.set_pipestatus(&statuses);
        let last = statuses.last().copied().unwrap_or(0);
        if self.opt_pipefail { statuses.iter().rev().find(|c| **c != 0).copied().unwrap_or(0) } else { last }
    }

    fn run_multi(&mut self, commands: &[parser::Command], background: bool) -> i32 {
        let n = commands.len();
        // Paired with the stage each came from: `PIPESTATUS` is in
        // stage order, and with a stage running in this shell rather
        // than as a child the two orders stop being the same.
        let mut children: Vec<(usize, std::process::Child)> = Vec::with_capacity(n);
        // The previous stage's read end, kept as a `ChildStdout` rather
        // than a `Stdio`: `lastpipe` needs its fd, and only ChildStdout
        // still has one.
        let mut prev_stdout: Option<std::os::fd::OwnedFd> = None;
        // Real job control isolation (see run_single's own identical
        // pre_exec/setpgid pattern) for a *backgrounded* pipeline only --
        // every stage joins the same process group, seeded from the
        // first stage's own (eventual) pid, once it's known (`None`
        // means "not seeded yet," i.e. this is that first stage). `kill
        // %N`/`bg`'s own SIGCONT-to-process-group then reaches every
        // stage at once, same as a real shell's own pipeline job.
        // Deliberately gated on `background` too, not just opt_monitor
        // (unlike run_single, which isolates a foreground command as
        // well before separately reassigning the terminal's own
        // foreground group to it): a *foreground* pipeline isn't given
        // any of that tcsetpgrp/stop-handling machinery here, so
        // isolating it into its own group without also doing that would
        // just stop a foreground Ctrl-C from ever reaching it -- a
        // regression today's "no isolation at all, inherits bish's own
        // group" behavior doesn't have.
        let mut pgid: Option<i32> = None;

        // `shopt -s lastpipe`: the *last* stage runs in this shell
        // rather than in a child, so `seq 3 | while read l; do n=$l;
        // done` leaves `n` set. bash gates it on job control being off
        // (an interactive shell has to be able to name the stage as a
        // job) and on the pipeline being in the foreground; this adds
        // one more gate, that stdin is not already redirected by an
        // enclosing converted construct, since the stage's stdin here
        // is installed by dup2 onto the real fd 0.
        let lastpipe = n > 1 && !background && !self.opt_monitor && self.stdio_override.is_none() && self.shopt_is_on("lastpipe");

        // A backgrounded pipeline's own pty (see background_pty): the
        // first stage reads from it, the last writes to it, and every
        // stage's stderr goes there too -- inherited, any of that would
        // land straight on the real screen for the compositor to wipe.
        // Wired as plain fds rather than via spawn_attached, whose setsid
        // would undo the process-group isolation set up just below (and
        // which `kill %N`/`bg` need to reach every stage at once).
        //
        // The one stage that can run in this shell instead of as a
        // separate process.
        //
        // A pipeline needs its stages to run *concurrently*, and that is
        // the only reason each one is a process today. But when exactly
        // one stage needs a shell, every other stage is a real command
        // already running on the other end of a pipe -- so the
        // concurrency is there, provided by them, and the shell stage
        // can simply run here, after they are spawned and before they
        // are waited for. It reads a pipe a real process is filling and
        // writes one a real process is draining; neither can deadlock,
        // because the other end is never this thread.
        //
        // Two or more shell stages would need two interpreters running
        // at once, which is a different problem, and those still
        // re-exec. `lastpipe` already claims the in-shell slot where it
        // applies, and only one stage can have it: a second would run
        // sequentially with the first, and whichever ran first would
        // fill its pipe with nothing draining it.
        // Two or more stages that need a shell: they cannot take turns
        // by running one here between the others, because each would be
        // waiting on a pipe the other is on the far end of. Those go to
        // the scheduler, which is the only path that needs coroutines
        // at all.
        if !background && !lastpipe && n >= 2 && commands.iter().filter(|c| self.stage_needs_interpreter(c)).count() >= 2 {
            return self.run_multi_scheduled(commands);
        }
        let inproc_stage = if background || n < 2 || lastpipe {
            None
        } else {
            let mut shell_stages = commands.iter().enumerate().filter(|(_, c)| self.stage_needs_interpreter(c)).map(|(i, _)| i);
            match (shell_stages.next(), shell_stages.next()) {
                (Some(only), None) => Some(only),
                _ => None,
            }
        };
        // Its stdin and stdout, once the loop below has worked out what
        // they are.
        let mut inproc_stdio: Option<(Option<std::os::fd::OwnedFd>, Option<std::os::fd::OwnedFd>)> = None;

        let bg_pty = if background { self.background_pty() } else { None };
        let bg_slave = |pty: &Option<pty::Pty>| -> Option<Stdio> {
            let p = pty.as_ref()?;
            std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok().map(Stdio::from)
        };

        for (i, cmd) in commands.iter().enumerate() {
            let is_last = i == n - 1;
            if is_last && lastpipe {
                break;
            }
            if Some(i) == inproc_stage {
                // Not spawned: its fds are recorded here and it runs
                // below, once every other stage is already going.
                let stdin_fd = prev_stdout.take();
                let stdout_fd = if is_last {
                    None
                } else {
                    match make_pipe() {
                        Ok((read_end, write_end)) => {
                            prev_stdout = Some(read_end);
                            Some(write_end)
                        }
                        Err(e) => {
                            sh_eprintln!(self, "bish: {}", e);
                            kill_all(children);
                            return 1;
                        }
                    }
                };
                inproc_stdio = Some((stdin_fd, stdout_fd));
                continue;
            }
            // Only the first stage takes the pty as stdin -- every other
            // one reads the previous stage's pipe, exactly as before.
            let default_stdin = match prev_stdout.take() {
                Some(prev) => Stdio::from(prev),
                None => bg_slave(&bg_pty).unwrap_or_else(|| self.spawn_stdin_stdio()),
            };
            let default_stdout = if is_last { bg_slave(&bg_pty).unwrap_or_else(|| self.spawn_stdout_stdio()) } else { Stdio::piped() };
            let mut default_stderr = bg_slave(&bg_pty);

            let mut command = match cmd {
                parser::Command::Simple(sc) => {
                    if sc.words.is_empty() {
                        sh_eprintln!(self, "bish: syntax error in pipeline");
                        kill_all(children);
                        return 1;
                    }
                    let argv: Vec<String> = self.expand_words(&sc.words);
                    if argv.is_empty() {
                        continue;
                    }
                    let redirs = match self.resolve_redirects(sc) {
                        Ok(r) => r,
                        Err(e) => {
                            sh_eprintln!(self, "bish: {}", e);
                            kill_all(children);
                            return 1;
                        }
                    };
                    // Builtins and shell functions aren't real executables,
                    // so they can't be Command::new'd directly. Route them
                    // through the same self-exec trick used for compound
                    // commands below, replaying the *already-expanded*
                    // argv as single-quoted literals so the child doesn't
                    // re-run any command substitution/glob/etc a second
                    // time.
                    let mut command = if self.is_active_builtin(&argv[0]) || self.functions.contains_key(&argv[0]) {
                        let exe = match std::env::current_exe() {
                            Ok(p) => p,
                            Err(e) => {
                                sh_eprintln!(self, "bish: {}", e);
                                kill_all(children);
                                return 1;
                            }
                        };
                        let script_line: String = argv.iter().map(|a| crate::serialize::quote_literal(a)).collect::<Vec<_>>().join(" ");
                        let script = self.functions_preamble() + &script_line;
                        let mut command = self.command(exe);
                        command.arg("-c").arg(script);
                        command
                    } else {
                        let mut command = self.command(&argv[0]);
                        command.args(&argv[1..]);
                        command
                    };
                    for (k, mode, val) in &sc.assigns {
                        let v = self.expand_word(val);
                        let v = match mode {
                            AssignMode::Set => v,
                            AssignMode::Append => self.appended_value(k, &v),
                        };
                        command.env(k, v);
                    }
                    command.stdin(default_stdin);
                    command.stdout(default_stdout);
                    command.stderr(default_stderr.take().unwrap_or_else(|| self.spawn_stderr_stdio()));
                    apply_fd_redirects(&mut command, redirs.actions);
                    command
                }
                other => {
                    let exe = match std::env::current_exe() {
                        Ok(p) => p,
                        Err(e) => {
                            sh_eprintln!(self, "bish: {}", e);
                            kill_all(children);
                            return 1;
                        }
                    };
                    let own_redirects = command_own_redirects(other);
                    let redirs = if own_redirects.is_empty() {
                        ResolvedRedirs { actions: Vec::new() }
                    } else {
                        match self.resolve_redirect_list(own_redirects) {
                            Ok(r) => r,
                            Err(e) => {
                                sh_eprintln!(self, "bish: {}", e);
                                kill_all(children);
                                return 1;
                            }
                        }
                    };
                    let script = self.functions_preamble() + &crate::serialize::serialize_command(other);
                    let mut command = self.command(exe);
                    command.arg("-c").arg(script);
                    command.stdin(default_stdin);
                    command.stdout(default_stdout);
                    command.stderr(default_stderr.take().unwrap_or_else(|| self.spawn_stderr_stdio()));
                    apply_fd_redirects(&mut command, redirs.actions);
                    command
                }
            };
            command.current_dir(&self.cwd);

            // Set from *both* sides (here, in the child; and again from
            // the parent right after spawn() returns below) to avoid the
            // classic job-control race -- same reasoning/idempotency as
            // run_single's own identical pre_exec hook.
            if background && self.opt_monitor {
                let join_pgid = pgid;
                unsafe {
                    command.pre_exec(move || {
                        setpgid(0, join_pgid.unwrap_or(0));
                        sigaction_raw(crate::term::SIGTTIN, SIG_DFL);
                        sigaction_raw(crate::term::SIGTTOU, SIG_DFL);
                        Ok(())
                    });
                }
            }

            // Every pipeline stage is a genuinely separate process (even
            // a builtin/function stage runs as a re-exec'd bish -c
            // script -- see the `other` arm above) whose own stdout, for
            // the last stage, is inherited straight from the real
            // terminal -- see ran_external_since_prompt's own doc
            // comment.
            if is_last {
                self.note_external_spawn();
            }
            match command.spawn() {
                Ok(mut child) => {
                    if background && self.opt_monitor {
                        let cpid = child.id() as i32;
                        unsafe { setpgid(cpid, pgid.unwrap_or(cpid)) };
                        if pgid.is_none() {
                            pgid = Some(cpid);
                        }
                    }
                    if !is_last {
                        prev_stdout = child.stdout.take().map(std::os::fd::OwnedFd::from);
                    }
                    children.push((i, child));
                }
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    kill_all(children);
                    return 127;
                }
            }
        }

        if background {
            let cmd_text = commands.iter().map(crate::serialize::serialize_command).collect::<Vec<_>>().join(" | ");
            // Both the pgid (so `kill %N`/`bg` reach every stage at once)
            // and the output pty, which push_job_full needs to record for
            // the drain to find later.
            let children = children.into_iter().map(|(_, c)| c).collect();
            self.push_job_full(children, cmd_text, bg_pty.map(|p| p.master), pgid.map(|p| p as u32));
            return 0;
        }

        // The last stage, in this shell, reading the pipe the others
        // are writing to. Run *before* waiting on them: they are
        // blocked on a full pipe until something drains it.
        let mut lastpipe_code = None;
        if lastpipe {
            unsafe extern "C" {
                fn dup2(oldfd: i32, newfd: i32) -> i32;
            }
            let saved = save_fd012();
            if let Some(read_end) = prev_stdout.take() {
                let fd = read_end;
                unsafe {
                    dup2(std::os::fd::AsRawFd::as_raw_fd(&fd), 0);
                }
                let result = crate::builtins::shell::run_command(self, &commands[n - 1], false);
                lastpipe_code = Some(match result {
                    ExecResult::Exit(code) => {
                        self.pending_exit = Some(code);
                        code
                    }
                    other => other.status(),
                });
            }
            restore_fd012(saved);
        }

        // The stage that runs here rather than as a child -- after
        // every other stage is spawned, and before any of them is
        // waited for. They are what drains the pipe it writes and fills
        // the pipe it reads, so they have to be running while it does.
        let mut inproc_code = None;
        if let (Some(stage), Some((stdin_fd, stdout_fd))) = (inproc_stage, inproc_stdio.take()) {
            unsafe extern "C" {
                fn dup2(oldfd: i32, newfd: i32) -> i32;
            }
            use std::os::fd::AsRawFd;
            let saved = save_fd012();
            if let Some(fd) = &stdin_fd {
                unsafe { dup2(fd.as_raw_fd(), 0) };
            }
            if let Some(fd) = &stdout_fd {
                unsafe { dup2(fd.as_raw_fd(), 1) };
            }
            // A virtual child rather than `run_command` directly: every
            // stage of a pipeline is a subshell in bash, so an
            // assignment here must not escape. That is exactly what this
            // already gives `( )`, and the one thing `lastpipe` above
            // deliberately does not do.
            // Armed only around this: an `EPIPE` anywhere else is
            // still ignored, as it always was.
            arm_broken_pipe();
            let result = self.run_command_in_child_shell(&commands[stage], ChildStdio::default());
            let broken = disarm_broken_pipe();
            restore_fd012(saved);
            // Dropped before anything is waited for: the stage
            // downstream sees end-of-input when the last copy of this
            // pipe's write end closes, and this shell is holding one.
            drop(stdout_fd);
            drop(stdin_fd);
            // No `pending_exit`: this stage is a subshell, so its own
            // `exit` ends the stage rather than the shell running it --
            // which is what `run_command_in_child_shell` has already
            // turned the result into. That is the one thing `lastpipe`
            // above must do differently, since its stage is not a
            // subshell at all.
            inproc_code = Some(if broken { 128 + 13 } else { result.status() });
        }

        // By stage rather than in completion order: `PIPESTATUS` is
        // positional, and a stage that ran in this shell never went
        // through `children` at all.
        let mut codes: Vec<Option<i32>> = vec![None; n];
        for (stage, mut c) in children {
            let code = match c.wait() {
                Ok(s) => exit_code_from_status(s),
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    1
                }
            };
            codes[stage] = Some(code);
        }
        if let (Some(stage), Some(code)) = (inproc_stage, inproc_code) {
            codes[stage] = Some(code);
        }
        if let Some(code) = lastpipe_code {
            codes[n - 1] = Some(code);
        }
        let codes: Vec<i32> = codes.into_iter().map(|c| c.unwrap_or(0)).collect();
        self.set_pipestatus(&codes);
        let status = codes.last().copied().unwrap_or(0);
        if self.opt_pipefail { codes.iter().rev().find(|c| **c != 0).copied().unwrap_or(0) } else { status }
    }

    fn expand_word(&mut self, w: &Word) -> String {
        let mut s = String::new();
        for c in &w.chunks {
            match c {
                Chunk::Tilde { name } => {
                    let name = name.clone();
                    s.push_str(&self.expand_tilde(&name));
                }
                Chunk::Str(t) | Chunk::LiteralStr(t) => s.push_str(t),
                Chunk::Var { name, .. } => {
                    let name = name.clone();
                    self.check_nounset(&name);
                    s.push_str(&self.lookup_var(&name));
                }
                Chunk::Sub { raw, .. } => s.push_str(&self.run_command_substitution(raw)),
                Chunk::Arith { raw, .. } => match self.eval_arith(raw) {
                    Ok(v) => s.push_str(&v.to_string()),
                    Err(e) => {
                        sh_eprintln!(self, "bish: (({})): {}", raw, e);
                        // Fatal, unlike the `(( ))` *command* -- see
                        // expansion_failed.
                        self.expansion_failed = true;
                    }
                },
                Chunk::VarExpand { name, op, .. } => {
                    let name = name.clone();
                    let op = op.clone();
                    match self.list_slice(&name, None, &op) {
                        Some(sliced) => {
                            let joined = self.joined_slice(sliced);
                            s.push_str(&joined);
                        }
                        None => s.push_str(&self.eval_var_op(&name, &op)),
                    }
                }
                Chunk::ArrayVar { name, index, .. } => {
                    let name = name.clone();
                    let index = index.clone();
                    s.push_str(&self.array_element(&name, &index));
                }
                Chunk::ArrayLength { name, index } => {
                    let name = name.clone();
                    let index = index.clone();
                    s.push_str(&self.array_length(&name, &index).to_string());
                }
                Chunk::ArrayVarExpand { name, index, op, .. } => {
                    let name = name.clone();
                    let index = index.clone();
                    let op = op.clone();
                    match self.list_slice(&name, Some(&index), &op) {
                        Some(sliced) => {
                            let joined = self.joined_slice(sliced);
                            s.push_str(&joined);
                        }
                        None => s.push_str(&self.eval_array_var_op(&name, &index, &op)),
                    }
                }
                Chunk::Indirect { name, .. } => {
                    let v = self.indirect_var(name);
                    s.push_str(&v);
                }
                Chunk::ArrayKeys { name, .. } => {
                    let name = name.clone();
                    let sep = self.ifs_join_char();
                    let keys = self.array_keys(&name).join(&sep);
                    s.push_str(&keys);
                }
                Chunk::VarNamesMatchingPrefix { prefix, .. } => {
                    let prefix = prefix.clone();
                    let sep = self.ifs_join_char();
                    let names = self.var_names_with_prefix(&prefix).join(&sep);
                    s.push_str(&names);
                }
                Chunk::ProcSubIn { raw } => {
                    let raw = raw.clone();
                    s.push_str(&self.run_proc_sub_in(&raw));
                }
                Chunk::ProcSubOut { raw } => {
                    let raw = raw.clone();
                    s.push_str(&self.run_proc_sub_out(&raw));
                }
            }
        }
        s
    }

    // index "@"/"*" joins all elements (in index order) with a space (used
    // outside the splitting-aware path, where "@" vs "*" can't be
    // distinguished anyway); any other index is evaluated as an arithmetic
    // expression (so `${arr[i+1]}` works) and looked up 0-based. A gap
    // index (never set) reads back as empty, same as an unset scalar.
    // Associative-array indices are literal (expandable) strings, not
    // arithmetic expressions -- re-lex the raw index text and expand it,
    // same as expand_raw (an index like `with space` re-lexes as more than
    // one Word token; taking only the first would silently truncate it).
    pub(crate) fn expand_index_as_string(&mut self, index: &str) -> String {
        self.expand_raw(index)
    }

    // Negative indices count back from the end: bash defines them as
    // relative to one greater than the array's maximum set index, so -1 is
    // the last (highest-index) element. Only meaningful for indexed arrays
    // -- associative-array indices are plain string keys, never resolved
    // here.
    pub(crate) fn resolve_array_index(&self, name: &str, i: i64) -> Option<usize> {
        if i >= 0 {
            return Some(i as usize);
        }
        let max = *self.arrays.get(name)?.keys().next_back()?;
        let resolved = max as i64 + 1 + i;
        if resolved >= 0 { Some(resolved as usize) } else { None }
    }

    // Every array accessor starts here: `declare -n r=m` makes `r` a
    // name for `m`, and `${r[k]}` has to reach `m`'s element the same
    // way `$r` reaches its value. Scalars were already redirected (see
    // lookup_var/assign_var); arrays were not, so a nameref to one
    // silently read as empty.
    pub(crate) fn array_target(&self, name: &str) -> String {
        if self.nameref_names.contains(name) { self.resolve_nameref(name) } else { name.to_string() }
    }

    fn array_element(&mut self, name: &str, index: &str) -> String {
        let name = &self.array_target(name);
        if index == "@" || index == "*" {
            let sep = self.ifs_join_char();
            return self.array_all(name).join(&sep);
        }
        if self.assoc_names.contains(name) {
            let key = self.expand_index_as_string(index);
            return self.assoc_arrays.get(name).and_then(|m| m.get(&key)).cloned().unwrap_or_default();
        }
        match arith::eval(index, self) {
            Ok(i) => match self.resolve_array_index(name, i) {
                Some(idx) => self.arrays.get(name).and_then(|m| m.get(&idx)).cloned().unwrap_or_default(),
                None => String::new(),
            },
            Err(_) => String::new(),
        }
    }

    // Array-element analog of var_is_set: is this specific index actually
    // present in the (sparse) array, not just reading back empty because
    // it was never set.
    fn array_element_is_set(&mut self, name: &str, index: &str) -> bool {
        let name = &self.array_target(name);
        if index == "@" || index == "*" {
            return !self.array_all(name).is_empty();
        }
        if self.assoc_names.contains(name) {
            let key = self.expand_index_as_string(index);
            return self.assoc_arrays.get(name).is_some_and(|m| m.contains_key(&key));
        }
        match arith::eval(index, self) {
            Ok(i) => match self.resolve_array_index(name, i) {
                Some(idx) => self.arrays.get(name).is_some_and(|m| m.contains_key(&idx)),
                None => false,
            },
            Err(_) => false,
        }
    }

    fn array_keys(&self, name: &str) -> Vec<String> {
        let name = &self.array_target(name);
        if let Some(m) = self.assoc_arrays.get(name) {
            return m.keys().cloned().collect();
        }
        self.arrays.get(name).map(|m| m.keys().map(|k| k.to_string()).collect()).unwrap_or_default()
    }

    fn array_all(&self, name: &str) -> Vec<String> {
        let name = &self.array_target(name);
        if let Some(m) = self.assoc_arrays.get(name) {
            return m.values().cloned().collect();
        }
        self.arrays.get(name).map(|m| m.values().cloned().collect()).unwrap_or_default()
    }

    // "@"/"*" counts only set elements (real bash arrays are sparse --
    // `arr[10]=x` alone gives a length of 1, not 11).
    fn array_length(&mut self, name: &str, index: &str) -> usize {
        let name = &self.array_target(name);
        if index == "@" || index == "*" {
            if let Some(m) = self.assoc_arrays.get(name) {
                return m.len();
            }
            return self.arrays.get(name).map(|m| m.len()).unwrap_or(0);
        }
        if self.assoc_names.contains(name) {
            let key = self.expand_index_as_string(index);
            return self.assoc_arrays.get(name).and_then(|m| m.get(&key)).map(|s| s.chars().count()).unwrap_or(0);
        }
        match arith::eval(index, self) {
            Ok(i) => match self.resolve_array_index(name, i) {
                Some(idx) => self.arrays.get(name).and_then(|m| m.get(&idx)).map(|s| s.chars().count()).unwrap_or(0),
                None => 0,
            },
            Err(_) => 0,
        }
    }

    // `arr[i]=value`. Sets exactly that index -- no resizing/filling, since
    // the array is a sparse map, matching bash (gaps stay genuinely unset).
    // Returns the concrete indexed-array index actually written (None for
    // an associative array, or on a bad-index error) -- used by
    // apply_array_literal to know where a literal's own running
    // "next index" counter should resume after an explicit `[i]=value`
    // element.
    // `declare 'a[0]=5'` reaches the same element assignment a bare
    // `a[0]=5` does -- see run_declare, which is outside this module.
    pub(crate) fn array_set_index_public(&mut self, name: &str, index: &str, value: String) {
        self.array_set_index(name, index, value);
    }

    fn array_set_index(&mut self, name: &str, index: &str, value: String) -> Option<usize> {
        let name = &self.array_target(name);
        if self.name_is_readonly(name) {
            sh_eprintln!(self, "bish: {}: readonly variable", name);
            return None;
        }
        if self.assoc_names.contains(name) {
            let key = self.expand_index_as_string(index);
            self.assoc_arrays.entry(name.to_string()).or_default().insert(key, value);
            return None;
        }
        let i = match arith::eval(index, self) {
            Ok(i) => match self.resolve_array_index(name, i) {
                Some(idx) => idx,
                None => {
                    sh_eprintln!(self, "bish: {}: bad array index: {}", name, index);
                    return None;
                }
            },
            Err(_) => {
                sh_eprintln!(self, "bish: {}: bad array index: {}", name, index);
                return None;
            }
        };
        self.arrays.entry(name.to_string()).or_default().insert(i, value);
        Some(i)
    }

    // Applies one `name=(...)`/`name+=(...)` array literal's elements to
    // self.arrays/self.assoc_arrays (whichever `name` already is -- an
    // indexed array unless self.assoc_names already contains it,
    // matching how a bracketed key is interpreted the same way
    // array_set_index already decides). Honors per-element `[index]=
    // value` syntax alongside plain positional elements: a positional
    // element continues from a running "next index" counter that a
    // keyed element ahead of it bumps forward, matching bash (`(1
    // [5]=x 2)` -> indices 0, 5, 6). Only meaningful for an indexed
    // array -- an associative one has no positional-index concept at
    // all, so a plain (unkeyed) element there is keyed by its own
    // running counter's string form as a last resort (real bash errors
    // on this instead; rare enough in practice that erroring isn't
    // worth the extra plumbing here).
    pub(crate) fn apply_array_literal(&mut self, name: &str, mode: AssignMode, items: &[ArrayLiteralItem]) -> bool {
        // Same guard, and the same message, as a scalar write -- see
        // assign_var_impl. Without it `readonly a=(1); a+=(2)` went
        // through, which is the one thing `readonly` promises not to.
        if self.name_is_readonly(name) {
            sh_eprintln!(self, "bish: {}: readonly variable", name);
            return false;
        }
        let is_assoc = self.assoc_names.contains(name);
        // Everything on the right is expanded *before* the old value is
        // thrown away. `a=("${a[@]}" 3)` has to see the array it is
        // replacing, and clearing first meant it saw an empty one: the
        // array became whatever was literal in the list, so the
        // idiomatic `a=("${a[@]/x/y}")` emptied it outright.
        //
        // One *field* per element, not one word of source text:
        // `b=("${a[@]}")` has to copy the array, and `c=($unquoted)`
        // has to split.
        enum Staged {
            Positional(Vec<String>),
            Keyed(String, String),
        }
        let mut staged: Vec<Staged> = Vec::with_capacity(items.len());
        for item in items {
            match item {
                ArrayLiteralItem::Positional(w) => staged.push(Staged::Positional(self.expand_words(std::slice::from_ref(w)))),
                ArrayLiteralItem::Keyed(index, w) => {
                    let v = self.expand_word(w);
                    staged.push(Staged::Keyed(index.clone(), v));
                }
            }
        }
        if mode == AssignMode::Set {
            if is_assoc {
                self.assoc_arrays.insert(name.to_string(), OrderedMap::default());
            } else {
                self.arrays.insert(name.to_string(), std::collections::BTreeMap::new());
            }
        }
        let mut next_index: usize = match mode {
            AssignMode::Append if !is_assoc => self.arrays.get(name).and_then(|m| m.keys().next_back()).map(|k| k + 1).unwrap_or(0),
            _ => 0,
        };
        for item in staged {
            match item {
                Staged::Positional(values) => {
                    for v in values {
                        if is_assoc {
                            self.assoc_arrays.entry(name.to_string()).or_default().insert(next_index.to_string(), v);
                        } else {
                            self.arrays.entry(name.to_string()).or_default().insert(next_index, v);
                        }
                        next_index += 1;
                    }
                }
                Staged::Keyed(index, v) => {
                    if let Some(idx) = self.array_set_index(name, &index, v) {
                        next_index = idx + 1;
                    }
                }
            }
        }
        true
    }

    // Reconstructs a display string for one `name=(...)`/`name+=(...)`
    // array literal -- used only for `set -x` tracing when this appears
    // as a later word of a declare/local/export/readonly/typeset
    // command (see SimpleCommand::array_word_assigns's own doc
    // comment); the actual application uses the structured items
    // directly (apply_array_literal), never re-parses this text. Each
    // element's expanded value is single-quote-escaped
    // (crate::serialize::quote_literal) so embedded whitespace stays
    // visually unambiguous; an element's own `[index]=` key (if any) is
    // shown as its raw, unevaluated source text, matching bash's own
    // xtrace output for an unresolved index expression.
    fn array_literal_display(&mut self, name: &str, mode: AssignMode, items: &[ArrayLiteralItem]) -> String {
        let op = if mode == AssignMode::Append { "+=" } else { "=" };
        let mut parts = Vec::new();
        for item in items {
            parts.push(match item {
                ArrayLiteralItem::Positional(w) => xtrace_quote(&self.expand_word(w)),
                ArrayLiteralItem::Keyed(index, w) => format!("[{}]={}", index, xtrace_quote(&self.expand_word(w))),
            });
        }
        format!("{name}{op}({})", parts.join(" "))
    }

    // Bash word-splitting: unquoted expansion results are split on
    // whitespace (IFS, hardcoded to the default here) into separate fields;
    // literal text (whether from quotes or plain source) never splits, since
    // unquoted literal whitespace would already have ended the word at the
    // lexer level. Only used where splitting actually applies (command
    // argv, `for` word-lists) -- assignment RHS, case words, redirect
    // targets, etc. still go through plain expand_word.
    // Returns (fields, patterns): `fields` is the word-split display text,
    // exactly as before; `patterns` is an index-aligned second copy of the
    // same fields but with every *quoted* chunk's contribution escaped via
    // glob::escape (unquoted chunks -- including their expansion results,
    // e.g. an unquoted `$var` whose value happens to contain `*` -- stay
    // raw glob syntax). expand_words glob-expands each field against its
    // paired pattern, falling back to the literal field text when the
    // pattern has no metachars or no filesystem matches. Building both in
    // one pass (rather than a second, expand_regex_operand-style walk)
    // matters here specifically because this function's expansions can
    // have side effects (command substitution, ${x:=default}, `$((x++))`)
    // that must not run twice.
    fn expand_word_split(&mut self, w: &Word) -> (Vec<String>, Vec<String>) {
        let ifs = self.get_ifs();
        let mut fields: Vec<String> = Vec::new();
        let mut current: Option<String> = None;
        let mut patterns: Vec<String> = Vec::new();
        let mut pattern_current: Option<String> = None;
        for c in &w.chunks {
            match c {
                // The expanded home directory is text, not a pattern:
                // a `~user` whose home holds a `*` must not glob.
                Chunk::Tilde { name } => {
                    let name = name.clone();
                    let home = self.expand_tilde(&name);
                    current.get_or_insert_with(String::new).push_str(&home);
                    pattern_current.get_or_insert_with(String::new).push_str(&crate::glob::escape(&home));
                }
                Chunk::Str(t) => {
                    current.get_or_insert_with(String::new).push_str(t);
                    pattern_current.get_or_insert_with(String::new).push_str(t);
                }
                Chunk::LiteralStr(t) => {
                    // Quoted or backslash-escaped source text -- always
                    // escaped for the pattern copy, whatever characters it
                    // contains, so it can never itself act as a wildcard.
                    current.get_or_insert_with(String::new).push_str(t);
                    pattern_current.get_or_insert_with(String::new).push_str(&crate::glob::escape(t));
                }
                Chunk::Var { name, quoted } => {
                    // "$@" is a special case even when quoted: it expands
                    // to one field per positional parameter (as if each
                    // were individually double-quoted), not one joined
                    // string -- unlike "$*", which does join. Unquoted $@
                    // falls through to the normal joined-then-split path,
                    // matching bash (both $@ and $* behave the same
                    // unquoted).
                    if name == "@" && *quoted {
                        let parts = self.arg_frames.last().cloned().unwrap_or_default();
                        append_parts_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &parts);
                    } else {
                        let name = name.clone();
                        self.check_param_name(&name);
                        self.check_nounset(&name);
                        let v = self.lookup_var(&name);
                        append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                    }
                }
                Chunk::Sub { raw, quoted } => {
                    let v = self.run_command_substitution(raw);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::Arith { raw, quoted } => {
                    let v = match self.eval_arith(raw) {
                        Ok(n) => n.to_string(),
                        Err(e) => {
                            sh_eprintln!(self, "bish: (({})): {}", raw, e);
                            // A `$(( ))` that does not parse is fatal
                            // in bash, unlike the `(( ))` *command*.
                            self.expansion_failed = true;
                            String::new()
                        }
                    };
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::VarExpand { name, op, quoted } => {
                    let name = name.clone();
                    let op = op.clone();
                    // A slice of `$@` is a list, and a quoted one is
                    // one field per element -- the same rule `"$@"`
                    // itself follows.
                    if let Some(sliced) = self.list_slice(&name, None, &op) {
                        let items = self.reported_slice(sliced);
                        if name == "@" && *quoted {
                            append_parts_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &items);
                        } else {
                            let joined = items.join(&self.ifs_join_char());
                            append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &joined, *quoted, &ifs);
                        }
                        continue;
                    }
                    let v = self.eval_var_op(&name, &op);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::ArrayVar { name, index, quoted } => {
                    // "${arr[@]}" is the array analog of "$@": one field per
                    // element even though it's quoted. "${arr[*]}" (quoted
                    // or not) and unquoted "${arr[@]}" join with a space
                    // first, like $*.
                    if index == "@" && *quoted {
                        let items = self.array_all(name);
                        append_parts_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &items);
                    } else if index == "@" || index == "*" {
                        let sep = self.ifs_join_char();
                        let joined = self.array_all(name).join(&sep);
                        append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &joined, *quoted, &ifs);
                    } else {
                        let name = name.clone();
                        let index = index.clone();
                        let v = self.array_element(&name, &index);
                        append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                    }
                }
                Chunk::ArrayLength { name, index } => {
                    let name = name.clone();
                    let index = index.clone();
                    let v = self.array_length(&name, &index).to_string();
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, true, &ifs);
                }
                Chunk::ArrayVarExpand { name, index, op, quoted } => {
                    let name = name.clone();
                    let index = index.clone();
                    let op = op.clone();
                    if let Some(sliced) = self.list_slice(&name, Some(&index), &op) {
                        let items = self.reported_slice(sliced);
                        if index == "@" && *quoted {
                            append_parts_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &items);
                        } else {
                            let joined = items.join(&self.ifs_join_char());
                            append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &joined, *quoted, &ifs);
                        }
                        continue;
                    }
                    let v = self.eval_array_var_op(&name, &index, &op);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::Indirect { name, quoted } => {
                    let v = self.indirect_var(name);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::ArrayKeys { name, quoted } => {
                    // Same @-vs-* / quoted-vs-not splitting rules as
                    // ${arr[@]}: "@" quoted is one field per key.
                    if *quoted {
                        let items = self.array_keys(name);
                        append_parts_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &items);
                    } else {
                        let sep = self.ifs_join_char();
                        let joined = self.array_keys(name).join(&sep);
                        append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &joined, *quoted, &ifs);
                    }
                }
                Chunk::VarNamesMatchingPrefix { prefix, at, quoted } => {
                    // Same splitting rules as ${!arr[@]}/${!arr[*]} above,
                    // but the '@'-vs-'*' distinction is actually tracked
                    // here (unlike ArrayKeys, which collapses both into
                    // one shape) -- only the true "@" quoted spelling gets
                    // one field per name.
                    let names = self.var_names_with_prefix(prefix);
                    if *at && *quoted {
                        append_parts_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &names);
                    } else {
                        let joined = names.join(" ");
                        append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &joined, *quoted, &ifs);
                    }
                }
                Chunk::ProcSubIn { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_in(&raw);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, true, &ifs);
                }
                Chunk::ProcSubOut { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_out(&raw);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, true, &ifs);
                }
            }
        }
        if let Some(c) = current {
            fields.push(c);
        }
        if let Some(c) = pattern_current {
            patterns.push(c);
        }
        (fields, patterns)
    }

    // The default IFS (" \t\n") when the variable is truly unset; its
    // actual value (which may be empty, meaning "don't split at all")
    // whenever it's been assigned, even to "".
    /// `FIGNORE` split into the suffixes completion should leave out.
    /// Empty entries are dropped, so a stray `:` cannot turn into a
    /// suffix that matches everything.
    pub fn fignore_suffixes(&self) -> Vec<String> {
        self.raw_var_lookup("FIGNORE").split(':').filter(|s| !s.is_empty()).map(str::to_string).collect()
    }

    fn get_ifs(&mut self) -> String {
        if self.var_is_set("IFS") { self.lookup_var("IFS") } else { " \t\n".to_string() }
    }

    // Re-lexes and expands a captured raw operand (the "word"/"pattern"
    // half of a ${...} expansion), so it can itself contain further $
    // expansions. Parsed as a single word (see parse_expansion_word) --
    // unlike a command line, unquoted whitespace inside it is literal
    // content, not a field separator.
    // Arithmetic over text that may contain a command substitution.
    // bash expands `$( )` and backticks in an arithmetic context before
    // evaluating, and arith.rs has no way to run a command -- so
    // `$(( $(echo 2) + 3 ))` was "bad substitution in arithmetic
    // expression".
    //
    // Only when there is actually a substitution to run: `$x` and
    // `${x}` are already arith.rs's own job, and a bare `x` has to stay
    // a *name* so `x=y; y=2; echo $((x))` still resolves through to 2.
    pub(crate) fn eval_arith(&mut self, raw: &str) -> Result<i64, String> {
        // `${...}` joins `$( )` and backticks here. arith.rs reads a
        // bare `x` and a plain `${x}` itself, but every parameter
        // expansion with an operator in it -- `${x:-5}`, `${#x}`,
        // `${#a[@]}` -- was reaching its lexer unexpanded and coming
        // out as 0. That is how `f() { f $((${1:-0}+1)); }` came to
        // recurse forever passing 1 each time: not a runaway function,
        // an argument that could not change.
        // Nothing to evaluate is zero, both as written (`$(( ))`) and
        // after expanding to nothing (`$(( ${unset} ))`). bash answers
        // 0 to both; bish used to answer 0 to neither, reporting
        // "unexpected token in arithmetic expression: Eof".
        if raw.trim().is_empty() {
            return Ok(0);
        }
        // Any `$` at all, not just `$(`/`${`. A bare `$x` was left for
        // arith.rs to read as a variable, which works where it stands
        // alone -- and not where it is part of a larger token:
        // `$((10#$x))`, the idiom for forcing base ten on a
        // zero-padded number, reached the lexer as `10#$x` and failed
        // with "invalid integer constant". bash expands the whole
        // expression before evaluating any of it.
        if raw.contains('$') || raw.contains('`') {
            let expanded = self.expand_raw(raw);
            if expanded.trim().is_empty() {
                return Ok(0);
            }
            return arith::eval(&expanded, self);
        }
        arith::eval(raw, self)
    }

    // What a `~` prefix stands for. Empty is `$HOME`; `+` and `-` are
    // bash's spellings of `$PWD` and `$OLDPWD`; anything else is a user
    // name, looked up in /etc/passwd. A name with no home found is left
    // as it was written, which is what bash does -- `~nosuchuser/x`
    // really is the literal path `~nosuchuser/x`.
    fn expand_tilde(&mut self, name: &str) -> String {
        match name {
            "" => self.lookup_var("HOME"),
            "+" => self.lookup_var("PWD"),
            "-" => self.lookup_var("OLDPWD"),
            user => home_of_user(user).unwrap_or_else(|| format!("~{user}")),
        }
    }

    // `PS4`, the string `set -x` puts in front of each traced line.
    // bash's default is `+ `, and the first character is repeated once
    // per level of nesting -- only the flat default is reproduced here,
    // since nothing in this shell tracks a trace depth.
    fn xtrace_prefix(&mut self) -> String {
        if !self.var_is_set("PS4") {
            return String::new();
        }
        // Expanded, not printed as written. PS4 is nearly always set to
        // something with an expansion in it -- `PS4='+$LINENO '` is the
        // reason anyone sets it at all -- and printing the text meant
        // every traced line was prefixed with a literal `+$LINENO`.
        // bash expands parameters, command substitutions and the
        // prompt escapes, in that order, which is what `${v@P}`
        // already does here.
        let raw = self.lookup_var("PS4");
        // With tracing off for the duration. A `$( )` in PS4 runs a
        // command, and tracing *that* command expands PS4 again to
        // print its prefix -- which runs the substitution again. It
        // recursed until the nesting guard stopped it, printing a few
        // hundred traced lines on the way.
        let tracing = std::mem::replace(&mut self.opt_xtrace, false);
        let expanded = self.expand_raw(&raw);
        let prefix = self.expand_prompt_string(&expanded);
        self.opt_xtrace = tracing;
        prefix
    }

    fn expand_raw(&mut self, raw: &str) -> String {
        let chunks = crate::lexer::parse_expansion_word(raw);
        self.expand_word(&Word { chunks, globbable: false })
    }

    fn eval_var_op(&mut self, name: &str, op: &VarOp) -> String {
        let cur = self.lookup_var(name);
        match op {
            // `${#@}` and `${#*}` are `$#` -- how many positional
            // parameters there are, not how long they are once joined.
            // Every other name measures its value.
            VarOp::Length if name == "@" || name == "*" => self.arg_frames.last().map(Vec::len).unwrap_or(0).to_string(),
            VarOp::Length => cur.chars().count().to_string(),
            VarOp::Default { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.var_is_set(name) };
                if trigger { self.expand_raw(word) } else { cur }
            }
            VarOp::AssignDefault { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.var_is_set(name) };
                if trigger {
                    let v = self.expand_raw(word);
                    self.assign_var(name, v.clone());
                    v
                } else {
                    cur
                }
            }
            VarOp::ErrorIfUnset { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.var_is_set(name) };
                if trigger {
                    // bash's own wording when `${x:?}` carries no
                    // message of its own.
                    let msg = match self.expand_raw(word) {
                        m if m.is_empty() => "parameter null or not set".to_string(),
                        m => m,
                    };
                    sh_eprintln!(self, "bish: {}: {}", name, msg);
                    // The whole point of `${x:?}` is to stop.
                    self.expansion_failed = true;
                    String::new()
                } else {
                    cur
                }
            }
            VarOp::AltIfSet { word, colon } => {
                let set_enough = if *colon { !cur.is_empty() } else { self.var_is_set(name) };
                if set_enough { self.expand_raw(word) } else { String::new() }
            }
            VarOp::RemovePrefix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_prefix_glob(&cur, &pattern, *longest)
            }
            VarOp::RemoveSuffix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_suffix_glob(&cur, &pattern, *longest)
            }
            VarOp::CaseConvert { pattern, upper, all } => {
                let pattern = self.expand_raw(pattern);
                apply_case_convert(&cur, &pattern, *upper, *all)
            }
            VarOp::Substring { offset, length } => {
                // An omitted offset is zero -- `${x::3}` -- and the
                // evaluator does not read an empty string as one.
                let off = match offset.trim().is_empty() {
                    true => 0,
                    false => self.eval_arith(offset).unwrap_or(0),
                };
                let len = length.as_ref().and_then(|l| self.eval_arith(l).ok());
                substring_expand(&cur, off, len)
            }
            VarOp::Replace { pattern, repl, global, anchor } => {
                let pattern = self.expand_raw(pattern);
                let repl = self.expand_raw(repl);
                glob_replace(&cur, &pattern, &repl, *global, *anchor)
            }
            VarOp::Transform(kind) => match kind {
                TransformKind::Attributes => self.transform_attributes(name, None),
                TransformKind::AttributeFlags => self.attribute_flags_string(name),
                TransformKind::KeyValue => apply_transform(&cur, TransformKind::Quote),
                TransformKind::Prompt => self.expand_prompt_string(&cur),
                TransformKind::Quote | TransformKind::Upper | TransformKind::UpperFirst | TransformKind::Lower | TransformKind::Escape => {
                    apply_transform(&cur, *kind)
                }
            },
        }
    }

    // Same operators as eval_var_op, but reading (and, for :=, writing)
    // one array element instead of a scalar variable. "@"/"*" indices are
    // treated as the joined-all-elements string, matching how they behave
    // as a plain (non-splitting-aware) expansion elsewhere.
    // The list `${a[@]:off:len}` and `${@:off:len}` slice.
    //
    // `:off:len` is the one operator whose meaning changes when what it
    // is applied to is a list: everywhere else in `${...}` the elements
    // are joined first and the operator works on that text, but a slice
    // of a list counts *elements*. `${a[@]:1:2}` is two elements, not
    // two characters of `"1 2 3 4"`.
    //
    // `None` when this is not that -- an ordinary variable, or any
    // other operator -- and the caller falls through to the text path.
    fn list_slice(&mut self, name: &str, index: Option<&str>, op: &VarOp) -> Option<Result<Vec<String>, String>> {
        // `${a[@]@Q}` and friends transform *each element*, and the
        // result is a list of that many words -- `"${a[@]@Q}"` is how a
        // script re-quotes an array for `eval`. Applying the transform
        // to the joined string instead produced one word holding
        // `'a b'` where bash gives `'a' 'b'`.
        if let VarOp::Transform(
            kind @ (TransformKind::Quote | TransformKind::Upper | TransformKind::UpperFirst | TransformKind::Lower | TransformKind::Escape),
        ) = op
            && matches!(index, Some("@") | Some("*"))
        {
            let items = self.array_all(name);
            return Some(Ok(items.iter().map(|v| apply_transform(v, *kind)).collect()));
        }
        let VarOp::Substring { offset, length } = op else { return None };
        let items = match index {
            // `${a[@]:...}` / `${a[*]:...}` -- the elements in index
            // order, which is also what bash slices for a sparse array
            // (by position among the elements that exist, not by index).
            Some("@") | Some("*") => self.array_all(name),
            Some(_) => return None,
            // `${@:...}` / `${*:...}` -- `$0` first, which is why
            // `${@:0}` names the script and `${@:1}` is the first
            // argument.
            None if name == "@" || name == "*" => {
                std::iter::once(self.script_name.clone()).chain(self.arg_frames.last().cloned().unwrap_or_default()).collect()
            }
            None => return None,
        };
        // `${a[@]::2}` -- an omitted offset is zero, and handing the
        // arithmetic evaluator an empty string is an error rather than
        // a default.
        let offset = match offset.trim().is_empty() {
            true => 0,
            false => match self.eval_arith(offset) {
                Ok(n) => n,
                Err(e) => return Some(Err(e.to_string())),
            },
        };
        let length = match length {
            None => None,
            Some(raw) => match arith::eval(raw, self) {
                Ok(n) => Some(n),
                Err(e) => return Some(Err(e.to_string())),
            },
        };
        Some(slice_elements(items, offset, length))
    }

    // A slice that failed -- a negative length -- reports and yields
    // nothing, which is what bash does with it.
    fn reported_slice(&mut self, sliced: Result<Vec<String>, String>) -> Vec<String> {
        match sliced {
            Ok(items) => items,
            Err(e) => {
                sh_eprintln!(self, "bish: {e}");
                // A word that could not be expanded is fatal in a
                // script -- see expansion_failed.
                self.expansion_failed = true;
                Vec::new()
            }
        }
    }

    fn joined_slice(&mut self, sliced: Result<Vec<String>, String>) -> String {
        let sep = self.ifs_join_char();
        self.reported_slice(sliced).join(&sep)
    }

    // What `"$*"` and `"${a[*]}"` put between elements: IFS's first
    // character, a space when IFS is unset, and nothing at all when it
    // is empty.
    fn ifs_join_char(&mut self) -> String {
        match self.var_is_set("IFS") {
            false => " ".to_string(),
            true => self.lookup_var("IFS").chars().next().map(String::from).unwrap_or_default(),
        }
    }

    // The ops that are purely a function of one string, so they can be
    // applied to a scalar or to each element of an array in turn.
    fn apply_string_var_op(&mut self, cur: &str, op: &VarOp) -> String {
        match op {
            VarOp::RemovePrefix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_prefix_glob(cur, &pattern, *longest)
            }
            VarOp::RemoveSuffix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_suffix_glob(cur, &pattern, *longest)
            }
            VarOp::CaseConvert { pattern, upper, all } => {
                let pattern = self.expand_raw(pattern);
                apply_case_convert(cur, &pattern, *upper, *all)
            }
            VarOp::Replace { pattern, repl, global, anchor } => {
                let pattern = self.expand_raw(pattern);
                let repl = self.expand_raw(repl);
                glob_replace(cur, &pattern, &repl, *global, *anchor)
            }
            _ => cur.to_string(),
        }
    }

    fn eval_array_var_op(&mut self, name: &str, index: &str, op: &VarOp) -> String {
        // `${a[@]OP}` applies OP to each element and rejoins, for the
        // ops that are about a *string*. Applied to the joined text
        // instead, the ones that act at most once per string acted once
        // for the whole array: `${a[@]/o/0}` on `(one two)` changed
        // only the first element, and `${a[@]%e}` looked at whether the
        // joined text ended in `e` rather than each element. The
        // globally-acting ones (`//`, `^^`) came out right by accident,
        // which is why this went unnoticed.
        //
        // Not every op is element-wise: `${a[@]:1:2}` slices the array,
        // `${#a[@]}` counts it, and `${a[@]:-x}` asks whether the whole
        // thing is empty. Those are handled elsewhere or below.
        if index == "@" || index == "*" {
            let element_wise =
                matches!(op, VarOp::RemovePrefix { .. } | VarOp::RemoveSuffix { .. } | VarOp::CaseConvert { .. } | VarOp::Replace { .. });
            if element_wise {
                let target = self.array_target(name);
                let elements = self.array_all(&target);
                let mapped: Vec<String> = elements.into_iter().map(|e| self.apply_string_var_op(&e, op)).collect();
                let sep = self.ifs_join_char();
                return mapped.join(&sep);
            }
        }
        let cur = self.array_element(name, index);
        match op {
            VarOp::Length => cur.chars().count().to_string(),
            VarOp::Default { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.array_element_is_set(name, index) };
                if trigger { self.expand_raw(word) } else { cur }
            }
            VarOp::AssignDefault { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.array_element_is_set(name, index) };
                if trigger {
                    let v = self.expand_raw(word);
                    if index != "@" && index != "*" {
                        self.array_set_index(name, index, v.clone());
                    }
                    v
                } else {
                    cur
                }
            }
            VarOp::ErrorIfUnset { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.array_element_is_set(name, index) };
                if trigger {
                    let msg = match self.expand_raw(word) {
                        m if m.is_empty() => "parameter null or not set".to_string(),
                        m => m,
                    };
                    sh_eprintln!(self, "bish: {}[{}]: {}", name, index, msg);
                    self.expansion_failed = true;
                    String::new()
                } else {
                    cur
                }
            }
            VarOp::AltIfSet { word, colon } => {
                let set_enough = if *colon { !cur.is_empty() } else { self.array_element_is_set(name, index) };
                if set_enough { self.expand_raw(word) } else { String::new() }
            }
            VarOp::RemovePrefix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_prefix_glob(&cur, &pattern, *longest)
            }
            VarOp::RemoveSuffix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_suffix_glob(&cur, &pattern, *longest)
            }
            VarOp::CaseConvert { pattern, upper, all } => {
                let pattern = self.expand_raw(pattern);
                apply_case_convert(&cur, &pattern, *upper, *all)
            }
            VarOp::Substring { offset, length } => {
                // An omitted offset is zero -- `${x::3}` -- and the
                // evaluator does not read an empty string as one.
                let off = match offset.trim().is_empty() {
                    true => 0,
                    false => self.eval_arith(offset).unwrap_or(0),
                };
                let len = length.as_ref().and_then(|l| self.eval_arith(l).ok());
                substring_expand(&cur, off, len)
            }
            VarOp::Replace { pattern, repl, global, anchor } => {
                let pattern = self.expand_raw(pattern);
                let repl = self.expand_raw(repl);
                glob_replace(&cur, &pattern, &repl, *global, *anchor)
            }
            VarOp::Transform(kind) => match kind {
                TransformKind::Attributes => {
                    // "@"/"*" (the whole array) reconstructs every
                    // element; a specific index still shows the array's
                    // own -a/-A attribute flag but only that one
                    // element's value -- both confirmed against real
                    // bash (see transform_attributes's own doc comment).
                    if index == "@" || index == "*" { self.transform_attributes(name, None) } else { self.transform_attributes(name, Some(&cur)) }
                }
                TransformKind::AttributeFlags => self.attribute_flags_string(name),
                TransformKind::KeyValue => {
                    if index == "@" || index == "*" {
                        self.array_key_value_pairs(name)
                    } else {
                        apply_transform(&cur, TransformKind::Quote)
                    }
                }
                TransformKind::Prompt => self.expand_prompt_string(&cur),
                TransformKind::Quote | TransformKind::Upper | TransformKind::UpperFirst | TransformKind::Lower | TransformKind::Escape => {
                    apply_transform(&cur, *kind)
                }
            },
        }
    }

    // Expands a simple-command's words into argv, applying filesystem
    // pathname (glob) expansion to any word that's both glob-eligible
    // (no quoting/escaping/expansion at all -- see Word::globbable) and
    // actually contains metacharacters. A pattern with no filesystem
    // matches is kept as its literal text, matching bash's default
    // (nullglob-off) behavior.
    // `GLOBIGNORE`: a colon-separated list of patterns, and any
    // pathname expansion result matching one is dropped.
    //
    // Matched against the whole produced name, under the same pathname
    // rules the expansion itself used: `*` does not cross a `/`. So
    // `GLOBIGNORE='*.o'` drops `a.o` and leaves `sub/x.o` alone,
    // because the pattern names one component and that name has two.
    // Checked against real bash, which is the opposite of what the
    // obvious reading of "matched against the filename" suggests.
    //
    // An empty or unset GLOBIGNORE filters nothing, which is the far
    // more common case and is checked first.
    fn apply_globignore(&mut self, matches: Vec<String>) -> Vec<String> {
        let ignore = self.lookup_var("GLOBIGNORE");
        if ignore.is_empty() {
            return matches;
        }
        let patterns: Vec<&str> = ignore.split(':').filter(|p| !p.is_empty()).collect();
        matches.into_iter().filter(|m| !patterns.iter().any(|p| crate::glob::matches_path(p, m))).collect()
    }

    // What `dotglob`/`nocaseglob` currently say. Read per expansion
    // rather than cached: `shopt -s` inside a script has to take effect
    // on the very next word, and reading two booleans is nothing next
    // to the `read_dir` that follows.
    fn glob_options(&self) -> glob::Options {
        glob::Options { dotglob: self.shopt_is_on("dotglob"), nocaseglob: self.shopt_is_on("nocaseglob"), globstar: self.shopt_is_on("globstar") }
    }

    // A pattern that matched nothing. bash's default is to leave the
    // word as its own text -- which is why `echo *.nothing` prints
    // `*.nothing` -- and the two shopts that change that are the whole
    // reason anyone sets them.
    //
    // `failglob` reports and abandons the command, as bash does -- see
    // expansion_failed for why that is fatal in a script and merely a
    // failed command in an interactive session.
    fn unmatched_pattern(&mut self, word: &str) -> Vec<String> {
        if self.shopt_is_on("failglob") {
            sh_eprintln!(self, "bish: no match: {word}");
            self.expansion_failed = true;
            return Vec::new();
        }
        match self.shopt_is_on("nullglob") {
            true => Vec::new(),
            false => vec![word.to_string()],
        }
    }

    fn expand_words(&mut self, words: &[Word]) -> Vec<String> {
        let mut out = Vec::new();
        for w in words {
            if w.globbable {
                // globbable implies no quoting/expansion at all in the word
                // (see Word::globbable), so splitting can't apply here --
                // glob-check the single literal value as before.
                let s = self.expand_word(w);
                if !self.opt_noglob
                    && let Some(matches) = glob::expand(&s, self.glob_options(), &self.cwd)
                {
                    let kept = self.apply_globignore(matches);
                    // Everything the pattern found was ignored, so it
                    // found nothing -- which is the same case as a
                    // pattern that matched nothing, and answered the
                    // same way.
                    if kept.is_empty() {
                        out.extend(self.unmatched_pattern(&s));
                    } else {
                        out.extend(kept);
                    }
                    continue;
                }
                out.push(s);
            } else {
                let (fields, patterns) = self.expand_word_split(w);
                if self.opt_noglob {
                    out.extend(fields);
                } else {
                    for (field, pattern) in fields.into_iter().zip(patterns.into_iter()) {
                        match glob::expand(&pattern, self.glob_options(), &self.cwd).map(|m| self.apply_globignore(m)) {
                            Some(matches) if !matches.is_empty() => out.extend(matches),
                            // A pattern that matched nothing. Not the
                            // same as a word that was never a pattern,
                            // which is the `None` arm.
                            Some(_) => out.extend(self.unmatched_pattern(&field)),
                            None => out.push(field),
                        }
                    }
                }
            }
        }
        out
    }

    // Reads a name's own raw stored value, bypassing nameref redirection --
    // used to read a nameref's target-name string, and internally by
    // resolve_nameref while following a chain.
    pub(crate) fn raw_var_lookup(&self, name: &str) -> String {
        for scope in self.var_scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return v.clone().unwrap_or_default();
            }
        }
        if let Some(v) = self.globals.get(name) {
            return v.clone();
        }
        // The real environment is still consulted last, for a name
        // something outside this table set behind its back -- the
        // session bridge's own `XDG_RUNTIME_DIR`, or `TZ` in a test.
        self.inherited_var(name).unwrap_or_default()
    }

    // Writes a name's own raw value, bypassing nameref redirection -- used
    // to set a nameref's target-name string itself (assign_var, by
    // contrast, is what a nameref's *reads/writes* get redirected through).
    pub(crate) fn raw_var_write(&mut self, name: &str, value: String) {
        for scope in self.var_scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), Some(value.clone()));
                self.export_to_environment(name, &value);
                return;
            }
        }
        self.globals.insert(name.to_string(), value.clone());
        self.export_to_environment(name, &value);
    }

    // The real environment gets a name only while it is exported --
    // that, and nothing else, is what a spawned child inherits.
    //
    // A value the environment cannot hold is simply not written there.
    // `std::env::set_var` *panics* on an interior NUL, which is how
    // `printf 'a\0b' | read -r -d "" v` used to kill the shell; a
    // shell variable may hold one, a C environment string may not, and
    // the variable is worth more than the export.
    /// Kept as the one place that documents why nothing happens here.
    ///
    /// An exported variable used to be written straight into the real
    /// process environment so a spawned child would inherit it. Children
    /// are now built from the shell's own exported names (see
    /// `Shell::command`), which is where the value already is -- and the
    /// write was the only thing making one shell's variables visible to
    /// another sharing this process, which is exactly what an in-process
    /// pipeline stage must not do.
    ///
    /// The NUL check that used to live here moved to `exported_pairs`,
    /// which is now what a value has to survive to reach a child.
    fn export_to_environment(&self, _name: &str, _value: &str) {}

    // Removes a name from wherever it lives: the innermost local scope
    // holding it, the globals, and the real environment. Shared by
    // `unset` and by the restore half of a `FOO=bar cmd` prefix.
    pub(crate) fn remove_var(&mut self, name: &str) {
        self.unset_names.insert(name.to_string());
        for scope in self.var_scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                // The shadow stays and becomes an *unset* one, rather
                // than being removed. Removing it uncovered whatever
                // the caller had, so `f() { local x=1; unset x; echo
                // "${x-gone}"; }` with a global `x=outer` printed
                // `outer` where bash prints `gone`: unsetting a local
                // leaves the name unset for the rest of the function,
                // it does not hand the enclosing one back.
                *slot = None;
                return;
            }
        }
        self.globals.remove(name);
    }

    // Whatever the real environment says about `name` -- unless this
    // shell has unset it, in which case it says nothing.
    fn inherited_var(&self, name: &str) -> Option<String> {
        if self.unset_names.contains(name) {
            return None;
        }
        std::env::var(name).ok()
    }

    // Follows a `declare -n`/`local -n` chain to the final target name,
    // capped to guard against a self-referential or circular nameref (bash
    // detects and errors on these; bish just stops following rather than
    // looping forever).
    fn resolve_nameref(&self, name: &str) -> String {
        let mut current = name.to_string();
        let mut hops = 0;
        while self.nameref_names.contains(&current) && hops < 16 {
            let target = self.raw_var_lookup(&current);
            if target.is_empty() || target == current {
                break;
            }
            current = target;
            hops += 1;
        }
        current
    }

    pub(crate) fn lookup_var(&mut self, name: &str) -> String {
        if self.nameref_names.contains(name) {
            let target = self.resolve_nameref(name);
            if target != name {
                return self.lookup_var(&target);
            }
        }
        match name {
            "?" => self.last_status.to_string(),
            "0" => self.script_name.clone(),
            "#" => self.arg_frames.last().map(|a| a.len()).unwrap_or(0).to_string(),
            // Joined on IFS's first character, not a space: `IFS=-;
            // echo "$*"` is `a-b-c`. Unquoted `$*` reads the same way
            // and is then split on IFS again, which is how bash gets
            // separate fields out of it.
            "@" | "*" => {
                let sep = self.ifs_join_char();
                self.arg_frames.last().map(|a| a.join(&sep)).unwrap_or_default()
            }
            // $$/$!/$RANDOM/$SECONDS/$- are always computed live, never
            // read back from var_scopes/env, matching how bash treats them
            // as effectively magic rather than ordinary settable
            // variables (SECONDS is the one partial exception -- see
            // assign_var's special-case for `SECONDS=n`).
            "$" => std::process::id().to_string(),
            "!" => self.jobs.borrow().last_bg_pid.map(|p| p.to_string()).unwrap_or_default(),
            // These four read something outside the shell's own state
            // (the RNG, the clock), so an otherwise-effectless function
            // body that reads one is not repeating itself. Counted as
            // effects so `check_nonproductive_recursion` cannot mistake
            // `f() { f $SECONDS; }` for a fixed point.
            "RANDOM" => {
                self.effects += 1;
                self.next_random().to_string()
            }
            "LINENO" => self.current_line.to_string(),
            // Seconds since the epoch, and the same with microseconds.
            // bash 5's own two spellings; a script that wants a
            // timestamp should not have to spawn `date` for it.
            "EPOCHSECONDS" => {
                self.effects += 1;
                unix_now().as_secs().to_string()
            }
            "EPOCHREALTIME" => {
                self.effects += 1;
                let now = unix_now();
                format!("{}.{:06}", now.as_secs(), now.subsec_micros())
            }
            "SECONDS" => {
                self.effects += 1;
                (self.shell_start.elapsed().as_secs() as i64 + self.seconds_offset).to_string()
            }
            // The single-letter name of every option currently on,
            // then the letter for how the shell was invoked. bash
            // orders them lowercase-alphabetical, then
            // uppercase-alphabetical, then the invocation letter last:
            // `set -e -f -u -x -C -m -T -E -a -b -v` under `-c` gives
            // `abefhmuvxBCETc`.
            //
            // `h` and `B` are unconditional because they are true and
            // not settable here: this shell hashes what it looks up on
            // PATH and expands braces, and has no way to stop doing
            // either. Everything else is a real option's real state.
            "-" => {
                let mut letters: Vec<char> = vec!['h', 'B'];
                for (on, c) in [
                    (self.opt_errexit, 'e'),
                    (self.opt_noglob, 'f'),
                    (self.opt_monitor, 'm'),
                    (self.opt_nounset, 'u'),
                    (self.opt_xtrace, 'x'),
                    (self.opt_noclobber, 'C'),
                    (self.opt_errtrace, 'E'),
                    (self.opt_functrace, 'T'),
                    (self.opt_restricted, 'r'),
                ] {
                    if on {
                        letters.push(c);
                    }
                }
                letters.sort_by_key(|c| (c.is_ascii_uppercase(), *c));
                let mut s: String = letters.into_iter().collect();
                if let Some(c) = self.invocation_flag {
                    s.push(c);
                }
                s
            }
            _ if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) => {
                let idx: usize = name.parse().unwrap_or(0);
                idx.checked_sub(1).and_then(|i| self.arg_frames.last().and_then(|a| a.get(i))).cloned().unwrap_or_default()
            }
            _ => {
                for scope in self.var_scopes.iter().rev() {
                    if let Some(v) = scope.get(name) {
                        return v.clone().unwrap_or_default();
                    }
                }
                if let Some(v) = self.globals.get(name) {
                    return v.clone();
                }
                // A bare `$a` on an array is `${a[0]}`, for every array
                // -- which is also what makes `$FUNCNAME` the name of
                // the running function rather than nothing. Checked
                // after the scopes and globals so a scalar of the same
                // name still wins.
                if let Some(first) = self.arrays.get(name).and_then(|m| m.values().next()) {
                    return first.clone();
                }
                // Indexed only: an associative array has no element 0
                // to be, so bash's bare `$m` on one is empty.
                // The real environment last, for a name something
                // outside the variable table set behind its back --
                // see raw_var_lookup, which reads the same three
                // places in the same order.
                if let Some(v) = self.inherited_var(name) {
                    return v;
                }
                // Startup-populated-in-real-bash variables: computed on
                // demand here instead, but still overridable by a normal
                // assignment (checked above) since that's the common bash
                // behavior for all of these once actually set.
                match name {
                    "BASH_VERSION" => BASH_VERSION.to_string(),
                    "PPID" => unsafe { getppid() }.to_string(),
                    "UID" => unsafe { getuid() }.to_string(),
                    "EUID" => unsafe { geteuid() }.to_string(),
                    "HOSTNAME" => get_hostname(),
                    // The path this shell was started from. `$0` is
                    // what it was *called* as, which is not the same
                    // thing and is what a script reaches for `$BASH`
                    // to avoid.
                    "BASH" => std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                    // The pid of the process actually running this.
                    // The same as `$$` here, where bash's differs
                    // inside a subshell, because a subshell here is
                    // not a process -- see run_in_child_shell.
                    "BASHPID" => unsafe { getpid_raw() }.to_string(),
                    "BASH_SUBSHELL" => self.subshell_depth.to_string(),
                    "BASH_COMMAND" => self.bash_command.clone(),
                    // The `set -o` and `shopt` options currently on,
                    // colon-separated and sorted, which is how a script
                    // asks `[[ $SHELLOPTS == *errexit* ]]` without
                    // running `set -o` and parsing it.
                    "SHELLOPTS" => {
                        let mut on: Vec<&str> = SET_O_OPTIONS.iter().copied().filter(|n| self.shell_option_enabled(n) == Some(true)).collect();
                        on.sort_unstable();
                        on.join(":")
                    }
                    "BASHOPTS" => {
                        let mut on: Vec<String> = KNOWN_SHOPT_OPTIONS.iter().map(|(n, _)| n.to_string()).filter(|n| self.shopt_is_on(n)).collect();
                        on.sort();
                        on.join(":")
                    }
                    _ => String::new(),
                }
            }
        }
    }

    // Read-only variable lookup for the debugger's own K-hover/`:dbg print`
    // -- deliberately `&self`, unlike lookup_var (`&mut self`, since magic
    // vars like $RANDOM mutate on read and $SECONDS reads a live clock).
    // Scoped to named user variables only: a plain scalar (checked in
    // var_scopes, then the real process environment -- see raw_var_write's
    // own doc comment for why that's where an ordinary global lives),
    // an indexed array, or an associative array. Returns `None` for a
    // truly-unset name *or* a magic/positional one ($?, $1, $RANDOM, ...)
    // -- inspecting those would need `&mut self` or a real side effect,
    // out of scope for a side-effect-free hover.
    pub(crate) fn debug_peek_var(&self, name: &str) -> Option<String> {
        for scope in self.var_scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return v.clone();
            }
        }
        if let Some(items) = self.arrays.get(name) {
            return Some(format!("({})", items.values().map(|v| crate::serialize::quote_literal(v)).collect::<Vec<_>>().join(" ")));
        }
        if let Some(map) = self.assoc_arrays.get(name) {
            return Some(format!(
                "({})",
                map.iter().map(|(k, v)| format!("[{}]={}", k, crate::serialize::quote_literal(v))).collect::<Vec<_>>().join(" ")
            ));
        }
        self.globals.get(name).cloned().or_else(|| self.inherited_var(name))
    }

    // pub: debugger.rs (a separate module -- see DebugHook's own doc
    // comment for why the concrete implementation lives outside exec.rs)
    // needs to install itself as this Shell's active debugger before
    // calling run_program.
    pub fn set_debug_hook(&mut self, hook: Option<Rc<RefCell<dyn DebugHook>>>) {
        self.debug_hook = hook;
    }

    // repl.rs's own multi-session dispatch (the only place more than one
    // Shell ever shares this real process -- window/pane splits, all
    // built on new_virtual_child) calls this right before running
    // anything in this specific session, if a *different* session's
    // Shell was the last one to run: applies this session's own
    // remembered real cwd/env/umask onto the actual process, so that
    // whichever session runs next sees its own variables/umask (not
    // whatever an unrelated sibling session last left the real process
    // in) and so that plain relative-path file I/O (open_out, a
    // redirect's own `Redirect::In`, `source`, ...) -- which, unlike a
    // real external-process spawn, has no explicit `.current_dir(&self.
    // cwd)` of its own and just resolves against the real process cwd --
    // resolves against *this* session's own cwd rather than a sibling's.
    pub fn sync_real_state_in(&self) {
        let _ = std::env::set_current_dir(&self.cwd);
        let current: std::collections::HashSet<String> = std::env::vars().map(|(k, _)| k).collect();
        // Deliberately the raw calls, not env_set/env_unset: this is
        // session switching, which by construction never runs inside a
        // subshell's env journal (see env_journal_push's own comment), and
        // it reapplies the whole snapshot on a hot path.
        for k in &current {
            if !self.env_snapshot.contains_key(k) {
                unsafe { std::env::remove_var(k) };
            }
        }
        for (k, v) in self.env_snapshot.iter() {
            unsafe { std::env::set_var(k, v) };
        }
        unsafe { umask(self.umask_snapshot) };
    }

    // The other half of sync_real_state_in: called right after this
    // session finishes running something, before a *different* session's
    // turn (if any) -- captures whatever the real environment/umask
    // actually are right now back into this session's own remembered
    // state, so its own next turn (sync_real_state_in again) restores
    // exactly what it just left behind. cwd needs no equivalent capture
    // here: self.cwd is already kept live-accurate by run_cd itself,
    // independently of any of this.
    pub fn sync_real_state_out(&mut self) {
        self.env_snapshot = Rc::new(std::env::vars().collect());
        self.umask_snapshot = current_umask();
    }

    // Updates this session's own remembered `TERM`/`COLORTERM` --
    // needed because a plain `std::env::set_var` from outside this
    // session's own state (e.g. session.rs's own bridge, reacting to a
    // `bish session` client's attach handshake) would otherwise be
    // silently undone the very next time this session runs anything at
    // all: `sync_real_state_in` unconditionally reapplies `env_snapshot`
    // -- captured before that external change ever happened -- onto the
    // real environment first thing every `run_program` call. This is
    // the one correct way to change a session's env from outside it.
    // `term` empty is treated as "leave TERM alone" (every real
    // terminal sets it; an empty value here almost certainly means the
    // client's own environment just didn't have it set, not a genuine
    // request to unset bish's own already-known-good value).
    // `colorterm` empty *does* unset it, matching real bash's own
    // convention (COLORTERM is either unset or a real, nonempty value)
    // -- deliberately asymmetric with `term`, since a reattach from a
    // truecolor terminal to a plain one needs the stale truecolor
    // capability to actually go away, not linger.
    pub fn set_terminal_capability_env(&mut self, term: &str, colorterm: &str) {
        let snap = Rc::make_mut(&mut self.env_snapshot);
        if !term.is_empty() {
            snap.insert("TERM".to_string(), term.to_string());
        }
        if colorterm.is_empty() {
            snap.remove("COLORTERM");
        } else {
            snap.insert("COLORTERM".to_string(), colorterm.to_string());
        }
        // And into the variables, which is what a spawned command now
        // reads (see `Shell::command`). Reattaching a session to a
        // different terminal used to reach a child through the real
        // process environment, which this wrote by way of
        // `sync_real_state_in`; children no longer read that, so a
        // `TERM` updated only there would have been invisible to every
        // program the session went on to run.
        if !term.is_empty() {
            self.assign_var("TERM", term.to_string());
            self.exported_names.insert("TERM".to_string());
        }
        if colorterm.is_empty() {
            self.globals.remove("COLORTERM");
        } else {
            self.assign_var("COLORTERM", colorterm.to_string());
            self.exported_names.insert("COLORTERM".to_string());
        }
    }

    // `set -u`: only a *bare* $VAR/${VAR} reference to a truly-unset name
    // triggers this -- ${VAR:-default}/${VAR-default}/${VAR?msg} etc are
    // explicitly exempt in bash (checking for unset is their whole point),
    // so this is only called from the plain Chunk::Var expansion sites, not
    // from eval_var_op/eval_array_var_op.
    // xorshift64* -- simple, fast, no external RNG crate. Bash's $RANDOM
    // range is 0..32767.
    // `command -v`/`-V name`: reports what `name` resolves to (function,
    // builtin, or a PATH-resolved executable) without running it. Prints
    // nothing and returns 1 if it doesn't resolve to anything, matching
    // bash -- this is what makes `command -v foo >/dev/null` work as an
    // existence check.
    fn command_v(&mut self, name: &str, verbose: bool) -> i32 {
        if self.functions.contains_key(name) {
            sh_println!(self, "{}", if verbose { format!("{} is a function", name) } else { name.to_string() });
            return 0;
        }
        if self.is_active_builtin(name) {
            sh_println!(self, "{}", if verbose { format!("{} is a shell builtin", name) } else { name.to_string() });
            return 0;
        }
        match resolve_in_path(name, &self.lookup_var("PATH")) {
            Some(p) => {
                sh_println!(self, "{}", if verbose { format!("{} is {}", name, p) } else { p });
                0
            }
            None => 1,
        }
    }

    fn next_random(&mut self) -> u32 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        ((x >> 33) % 32768) as u32
    }

    // `${x!y}`, `${1bad}`, `${a[}`: brace content that is not a
    // parameter name at all. The lexer hands it here as an ordinary
    // name because that is what it looks like from outside; nothing
    // could ever set it, so reading it as an unset variable turns a
    // typo into an empty string. Reports it and marks the command
    // failed, which is what bash does.
    // Fatal in a script, the way bash has it -- `${x!y}` is a typo, and
    // every line after it was written expecting the value it did not
    // get. An interactive session just fails the command, as with a
    // refused readonly write.
    fn take_expansion_failure(&mut self) -> Option<ExecResult> {
        if !std::mem::take(&mut self.expansion_failed) {
            return None;
        }
        if self.interactive {
            return Some(ExecResult::Status(1));
        }
        self.run_exit_trap();
        Some(ExecResult::Exit(1))
    }

    fn check_param_name(&mut self, name: &str) {
        if is_parameter_name(name) {
            return;
        }
        sh_eprintln!(self, "bish: ${{{}}}: bad substitution", name);
        self.expansion_failed = true;
    }

    fn check_nounset(&mut self, name: &str) {
        if !self.opt_nounset {
            return;
        }
        if self.var_is_set(name) {
            return;
        }
        // bash names a positional with its `$`, and an ordinary
        // variable without one.
        let shown = match name.chars().all(|c| c.is_ascii_digit()) {
            true => format!("${}", name),
            false => name.to_string(),
        };
        sh_eprintln!(self, "bish: {}: unbound variable", shown);
        // 127, the status bash uses for an unbound variable under
        // `set -u` -- not 1. Checked against bash 5.3.
        self.pending_exit = Some(127);
    }

    // Whether `name` is a variable that's actually been assigned, as
    // opposed to merely evaluating to an empty string -- the distinction
    // `${V-x}`/`${V+x}` (unset only) need vs. `${V:-x}`/`${V:+x}` (unset OR
    // empty). Special/positional parameters always count as set.
    pub(crate) fn var_is_set(&self, name: &str) -> bool {
        let resolved;
        let name = if self.nameref_names.contains(name) {
            resolved = self.resolve_nameref(name);
            resolved.as_str()
        } else {
            name
        };
        // Special/positional parameters and the magic variables
        // lookup_var computes on demand (RANDOM, SECONDS, PPID, ...) are
        // always considered set, even under `set -u` -- matching bash,
        // which never treats these as unbound.
        let is_special = matches!(
            name,
            "?" | "0"
                | "#"
                | "@"
                | "*"
                | "$"
                | "!"
                | "-"
                | "RANDOM"
                | "LINENO"
                | "EPOCHSECONDS"
                | "EPOCHREALTIME"
                | "SECONDS"
                | "BASH_VERSION"
                | "PPID"
                | "UID"
                | "EUID"
                | "HOSTNAME"
                | "BASH"
                | "BASHPID"
                | "BASH_SUBSHELL"
                | "BASH_COMMAND"
                | "SHELLOPTS"
                | "BASHOPTS"
        );
        if is_special {
            return true;
        }
        // A positional is set only when there is one at that position:
        // `$1` with no arguments is exactly what `set -u` exists to
        // catch, and treating every digit as always-set exempted the
        // whole family from it. (`$0` is in the list above -- it is the
        // shell's own name and always there.)
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) {
            let index: usize = name.parse().unwrap_or(0);
            return index <= self.arg_frames.last().map(Vec::len).unwrap_or(0);
        }
        // The innermost scope holding the name decides: a `local x`
        // with no value shadows as *unset*, so the search stops there
        // rather than falling through to whatever the caller had.
        for scope in self.var_scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return v.is_some();
            }
        }
        self.globals.contains_key(name) || self.inherited_var(name).is_some()
    }

    // Plain assignment targets the global (process-env) variable, unless it
    // shadows an existing `local` of the same name in the current function
    // scope -- matching bash, where functions don't auto-localize vars.
    // Returns false when the name is readonly and the write was
    // refused -- the message is already printed by then. Most callers
    // have nothing to do with that; the ones that owe the shell an
    // exit status (a bare `x=2` command, `declare`, `local`) check it.
    pub(crate) fn assign_var(&mut self, name: &str, value: String) -> bool {
        self.assign_var_impl(name, value, false)
    }

    // What `name+=v` assigns. Concatenation, except for a name carrying
    // the integer attribute, where bash makes it addition:
    // `declare -i n=3; n+=2` is 5, not "32". Every `+=` on a scalar
    // goes through here -- the prefix-assignment form (`n+=2 cmd`) and
    // the environment a child is built with included, since those read
    // the same variable.
    pub(crate) fn appended_value(&mut self, name: &str, v: &str) -> String {
        if self.integer_names.contains(name) {
            let current = self.lookup_var(name);
            let a = arith::eval(&current, self).unwrap_or(0);
            let b = arith::eval(v, self).unwrap_or(0);
            return (a + b).to_string();
        }
        self.lookup_var(name) + v
    }

    /// Assigns and marks exported, the way `export NAME=value` does --
    /// without going through the lexer and parser to say so.
    pub fn export_var(&mut self, name: &str, value: String) {
        self.exported_names.insert(name.to_string());
        self.assign_var(name, value.clone());
        self.export_to_environment(name, &value);
    }

    // `declare -g`/`local -g`: same readonly guard, SECONDS/RANDOM
    // specials, integer/case-fold attributes, and export mirroring as
    // assign_var, but always writes straight to the true global
    // (process-env) scope, bypassing any same-named local shadow in the
    // current function -- unlike assign_var/raw_var_write, which target
    // whichever scope already shadows the name.
    pub(crate) fn assign_var_global(&mut self, name: &str, value: String) -> bool {
        self.assign_var_impl(name, value, true)
    }

    fn assign_var_impl(&mut self, name: &str, value: String, force_global: bool) -> bool {
        let resolved;
        let name = if self.nameref_names.contains(name) {
            resolved = self.resolve_nameref(name);
            resolved.as_str()
        } else {
            name
        };
        if self.name_is_readonly(name) {
            sh_eprintln!(self, "bish: {}: readonly variable", name);
            return false;
        }
        // `SECONDS=n` resets the elapsed-time counter to start counting
        // from n, matching bash -- lookup_var computes it live rather
        // than storing it, so the assignment records an offset instead of
        // writing a var_scopes/env entry.
        if name == "SECONDS" {
            if let Ok(n) = value.trim().parse::<i64>() {
                self.seconds_offset = n - self.shell_start.elapsed().as_secs() as i64;
            }
            return true;
        }
        // `RANDOM=n` reseeds the generator, matching bash (rather than
        // making $RANDOM a static value forever).
        if name == "RANDOM" {
            if let Ok(n) = value.trim().parse::<u64>() {
                self.rng_state = if n == 0 { 0x2545F4914F6CDD1D } else { n };
            }
            return true;
        }
        // `declare -i`/`local -i`: the assigned text is evaluated as an
        // arithmetic expression rather than stored literally (bash: `n="2+3"`
        // on an integer-attribute variable stores 5, not the string "2+3").
        let value = if self.integer_names.contains(name) { arith::eval(&value, self).unwrap_or(0).to_string() } else { value };
        // `declare -u`/`-l`: case-fold on every assignment.
        let value = if self.upper_names.contains(name) {
            value.to_uppercase()
        } else if self.lower_names.contains(name) {
            value.to_lowercase()
        } else {
            value
        };
        // `declare -a b; b=x` writes element 0, not a scalar `b` that
        // shadows the array -- the attribute decides, so it applies to a
        // name that was declared and never assigned as much as to one
        // that already holds values.
        if self.assoc_names.contains(name) {
            self.assoc_arrays.entry(name.to_string()).or_default().insert("0".to_string(), value);
            return true;
        }
        if self.array_names.contains(name) || self.arrays.contains_key(name) {
            self.arrays.entry(name.to_string()).or_default().insert(0, value);
            return true;
        }
        if force_global {
            // Bypass any local shadow entirely -- raw_var_write would
            // just update that shadow instead, same as plain assignment.
            self.globals.insert(name.to_string(), value.clone());
            self.export_to_environment(name, &value);
            return true;
        }
        self.raw_var_write(name, value);
        true
    }

    fn resolve_redirects(&mut self, cmd: &SimpleCommand) -> Result<ResolvedRedirs, String> {
        self.resolve_redirect_list(&cmd.redirects)
    }

    // Installs an OutputSink::Builtin override for the duration of one
    // dispatch_builtin_or_external call (see that wrapper and OutputSink::
    // Builtin's own doc comment). Returns Ok(false) -- sink left untouched
    // -- when `redirects` has nothing touching fd 1 or 2 at all, the
    // overwhelming common case: only Out/Err/Both/DupErrToOut and their
    // explicit `1>`/`2>`/`1>&2`/`2>&1` numbered-fd spellings count, since no
    // builtin ever writes to any other fd (an external spawn already
    // handles the rest in full via resolve_redirect_list/apply_fd_redirects,
    // untouched by this). Ok(true) means the sink was swapped and the
    // caller must call pop_builtin_output_sink when done; Err surfaces a
    // failed `open_out` (e.g. `> /no/such/dir/f`) the same way every other
    // redirect failure already does.
    fn push_builtin_output_sink(&mut self, redirects: &[Redirect], who: &str) -> Result<bool, String> {
        // A left-to-right simulation of the command's whole redirect
        // list. Order is the point, and so is *when* a destination is
        // captured:
        //
        //     echo x 3>&1 >&2 2>&3 3>&-     the stdout/stderr swap
        //
        // `2>&3` copies whatever fd 3 names at that moment; the `3>&-`
        // after it closes fd 3 and must not touch fd 2. And in
        // `>file 2>&1` the two streams have to end up on *one* open
        // file, or their write positions diverge and they overwrite
        // each other.
        //
        // Both fall out of giving each destination an identity: a
        // redirect that creates one appends to `dests`, a dup copies
        // the *id*, and a later rebinding of the source fd creates a
        // new id and leaves the old one alone. Two descriptors share
        // exactly when they hold the same id.
        //
        // Only fds 1 and 2 reach the builtin -- it writes nowhere else
        // -- but every fd has to be tracked, because 1 and 2 can be
        // defined in terms of them.
        enum Dest {
            // The *process's* fd N, as this command found it. `echo e
            // >&2 2>/dev/null` prints on the terminal in bash, because
            // `>&2` names fd 2 as it stood then.
            ProcessFd(i32),
            // Opened as the redirect is *read*, not when the sink is
            // built at the end. A redirect creates and truncates its
            // file whether or not anything ends up writing to it:
            // `echo x > a > b` creates both and writes to `b`, and
            // `echo x 3>f` creates `f` though no builtin writes to fd
            // 3. Opening only the two destinations the sink needed
            // meant neither happened, and an error opening a
            // superseded path went unreported.
            File(Rc<RefCell<std::fs::File>>),
            Closed,
        }
        let mut dests: Vec<Dest> = Vec::new();
        let mut table: std::collections::HashMap<i32, usize> = std::collections::HashMap::new();
        macro_rules! define {
            ($fd:expr, $dest:expr) => {{
                dests.push($dest);
                table.insert($fd, dests.len() - 1);
            }};
        }
        // The id fd `n` holds right now -- creating one for the
        // process's own descriptor if this command has not touched it.
        macro_rules! id_of {
            ($fd:expr) => {{
                let fd = $fd;
                match table.get(&fd) {
                    Some(id) => *id,
                    None => {
                        dests.push(Dest::ProcessFd(fd));
                        dests.len() - 1
                    }
                }
            }};
        }
        for r in redirects {
            match r {
                Redirect::Out { word, append, clobber } | Redirect::FdOut { fd: 1, word, append, clobber } => {
                    let p = self.expand_word(word);
                    define!(1, Dest::File(Rc::new(RefCell::new(self.open_out(&p, *append, *clobber)?))))
                }
                Redirect::Err { word, append, clobber } | Redirect::FdOut { fd: 2, word, append, clobber } => {
                    let p = self.expand_word(word);
                    define!(2, Dest::File(Rc::new(RefCell::new(self.open_out(&p, *append, *clobber)?))))
                }
                Redirect::FdOut { fd, word, append, clobber } => {
                    let p = self.expand_word(word);
                    define!(*fd as i32, Dest::File(Rc::new(RefCell::new(self.open_out(&p, *append, *clobber)?))))
                }
                // `&>file`: both descriptors, one open file.
                Redirect::Both { word, append } => {
                    let p = self.expand_word(word);
                    define!(1, Dest::File(Rc::new(RefCell::new(self.open_out(&p, *append, false)?))));
                    let id = table[&1];
                    table.insert(2, id);
                }
                Redirect::DupErrToOut => {
                    let id = id_of!(1);
                    table.insert(2, id);
                }
                Redirect::FdDup { fd, target } => {
                    let id = id_of!(*target as i32);
                    table.insert(*fd as i32, id);
                }
                // `>&$var` -- how a script writes to a coproc's own
                // descriptor. Only the literal-number spelling was
                // matched before, so the redirect was dropped and the
                // builtin wrote to stdout; an external command in the
                // same position worked, which is what made it look like
                // the co-process's descriptors were wrong.
                Redirect::FdDupWord { fd, word } => {
                    let text = self.expand_word(word);
                    let Ok(target) = text.trim().parse::<i32>() else {
                        return Err(format!("{}: ambiguous redirect", text));
                    };
                    let id = id_of!(target);
                    table.insert(*fd as i32, id);
                }
                Redirect::FdClose { fd } => define!(*fd as i32, Dest::Closed),
                _ => {}
            }
        }
        let out_dest = table.get(&1).map(|&i| &dests[i]);
        let err_dest = table.get(&2).map(|&i| &dests[i]);
        if out_dest.is_none() && err_dest.is_none() {
            return Ok(false);
        }
        let shared = matches!((table.get(&1), table.get(&2)), (Some(a), Some(b)) if a == b);
        let stdout = match out_dest {
            None => SinkStream::OuterOut,
            Some(d) => open_dest(d, who)?,
        };
        let stderr = match err_dest {
            None => SinkStream::OuterErr,
            // One destination for both, opened once -- so the two share
            // a write position instead of overwriting each other.
            Some(_) if shared => stdout.clone(),
            Some(d) => open_dest(d, who)?,
        };
        let previous = std::mem::replace(&mut self.sink, OutputSink::Real);
        self.sink = OutputSink::Builtin { previous: Box::new(previous), stdout, stderr };
        return Ok(true);

        fn open_dest(dest: &Dest, who: &str) -> Result<SinkStream, String> {
            match dest {
                Dest::File(f) => Ok(SinkStream::File(Rc::clone(f))),
                // For a *builtin*, fd 1 and fd 2 are not the process's
                // own: they are the enclosing sink, which may already
                // be a capture, a grid, or an outer command's redirect.
                // So these resolve to "whatever this sink's parent does
                // with them" rather than to a dup of the real
                // descriptor. Any other descriptor is a real one and
                // really is duped.
                Dest::ProcessFd(1) => Ok(SinkStream::OuterOut),
                Dest::ProcessFd(2) => Ok(SinkStream::OuterErr),
                // A dup of a descriptor nothing has open is an error,
                // not a silent fallback to stdout: `echo x >&9` reports
                // and fails, the way it does for an external command.
                Dest::ProcessFd(fd) => match dup_existing_fd(*fd) {
                    Some(f) => Ok(SinkStream::File(Rc::new(RefCell::new(f)))),
                    None => Err(format!("{}: Bad file descriptor", fd)),
                },
                Dest::Closed => Err(format!("{}: write error: Bad file descriptor", who)),
            }
        }
    }

    // Restores whatever sink push_builtin_output_sink saved -- a no-op if
    // the sink isn't currently an OutputSink::Builtin (push returned false
    // or was never called), so callers can invoke this unconditionally.
    fn pop_builtin_output_sink(&mut self) {
        if let OutputSink::Builtin { previous, .. } = &mut self.sink {
            self.sink = *std::mem::replace(previous, Box::new(OutputSink::Real));
        }
    }

    // Diagnostics for a command that never got to inherit its own stdio
    // (e.g. spawn() failing with "not found") would otherwise bypass a
    // `2>` redirect entirely -- real bash routes them through it too. Falls
    // back to the shell's real stderr when there's no stderr redirect.
    fn write_command_error(&mut self, cmd: &SimpleCommand, msg: &str) {
        let target = self.peek_stderr_target(&cmd.redirects);
        write_diagnostic(&target, msg, self.sink.clone());
    }

    fn peek_stderr_target(&mut self, redirects: &[Redirect]) -> Option<String> {
        let mut target: Option<String> = None;
        for r in redirects {
            match r {
                Redirect::Err { word, .. } | Redirect::Both { word, .. } => {
                    target = Some(self.expand_word(word));
                }
                Redirect::DupErrToOut => {
                    for r2 in redirects {
                        if let Redirect::Out { word, .. } = r2 {
                            target = Some(self.expand_word(word));
                        }
                    }
                }
                _ => {}
            }
        }
        target
    }

    // The plain-fd-0/1/2 half of `exec`'s redirect-only form (`exec >
    // file`, `exec 2>> file`, `exec < file`, `exec &> file`) -- a
    // dedicated resolver rather than reusing resolve_redirect_list's
    // Option<Stdio> output because Stdio doesn't expose its underlying fd
    // for this function's caller (apply_fds_to_self) to dup2 onto 0/1/2 of
    // *this* process; it needs the raw File instead. Mirrors
    // resolve_redirect_list's In/Out/Err/Both handling (last-one-wins per
    // fd, matching real bash), skipping the numbered-fd/DupErrToOut forms
    // since those are already covered by resolve_redirects' own
    // extra_fds/dup_stderr_to_stdout, computed separately by the caller
    // from the same redirect list.
    // The one place every redirect that opens a file for *writing*
    // funnels through (`>`/`>>`, `2>`/`2>>`, `&>`/`&>>`, `N>file`) --
    // restricted mode's "cannot redirect output" check lives here so it
    // covers all of them at once, rather than duplicated at each
    // Redirect variant's own resolution site.
    /// A path as *this shell* sees it.
    ///
    /// A relative name resolves against the shell's own cwd rather than
    /// the process's. The two agree while only one shell is running, and
    /// stop agreeing the moment two of them share a process and one of
    /// them runs `cd` -- which is what an in-process pipeline stage is.
    /// Every path a script can name goes through here for that reason.
    pub(crate) fn resolve_path(&self, path: &str) -> std::path::PathBuf {
        let given = std::path::Path::new(path);
        if given.is_absolute() { given.to_path_buf() } else { self.cwd.join(given) }
    }

    fn open_out(&self, path: &str, append: bool, clobber: bool) -> Result<std::fs::File, String> {
        if self.opt_restricted {
            return Err(format!("{}: restricted: cannot redirect output", path));
        }
        if let Some(result) = dev_socket_file(path) {
            return result;
        }
        // `set -C`. Only a truncating `>` is refused, and only over an
        // existing *regular* file -- `> /dev/null` and `> fifo` stay
        // legal in real bash, which is what makes noclobber usable at
        // all. Deliberately a stat-then-open rather than O_EXCL: bash
        // does the same, and O_EXCL would also reject the /dev/null
        // case. The race that leaves is bash's too.
        let resolved = self.resolve_path(path);
        if self.opt_noclobber && !append && !clobber && std::fs::metadata(&resolved).is_ok_and(|m| m.is_file()) {
            return Err(format!("{}: cannot overwrite existing file", path));
        }
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(&resolved)
            .map_err(|e| format!("{}: {}", path, os_message(&e)))
    }

    // The one place every redirect that opens a file for *reading*
    // funnels through (`<`, numbered-fd `N<`; here-strings/heredocs go
    // through here_string_file instead, they're never a real path) --
    // mirrors open_out's own shape/doc comment, just above. `/dev/tcp/
    // HOST/PORT` and `/dev/udp/HOST/PORT` (see dev_socket_file) are
    // recognized here too, not just in open_out -- real bash's own
    // /dev/tcp works with any redirect direction, not just `<`, and
    // `exec 3<>/dev/tcp/host/80`'s bidirectional form is actually the
    // most common real-world shape. No restricted-mode check here,
    // unlike open_out -- restricted mode only blocks *output*
    // redirection (see open_out's own doc comment), reading a file is
    // always allowed.
    fn open_in(&self, path: &str) -> Result<std::fs::File, String> {
        if let Some(result) = dev_socket_file(path) {
            return result;
        }
        std::fs::File::open(self.resolve_path(path)).map_err(|e| format!("{}: {}", path, os_message(&e)))
    }

    // The read+write counterpart to open_in/open_out -- bare `<>`/`N<>`'s
    // own target (real bash: "opened for reading and writing on file
    // descriptor n, or on file descriptor 0 if n is not specified").
    // `/dev/tcp`/`/dev/udp` (see dev_socket_file) are recognized here
    // too -- `<>` is in fact the operator real /dev/tcp usage actually
    // needs (one connection used for both the request and the
    // response), not `<`/`>` on their own, which would each open an
    // independent, unrelated connection to the same address.
    fn open_in_out(&self, path: &str) -> Result<std::fs::File, String> {
        if let Some(result) = dev_socket_file(path) {
            return result;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(self.resolve_path(path))
            .map_err(|e| format!("{}: {}", path, os_message(&e)))
    }

    // Restricted mode: SHELL/PATH/ENV/BASH_ENV can't be set or unset --
    // real bash enforces this by reporting exactly the same "readonly
    // variable"/"cannot unset: readonly variable" errors a genuinely
    // `readonly`'d name would, so this rides the same two existing
    // readonly_names checks (assign_var_impl/run_unset) rather than a
    // separate error path, without actually inserting these into
    // readonly_names itself (that would make them readonly forever,
    // including outside restricted mode, and would wrongly show up as
    // `-r` in declare -p/${v@a}).
    // Whether a write to this name will be refused -- `readonly`'s own
    // list plus restricted mode's. The refusal and its message live in
    // assign_var_impl/apply_array_literal/array_set_index; this is for
    // a caller that needs to know one happened without printing a
    // second message about it.
    pub(crate) fn name_is_readonly(&self, name: &str) -> bool {
        self.readonly_names.contains(name) || self.is_restricted_readonly_name(name)
    }

    pub(crate) fn is_restricted_readonly_name(&self, name: &str) -> bool {
        self.opt_restricted && matches!(name, "SHELL" | "PATH" | "ENV" | "BASH_ENV")
    }

    // Restricted mode: "cannot specify `/' in command names" -- blocks
    // running an external command by an explicit path, whether it's an
    // ordinary command word containing '/' or `command NAME`'s own
    // bypass-spawn. Prints bash's own exact error text and returns true
    // when blocked, so a call site can just
    // `if self.check_restricted_command_name(name) { return ...; }`.
    fn check_restricted_command_name(&mut self, name: &str) -> bool {
        if self.opt_restricted && name.contains('/') {
            sh_eprintln!(self, "bish: {}: restricted: cannot specify `/' in command names", name);
            true
        } else {
            false
        }
    }

    // Pure classification, no expansion/side effects: whether every
    // redirect in `redirects` is one run_compound_redirected's in-process
    // path (resolve_simple_redirects_for_compound) can actually handle --
    // checked *before* attempting that resolution (rather than having it
    // fail/fall back partway through) so a redirect target with a side
    // effect (`{ ...; } > "$(side_effect)"`) is never expanded twice, once
    // by a "trial" resolve and again by the re-exec fallback's own
    // resolve_redirect_list.
    fn compound_redirects_are_simple(redirects: &[Redirect]) -> bool {
        redirects.iter().all(|r| {
            matches!(
                r,
                Redirect::In(_)
                    | Redirect::HereString(_)
                    | Redirect::HereDoc(_)
                    | Redirect::Out { .. }
                    | Redirect::Err { .. }
                    | Redirect::Both { .. }
                    | Redirect::DupErrToOut
                    | Redirect::FdOut { fd: 1, .. }
                    | Redirect::FdOut { fd: 2, .. }
                    | Redirect::FdDup { fd: 2, target: 1 }
                    | Redirect::FdDup { fd: 1, target: 2 }
            )
        })
    }

    // Resolves `redirects` into real Files for run_compound_redirected's
    // foreground (in-process, via run_in_child_shell) path -- the same
    // "plain fd 0/1/2" subset push_builtin_output_sink already scopes a
    // single builtin's own redirects to. Only ever called after
    // compound_redirects_are_simple has already confirmed every redirect
    // here is one of those kinds. A stream that ends up on the other
    // one's destination records that it follows it, rather than opening
    // the same path a second time -- two separate opens would track
    // their own, unshared write positions, letting stdout and stderr
    // overwrite each other.
    fn resolve_simple_redirects_for_compound(&mut self, redirects: &[Redirect]) -> Result<ChildStdio, String> {
        let mut stdio = ChildStdio::default();
        // path, append, clobber (`>|`) -- as in open_out's signature.
        let mut stdout_target: Option<(String, bool, bool)> = None;
        let mut stderr_target: Option<(String, bool, bool)> = None;
        for r in redirects {
            match r {
                Redirect::In(w) => {
                    let p = self.expand_word(w);
                    stdio.stdin = Some(self.open_in(&p)?);
                }
                Redirect::HereString(w) => {
                    let mut content = self.expand_word(w);
                    content.push('\n');
                    stdio.stdin = Some(here_string_file(&content)?);
                }
                Redirect::HereDoc(w) => {
                    let content = self.expand_word(w);
                    stdio.stdin = Some(here_string_file(&content)?);
                }
                Redirect::Out { word, append, clobber } | Redirect::FdOut { fd: 1, word, append, clobber } => {
                    stdout_target = Some((self.expand_word(word), *append, *clobber));
                    stdio.out_follows_err = None;
                }
                Redirect::Err { word, append, clobber } | Redirect::FdOut { fd: 2, word, append, clobber } => {
                    stderr_target = Some((self.expand_word(word), *append, *clobber));
                    stdio.err_follows_out = None;
                }
                Redirect::Both { word, append } => {
                    stdout_target = Some((self.expand_word(word), *append, false));
                    stdio.out_follows_err = None;
                    stdio.err_follows_out = Some(Follows::OwnFile);
                }
                Redirect::DupErrToOut | Redirect::FdDup { fd: 2, target: 1 } => {
                    stdio.err_follows_out = Some(if stdout_target.is_some() { Follows::OwnFile } else { Follows::Outer });
                }
                Redirect::FdDup { fd: 1, target: 2 } => {
                    stdio.out_follows_err = Some(if stderr_target.is_some() { Follows::OwnFile } else { Follows::Outer });
                }
                _ => unreachable!("compound_redirects_are_simple already filtered these out"),
            }
        }
        if let Some((p, append, clobber)) = &stdout_target {
            stdio.stdout = Some(self.open_out(p, *append, *clobber)?);
        }
        // A stream that follows the other one opens nothing of its own
        // -- and a `2>file` the dup came *after* named a descriptor that
        // has since been rebound, so its target is stale.
        if stdio.err_follows_out.is_none() {
            if let Some((p, append, clobber)) = &stderr_target {
                stdio.stderr = Some(self.open_out(p, *append, *clobber)?);
            }
        }
        Ok(stdio)
    }

    /// Every redirect this command carries, in source order, as the
    /// actions to perform on the descriptors it has already inherited.
    ///
    /// Source order is the whole of it. A redirect names a descriptor
    /// *as it stands at that point*, so `3>&1 1>&2 2>&3` swaps the two
    /// streams -- and doing fds 0, 1 and 2 first, through the Command
    /// builder, and only then the numbered ones meant `3>&1` copied
    /// stdout's *final* destination rather than the one it had when the
    /// dup was written. That is also why every file the list names is
    /// opened, even one a later redirect supersedes: `> a > b` creates
    /// both, and only `b` survives.
    ///
    /// This is what a real shell does after forking, and the reason it
    /// can be expressed the same way here is that these all run in the
    /// child (a `pre_exec` hook, or this process itself for `exec`),
    /// after it has the descriptors it inherits.
    fn resolve_redirect_list(&mut self, redirects: &[Redirect]) -> Result<ResolvedRedirs, String> {
        let mut actions: Vec<FdAction> = Vec::new();
        for r in redirects {
            match r {
                Redirect::In(w) => {
                    let p = self.expand_word(w);
                    let file = self.open_in(&p)?;
                    actions.push(FdAction::Open { fd: 0, file });
                }
                Redirect::InOut(w) => {
                    let p = self.expand_word(w);
                    let file = self.open_in_out(&p)?;
                    actions.push(FdAction::Open { fd: 0, file });
                }
                Redirect::HereString(w) => {
                    let mut content = self.expand_word(w);
                    content.push('\n');
                    actions.push(FdAction::Open { fd: 0, file: here_string_file(&content)? });
                }
                Redirect::HereDoc(w) => {
                    // Body already ends in '\n' from capture_heredoc_body.
                    let content = self.expand_word(w);
                    actions.push(FdAction::Open { fd: 0, file: here_string_file(&content)? });
                }
                Redirect::Out { word, append, clobber } | Redirect::FdOut { fd: 1, word, append, clobber } => {
                    let p = self.expand_word(word);
                    let file = self.open_out(&p, *append, *clobber)?;
                    actions.push(FdAction::Open { fd: 1, file });
                }
                Redirect::Err { word, append, clobber } | Redirect::FdOut { fd: 2, word, append, clobber } => {
                    let p = self.expand_word(word);
                    let file = self.open_out(&p, *append, *clobber)?;
                    actions.push(FdAction::Open { fd: 2, file });
                }
                // `&>file`: one open file, both descriptors on it, so
                // they share a write position.
                Redirect::Both { word, append } => {
                    let p = self.expand_word(word);
                    let file = self.open_out(&p, *append, false)?;
                    actions.push(FdAction::Open { fd: 1, file });
                    actions.push(FdAction::Dup { fd: 2, source: 1 });
                }
                Redirect::DupErrToOut => actions.push(FdAction::Dup { fd: 2, source: 1 }),
                Redirect::FdOut { fd, word, append, clobber } => {
                    let p = self.expand_word(word);
                    let file = self.open_out(&p, *append, *clobber)?;
                    actions.push(FdAction::Open { fd: *fd as i32, file });
                }
                // `{name}>file`: the descriptor is chosen here, in the
                // shell, because the *variable* has to be set here --
                // the child only ever sees the number.
                Redirect::VarFd { var, kind, word } => {
                    let p = self.expand_word(word);
                    if *kind == crate::lexer::VarFdKind::Dup {
                        // `{v}>&-` closes the descriptor `v` already
                        // names; `{v}>&N` opens a fresh one duplicating
                        // N and names *that*.
                        if p.trim() == "-" {
                            match self.lookup_var(var).trim().parse::<i32>() {
                                Ok(fd) => actions.push(FdAction::Close(fd)),
                                Err(_) => return Err(format!("{}: ambiguous redirect", var)),
                            }
                        } else {
                            match p.trim().parse::<i32>() {
                                Ok(source) => {
                                    let fd = next_free_fd();
                                    self.assign_var(var, fd.to_string());
                                    actions.push(FdAction::Dup { fd, source });
                                }
                                Err(_) => return Err(format!("{}: ambiguous redirect", p)),
                            }
                        }
                        continue;
                    }
                    let file = match kind {
                        crate::lexer::VarFdKind::In => self.open_in(&p)?,
                        crate::lexer::VarFdKind::InOut => self.open_in_out(&p)?,
                        crate::lexer::VarFdKind::Out { append, clobber } => self.open_out(&p, *append, *clobber)?,
                        crate::lexer::VarFdKind::Dup => unreachable!("handled above"),
                    };
                    let fd = next_free_fd();
                    self.assign_var(var, fd.to_string());
                    actions.push(FdAction::Open { fd, file });
                }
                Redirect::FdIn { fd, word } => {
                    let p = self.expand_word(word);
                    let file = self.open_in(&p)?;
                    actions.push(FdAction::Open { fd: *fd as i32, file });
                }
                Redirect::FdInOut { fd, word } => {
                    let p = self.expand_word(word);
                    let file = self.open_in_out(&p)?;
                    actions.push(FdAction::Open { fd: *fd as i32, file });
                }
                Redirect::FdDup { fd, target } => {
                    actions.push(FdAction::Dup { fd: *fd as i32, source: *target as i32 });
                }
                Redirect::FdDupWord { fd, word } => {
                    let target_str = self.expand_word(word);
                    match target_str.trim().parse::<i32>() {
                        Ok(source) => actions.push(FdAction::Dup { fd: *fd as i32, source }),
                        Err(_) => return Err(format!("{}: ambiguous redirect", target_str)),
                    }
                }
                Redirect::FdClose { fd } => actions.push(FdAction::Close(*fd as i32)),
            }
        }
        move_opened_files_out_of_the_way(&mut actions);
        Ok(ResolvedRedirs { actions })
    }
}

impl arith::VarContext for Shell {
    fn get(&mut self, name: &str) -> i64 {
        let text = match split_subscript(name) {
            Some((base, index)) => self.array_element(base, &index),
            None => self.lookup_var(name),
        };
        // A name whose value is itself an expression is evaluated, not
        // parsed: bash's `x=y; y=2; echo $((x))` is 2. A plain number
        // takes the fast path; anything else recurses, capped by the
        // evaluator's own depth limit.
        match text.trim().parse::<i64>() {
            Ok(n) => n,
            Err(_) if text.trim().is_empty() => 0,
            Err(_) => arith::eval(text.trim(), self).unwrap_or(0),
        }
    }

    fn set(&mut self, name: &str, value: i64) {
        // An arithmetic assignment is an effect wherever it appears --
        // `(( i++ ))`, `let`, `$(( x = 1 ))`. Counted here rather than
        // at the `(( ))` command, because only some of those are
        // commands: `f() { ((i++)); f; }` makes progress every time
        // round and was being reported as a fixed point, which is the
        // one thing the recursion proof must never do.
        self.effects += 1;
        match split_subscript(name) {
            Some((base, index)) => {
                let base = base.to_string();
                self.array_set_index(&base, &index, value.to_string());
            }
            None => {
                self.assign_var(name, value.to_string());
            }
        }
    }
}

// `a[1]` -> ("a", "1"). The subscript arrives unevaluated (it may be an
// expression, or an associative array's key), so it is handed back as
// text for array_element/array_set_index to resolve the way a `${a[i]}`
// expansion already does.
fn split_subscript(name: &str) -> Option<(&str, String)> {
    let (base, rest) = name.split_once('[')?;
    let index = rest.strip_suffix(']')?;
    Some((base, index.to_string()))
}

struct ResolvedRedirs {
    /// The command's whole redirect list, in source order -- see
    /// resolve_redirect_list. Nothing is pre-applied to the Command
    /// builder's stdin/stdout/stderr: those carry only what the command
    /// *inherits* (a pipeline's pipe, a capture, the shell's own), and
    /// these run on top of it in the child.
    actions: Vec<FdAction>,
}

/// Moves every file this list opened above the descriptors the list
/// itself rebinds.
///
/// The files are opened in the *shell* and inherited at whatever
/// numbers they happened to get -- which can be numbers the list then
/// dups over. `3>&1 1>&2 2>&3 3>&- 2>/dev/null` opens /dev/null in the
/// shell, and if that landed on fd 3, the `3>&1` overwrote the handle
/// before `2>/dev/null` could install it: the redirect either pointed
/// at the wrong file or failed outright with EBADF.
///
/// `F_DUPFD_CLOEXEC` asks for the lowest free descriptor at or above a
/// floor, which is exactly the primitive for this. Close-on-exec is
/// right for the copy: it is only ever the *source* of a dup2 that
/// happens before the exec, and the descriptor it is duplicated onto
/// gets its own flags.
fn move_opened_files_out_of_the_way(actions: &mut [FdAction]) {
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    }
    const F_DUPFD_CLOEXEC: i32 = 1030;
    let mut floor = 3;
    for a in actions.iter() {
        let highest = match a {
            FdAction::Open { fd, .. } | FdAction::Close(fd) => *fd,
            FdAction::Dup { fd, source } => (*fd).max(*source),
        };
        floor = floor.max(highest + 1);
    }
    for a in actions.iter_mut() {
        let FdAction::Open { file, .. } = a else { continue };
        let raw = std::os::unix::io::AsRawFd::as_raw_fd(file);
        if raw >= floor {
            continue;
        }
        let moved = unsafe { fcntl(raw, F_DUPFD_CLOEXEC, floor) };
        if moved >= 0 {
            *file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(moved) };
        }
    }
}

/// One step in a command's redirect list, to be performed on the
/// descriptors a child has already inherited: point `fd` at an open
/// file, at another descriptor as it stands right now, or close it.
enum FdAction {
    Open { fd: i32, file: std::fs::File },
    Dup { fd: i32, source: i32 },
    Close(i32),
}

// Associative-array storage (`declare -A`). Iterates in insertion order --
// confirmed via a clean probe against real bash that its own iteration
// order is neither insertion order nor alphabetical, but its internal
// hash-table bucket order (stable across updates/delete-then-reinsert on a
// given key, but not derivable without literally reimplementing bash's
// specific hash function and bucket-growth behavior). Insertion order is
// the more useful, predictable choice for a new implementation even though
// it won't byte-match bash's own output for a script that happens to
// depend on the exact order -- a genuinely rare thing to depend on, since
// bash's own order isn't something a script author could reliably predict
// either.
#[derive(Default, Clone)]
pub(crate) struct OrderedMap {
    order: Vec<String>,
    values: std::collections::HashMap<String, String>,
}

impl OrderedMap {
    fn get(&self, key: &str) -> Option<&String> {
        self.values.get(key)
    }

    fn contains_key(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    fn insert(&mut self, key: String, value: String) {
        if !self.values.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.values.insert(key, value);
    }

    pub(crate) fn remove(&mut self, key: &str) -> Option<String> {
        let v = self.values.remove(key);
        if v.is_some() {
            self.order.retain(|k| k != key);
        }
        v
    }

    fn keys(&self) -> impl Iterator<Item = &String> {
        self.order.iter()
    }

    fn values(&self) -> impl Iterator<Item = &String> {
        self.order.iter().map(move |k| self.values.get(k).unwrap())
    }

    fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.order.iter().map(move |k| (k, self.values.get(k).unwrap()))
    }

    fn len(&self) -> usize {
        self.order.len()
    }
}

// A background job (`cmd &`), one or more children (a whole pipeline when
// backgrounded together). `children` are kept so `wait`/`jobs`/`fg` can
// A coproc pipe half the shell keeps open. The read side is wrapped in a
// persistent BufReader (not a fresh one per `read -u` call) for the same
// reason read_input_source avoids `BufReader::new(stdin())`: a throwaway
// wrapper's internal read-ahead buffer would silently discard whatever it
// over-read past the first line every time it's dropped.
enum KeptFd {
    Read(std::io::BufReader<std::io::PipeReader>),
    Write(std::io::PipeWriter),
}

impl std::os::fd::AsRawFd for KeptFd {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            KeptFd::Read(r) => r.get_ref().as_raw_fd(),
            KeptFd::Write(w) => w.as_raw_fd(),
        }
    }
}

// poll/block on them directly -- earlier this shell just spawned a thread
// that blindly `.wait()`d and dropped the result, which meant nothing else
// could ever observe a background job's completion or exit status.
pub(crate) struct Job {
    pub(crate) id: u32,
    pub(crate) pids: Vec<u32>,
    children: Vec<std::process::Child>,
    pub(crate) cmd_text: String,
    // The pty (M7) this background job's stdio is attached to instead of
    // inheriting the real terminal's fds -- set for every background job
    // spawned while promoted, whatever shape it is: a single external
    // command, a subshell, a pipeline, or any of those with redirects of
    // their own (the redirected streams still go where they were told;
    // only the ones nothing claimed land here). `None` when the session
    // wasn't promoted at spawn time, which is also when there's no grid
    // to render into and inherited stdio is already going to the right
    // place.
    //
    // Two things read it: `fg` (see run_fg) drives poll-based rendering
    // into the fg-ing session's grid instead of blocking on Job::wait(),
    // and drain_background_output keeps the job's output flowing into
    // `sink_screen` while it stays in the background.
    pub(crate) pty_master: Option<std::fs::File>,
    // The grid this job's own output belongs in: the sink of whichever
    // session spawned it, captured at that moment (see push_job_full).
    // Recorded on the *job* rather than looked up later because the job
    // table is shared by every session (see the `jobs` field's own doc
    // comment) -- there is no "the current session" to ask by the time
    // the output actually shows up. `None` when that session wasn't
    // promoted, which is also exactly when there's no grid to feed and
    // inherited stdio is already going to the right place.
    sink_screen: Option<Rc<RefCell<crate::vt100::Screen>>>,
    // Set to true once this job's pty master has had O_NONBLOCK applied
    // -- done lazily on the first drain rather than at spawn, so every
    // spawn site stays free of it.
    nonblocking: bool,
    // Real job control (M11): Some(pid) for a single external command
    // spawned via run_single's plain (non-pty) path, where the pre_exec
    // hook there gave it its own process group seeded from its own pid --
    // run_fg/run_bg use this to tcsetpgrp the real terminal at it and
    // SIGCONT it. None for everything else: a pty-attached job doesn't
    // need this (spawn_attached's setsid already isolates it into its
    // own session, and drive_fg_job's raw-byte forwarding lets its own
    // pty's line discipline generate SIGTSTP/SIGINT correctly without
    // any of this machinery). A backgrounded pipeline sets one too (see
    // run_multi -- every stage shares the first one's group, so `kill
    // %N`/`bg` reach the whole job), which is why its own pty is wired as
    // plain fds rather than through spawn_attached: that setsid would
    // undo exactly this.
    pub(crate) pgid: Option<u32>,
    // True once this job has been observed stopped (via a WUNTRACED-
    // aware wait -- see waitpid_untraced) rather than exited. Checked by
    // `jobs`/`wait` before touching Job::poll/wait, neither of which can
    // ever observe a stop (Child::try_wait/wait never pass WUNTRACED, so
    // a stopped child just looks like it's still running to them --
    // correct for every *other* caller, since only run_fg's real-job-
    // control path and run_single's own foreground wait ever use
    // waitpid_untraced directly). Cleared by `fg`/`bg` resuming it.
    pub(crate) stopped: bool,
}

// Backs Shell.jobs (see that field's doc comment for why this lives behind
// Rc<RefCell<_>> instead of being owned directly).
pub(crate) struct JobTable {
    pub(crate) jobs: Vec<Job>,
    next_job_id: u32,
    // $!: PID of the most recently backgrounded command (the last stage,
    // for a backgrounded pipeline -- matches bash). Mirrors `jobs.last()`'s
    // PID; kept as its own field since it needs to keep reporting the same
    // PID even after that job is reaped out of the table by `wait`/`jobs`.
    last_bg_pid: Option<u32>,
}

impl JobTable {
    fn new() -> Self {
        JobTable { jobs: Vec::new(), next_job_id: 1, last_bg_pid: None }
    }
}

impl Job {
    // Non-blocking: reaps any children that have already exited without
    // blocking on ones that haven't. Returns the job's exit status once
    // *every* child has exited (bash: a job composed of a pipeline is
    // "done" when its last stage exits; this shell reports done once the
    // whole pipeline has, and uses the last child's code, matching the
    // pipeline exit-status convention used elsewhere in this file).
    pub(crate) fn poll(&mut self) -> Option<i32> {
        let mut all_done = true;
        let mut last_code = 0;
        for c in &mut self.children {
            match c.try_wait() {
                Ok(Some(status)) => last_code = exit_code_from_status(status),
                Ok(None) => all_done = false,
                Err(_) => last_code = 1,
            }
        }
        if all_done { Some(last_code) } else { None }
    }

    // Blocking wait for every child in the job.
    pub(crate) fn wait(&mut self) -> i32 {
        let mut last_code = 0;
        for c in &mut self.children {
            match c.wait() {
                Ok(status) => last_code = exit_code_from_status(status),
                Err(_) => last_code = 1,
            }
        }
        last_code
    }
}

// An opaque handle to a pty-attached job, handed to repl.rs by
// Shell::take_pending_fg once run_fg bubbles ExecResult::Fg. repl.rs is
// what owns the session/window/grid state a redraw needs (see that
// variant's doc comment for why exec.rs can't hold a reference to it),
// and -- once the Frame::Job stack lets a job keep running while another
// window is focused -- repl.rs is also what needs to keep polling it
// across separate main-loop iterations, not just for the duration of one
// blocking call into exec.rs. Wrapping Job (whose fields, like
// std::process::Child, this module deliberately doesn't want to expose
// directly) rather than making Job itself pub keeps that boundary intact
// while still letting repl.rs hold and drive one.
pub struct FgJob(Job);

// How FgJob::poll_untraced found the job. Named distinctly from the
// module-private JobWaitOutcome (same shape) since this one crosses the
// module boundary into repl.rs's public API surface.
pub enum FgWait {
    Running,
    Exited(i32),
    Stopped,
}

impl FgJob {
    pub fn wait(&mut self) -> i32 {
        self.0.wait()
    }

    // Always Some: FgJob only ever exists for a job that had
    // Job::pty_master.is_some() at the point run_fg bubbled it (see that
    // function's own pty_master check).
    pub fn pty_master(&mut self) -> &mut std::fs::File {
        self.0.pty_master.as_mut().expect("FgJob always has a pty_master")
    }

    // Explicitly signals this job to stop, rather than relying on
    // forwarding a raw Ctrl-Z byte into its pty and letting ISIG
    // generate SIGTSTP the way SIGINT/Ctrl-C already does. That natural
    // path doesn't work here: pty::spawn_attached's pre_exec makes the
    // job its own session leader (setsid -- required for TIOCSCTTY to
    // succeed at all, so the pty can be its controlling terminal), which
    // means its process group is *orphaned* relative to bish's own
    // session (its parent isn't a member of that new session). Confirmed
    // empirically (not just from the spec): a job in this state doesn't
    // stop for a *default-disposition* SIGTSTP no matter how it's sent
    // -- forwarded through the pty (ISIG), or even an unrelated `kill
    // -TSTP` from a completely separate shell outside bish entirely all
    // silently do nothing, while `kill -STOP` on the exact same process
    // stops it immediately. That's the real rule (broader than "only
    // line-discipline-generated stop signals get discarded" -- it's
    // POSIX/Linux discarding *any* uncaught SIGTSTP/SIGTTIN/SIGTTOU
    // delivery to an orphaned group, regardless of source), and it's
    // exactly why Ctrl-C already worked here (SIGINT isn't subject to
    // it) while Ctrl-Z silently didn't. SIGSTOP sidesteps this --
    // uncatchable, so the same orphan-discard doesn't apply -- at the
    // cost of not giving a job with its own SIGTSTP handler (a
    // full-screen program like vim/less, saving/restoring terminal
    // state around a stop) any chance to run it first. Accepted
    // trade-off: every job spawned via run_single's simple-external-
    // command path (the only thing that reaches this pty-attachment
    // mechanism at all) is orphaned this same way, so there's no way to
    // reach a job with such a handler AND deliver a real SIGTSTP to it
    // in this architecture regardless.
    pub fn send_stop(&mut self) {
        if let Some(&pid) = self.0.pids.first() {
            send_signal_to_pgrp(pid, SIGSTOP);
        }
    }

    // Non-blocking, WUNTRACED-aware poll -- unlike the plain Job::poll
    // (which wraps Child::try_wait and can never observe anything but
    // exited/still-running), this can tell repl.rs's drive_fg_job that
    // the job has *stopped* (Ctrl-Z forwarded into its own pty, or an
    // explicit `kill -STOP`) instead of leaving it looking like it's
    // still running forever -- the same M11 gap Job::poll has for the
    // non-pty foreground path, fixed here for the pty-attached one.
    pub fn poll_untraced(&mut self) -> FgWait {
        let pid = self.0.pids[0];
        let mut status: i32 = 0;
        let r = unsafe { waitpid(pid as i32, &mut status, WNOHANG | WUNTRACED) };
        if r == 0 {
            return FgWait::Running;
        }
        if r < 0 {
            return FgWait::Exited(1);
        }
        if wait_status_stopped(status) {
            return FgWait::Stopped;
        }
        if wait_status_signaled(status) {
            return FgWait::Exited(128 + wait_status_term_sig(status));
        }
        FgWait::Exited(wait_status_exit_code(status))
    }
}

// `echo -e`'s escapes, and `printf %b`'s, which are nearly the same
// set. The bool is `\c`: it means "stop here, and stop the caller too".
//
// Assembled as bytes and decoded once at the end, because `\0303\0244`
// names the two bytes of `ä` rather than two Latin-1 characters --
// the same reason `$'...'` and the printf format are built this way.
pub(crate) fn echo_expand_escapes(s: &str) -> (String, bool) {
    backslash_escapes(s, false)
}

// The same, for `%b`, which takes a bare `\nnn` as octal where
// `echo -e` reads it as text. That difference is bash's, checked
// against it: `echo -e 'a\101b'` prints `a\101b` and
// `printf '%b' 'a\101b'` prints `aAb`.
pub(crate) fn printf_b_escapes(s: &str) -> (String, bool) {
    backslash_escapes(s, true)
}

fn backslash_escapes(s: &str, bare_octal: bool) -> (String, bool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chars = s.chars().peekable();
    let octal = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>, first: u32| -> u8 {
        let mut value = first;
        for _ in 0..2 {
            match chars.peek().and_then(|c| c.to_digit(8)) {
                Some(next) => {
                    value = value * 8 + next;
                    chars.next();
                }
                None => break,
            }
        }
        value as u8
    };
    while let Some(c) = chars.next() {
        if c != '\\' {
            push_char(&mut buf, c);
            continue;
        }
        match chars.next() {
            Some('\\') => push_char(&mut buf, '\\'),
            Some('a') => push_char(&mut buf, '\u{7}'),
            Some('b') => push_char(&mut buf, '\u{8}'),
            Some('c') => return (String::from_utf8_lossy(&buf).into_owned(), true),
            Some('e' | 'E') => push_char(&mut buf, '\u{1b}'),
            Some('f') => push_char(&mut buf, '\u{c}'),
            Some('n') => push_char(&mut buf, '\n'),
            Some('r') => push_char(&mut buf, '\r'),
            Some('t') => push_char(&mut buf, '\t'),
            Some('v') => push_char(&mut buf, '\u{b}'),
            // `\0nnn` in both, and a bare `\nnn` only where the caller
            // says so.
            Some('0') => {
                let first = chars.peek().and_then(|c| c.to_digit(8)).map(|d| {
                    chars.next();
                    d
                });
                buf.push(match first {
                    Some(d) => octal(&mut chars, d),
                    None => 0,
                });
            }
            Some(d @ '1'..='7') if bare_octal => {
                let first = d.to_digit(8).unwrap();
                buf.push(octal(&mut chars, first));
            }
            Some('x') => buf.push(read_hex_escape(&mut chars, 2) as u8),
            Some('u') => match char::from_u32(read_hex_escape(&mut chars, 4)) {
                Some(c) => push_char(&mut buf, c),
                None => push_char(&mut buf, '?'),
            },
            Some('U') => match char::from_u32(read_hex_escape(&mut chars, 8)) {
                Some(c) => push_char(&mut buf, c),
                None => push_char(&mut buf, '?'),
            },
            Some(other) => {
                push_char(&mut buf, '\\');
                push_char(&mut buf, other);
            }
            None => push_char(&mut buf, '\\'),
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), false)
}

// `printf %q`: the argument written as text a shell reads back as
// exactly this string. Bash's rules, byte for byte, because the whole
// point of the output is to survive being pasted somewhere else --
// including through something that does not know the encoding.
//
// Two forms. While every byte is printable ASCII, the shell-special
// ones each get a backslash. As soon as one is not -- a control
// character, or anything with the high bit set -- the whole string goes
// into `$'...'` and those bytes are written as octal escapes: `ä`
// becomes `$'\303\244'`, its own UTF-8 bytes, which is the spelling
// least likely to arrive somewhere as something else.
//
// `shell_quote` next door does the always-single-quote form instead,
// which is what `${v@Q}` wants. Bash draws the same distinction between
// the two.
pub(crate) fn printf_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let bytes = s.as_bytes();
    if bytes.iter().any(|b| !(0x20..0x7f).contains(b)) {
        return dollar_quote(bytes);
    }
    let mut out = String::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        // `#` opens a comment and `~` names a home directory only at
        // the front of a word, and bash escapes them only there.
        let front_only = matches!(b, b'#' | b'~');
        if BACKSLASH_QUOTED.contains(&b) && (!front_only || i == 0) {
            out.push('\\');
        }
        out.push(b as char);
    }
    out
}

// Every ASCII character bash puts a backslash in front of. `%`, `+`,
// `-`, `.`, `/`, `:`, `=`, `@`, `_` and the alphanumerics are the ones
// it leaves alone.
const BACKSLASH_QUOTED: &[u8] = b" !\"#$&'()*,;<>?[\\]^`{|}~";

// The `$'...'` form. Only `'` and `\` are special inside it -- `$`,
// `"` and a backtick are all literal there, which is most of why bash
// reaches for it.
fn dollar_quote(bytes: &[u8]) -> String {
    let mut out = String::from("$'");
    for &b in bytes {
        match b {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            0x0b => out.push_str("\\v"),
            0x0c => out.push_str("\\f"),
            b'\r' => out.push_str("\\r"),
            // `\E`, not `\e`: bash writes the one every shell reads.
            0x1b => out.push_str("\\E"),
            b'\'' => out.push_str("\\'"),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            other => out.push_str(&format!("\\{other:03o}")),
        }
    }
    out.push('\'');
    out
}

// Wraps `s` in single quotes, escaping any embedded single quote as
// '\'' (close, escaped-quote, reopen) -- the standard POSIX-shell-safe
// quoting form, and what printf's own %q conversion produces.
pub(crate) fn shell_quote(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod time_format_tests {
    use super::TimeStyle;

    fn formatted(style: TimeStyle, format: Option<&str>, real: f64, user: f64, sys: f64) -> String {
        let mut shell = super::Shell::new();
        // A global lives in the real process environment, so it
        // outlives the `Shell` that set it and would leak into the next
        // case here.
        match format {
            Some(f) => {
                shell.assign_var("TIMEFORMAT", f.to_string());
            }
            None => unsafe { std::env::remove_var("TIMEFORMAT") },
        }
        shell.format_times(style, real, user, sys)
    }

    // The numbers are what a corpus case cannot pin, so they are
    // pinned here instead. Every expectation was read off real bash
    // with the same inputs.
    #[test]
    fn the_default_shape_is_bashs_own() {
        assert_eq!(formatted(TimeStyle::Shell, None, 0.0, 0.0, 0.0), "\nreal\t0m0.000s\nuser\t0m0.000s\nsys\t0m0.000s\n");
        assert_eq!(
            formatted(TimeStyle::Shell, None, 65.5, 1.25, 0.5),
            "\nreal\t1m5.500s\nuser\t0m1.250s\nsys\t0m0.500s\n",
            "whole minutes, and no padding on the seconds"
        );
    }

    #[test]
    fn dash_p_is_posixs() {
        assert_eq!(formatted(TimeStyle::Posix, None, 1.5, 0.25, 0.0), "real 1.50\nuser 0.25\nsys 0.00\n");
        assert_eq!(
            formatted(TimeStyle::Posix, Some("ignored"), 1.0, 0.0, 0.0),
            "real 1.00\nuser 0.00\nsys 0.00\n",
            "TIMEFORMAT is not consulted for -p"
        );
    }

    #[test]
    fn timeformat_reads_the_specifiers_anyone_writes() {
        assert_eq!(formatted(TimeStyle::Shell, Some("R=%R U=%U S=%S"), 2.0, 1.0, 0.5), "R=2.000 U=1.000 S=0.500\n");
        assert_eq!(formatted(TimeStyle::Shell, Some("%1R"), 2.25, 0.0, 0.0), "2.2\n", "a digit sets the precision");
        assert_eq!(formatted(TimeStyle::Shell, Some("%lR"), 61.0, 0.0, 0.0), "1m1.000s\n", "`l` is the minutes-and-seconds spelling");
        assert_eq!(formatted(TimeStyle::Shell, Some("%P"), 2.0, 1.0, 0.5), "75.000\n", "cpu as a percentage of wall clock");
        assert_eq!(formatted(TimeStyle::Shell, Some("%P"), 0.0, 1.0, 0.5), "0.000\n", "and no division by a zero elapsed");
        assert_eq!(formatted(TimeStyle::Shell, Some("%%lit%%"), 0.0, 0.0, 0.0), "%lit%\n");
        assert_eq!(formatted(TimeStyle::Shell, Some("%Q"), 0.0, 0.0, 0.0), "%Q\n", "an unknown one is printed back");
    }
}

#[cfg(test)]
mod did_you_mean_tests {
    fn stderr_of(script: &str) -> String {
        let mut shell = super::Shell::new();
        let captured = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        shell.set_sink_capture(captured.clone());
        shell.run_source_here(script, "<did-you-mean>");
        let out = captured.borrow().clone();
        out
    }

    // The wiring, not the algorithm -- `suggest`'s own tests cover
    // which name is closest. What matters here is that the shell
    // actually says it, on the paths where a name is nearly right.
    #[test]
    fn a_mistyped_builtin_is_named() {
        assert!(stderr_of("ecoh hi\n").contains("did you mean 'echo'?"), "{}", stderr_of("ecoh hi\n"));
        assert!(stderr_of("expor X=1\n").contains("did you mean 'export'?"));
        // A name that is not nearly anything gets no guess.
        let miss = stderr_of("nosuchcommand_at_all\n");
        assert!(miss.contains("nosuchcommand_at_all"), "{miss}");
        assert!(!miss.contains("did you mean"), "{miss}");
    }

    #[test]
    fn a_mistyped_bish_subcommand_is_named() {
        assert!(stderr_of("::bish lps ls\n").contains("did you mean 'lsp'?"));
        assert!(stderr_of("::bish hook lls\n").contains("did you mean 'ls'?"));
        assert!(stderr_of("::bish theme bgein\n").contains("did you mean 'begin'?"));
        assert!(!stderr_of("::bish qqqq\n").contains("did you mean"));
    }

    #[test]
    fn a_mistyped_bishopt_name_is_named() {
        assert!(stderr_of("bishopt gitignor\n").contains("did you mean 'gitignore'?"));
    }

    // The "expected:" list and the suggestion read from the same array,
    // so a subcommand cannot exist in one and not the other.
    #[test]
    fn the_expected_list_is_the_list_that_is_searched() {
        let listed = stderr_of("::bish qqqq\n");
        for name in ["theme", "window", "hook", "hl", "lsp", "map"] {
            assert!(listed.contains(name), "{name} missing from {listed}");
        }
    }
}

#[cfg(test)]
mod subshell_inheritance_tests {
    // A subshell sees what the shell that made it sees. Everything
    // *around* the positional parameters was already inherited --
    // functions, arrays, aliases, the directory stack -- which is what
    // made `$(... "$1" ...)` coming back empty look like a lookup
    // problem rather than two fields left blank in one constructor.
    //
    // Every expectation here was checked against real bash first.
    fn output(script: &str, args: &[&str]) -> String {
        let mut shell = super::Shell::new();
        shell.arg_frames = vec![args.iter().map(|s| s.to_string()).collect()];
        let captured = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        shell.set_sink_capture(captured.clone());
        shell.run_source_here(script, "<subshell>");
        let out = captured.borrow().clone();
        out
    }

    #[test]
    fn a_command_substitution_sees_the_positional_parameters() {
        assert_eq!(output("printf '%s' \"$(printf '%s' \"$1\")\"\n", &["hello", "there"]), "hello");
        assert_eq!(output("printf '%s' \"$(printf '%s' \"$#\")\"\n", &["hello", "there"]), "2");
        assert_eq!(output("printf '%s' \"$(printf '%s' \"$*\")\"\n", &["hello", "there"]), "hello there");
    }

    #[test]
    fn a_command_substitution_inside_a_function_sees_that_functions_arguments() {
        let script = "f() { printf '%s' \"$(printf '%s' \"$1-$2-$#\")\"; }\nf a b\n";
        assert_eq!(output(script, &["outer"]), "a-b-2");
        // ...and what `shift` left behind, not what the call started
        // with.
        let shifted = "f() { shift; printf '%s' \"$(printf '%s' \"$1\")\"; }\nf one two\n";
        assert_eq!(output(shifted, &[]), "two");
    }

    #[test]
    fn a_command_substitution_sees_a_local_from_the_function_around_it() {
        let script = "f() { local x=inner; printf '%s' \"$(printf '%s' \"$x\")\"; }\nf\n";
        assert_eq!(output(script, &[]), "inner");
    }

    #[test]
    fn what_a_subshell_does_to_its_own_arguments_stays_there() {
        let script = "f() { printf '%s' \"$(shift; printf '%s' \"$1\")-$1\"; }\nf one two\n";
        assert_eq!(output(script, &[]), "two-one", "the shift is the subshell's own");
    }
}

#[cfg(test)]
mod quoting_round_trip_tests {
    // `%q` exists so a string can be carried somewhere else and read
    // back as itself. Bash's spelling is what bish writes -- including
    // `$'\303\244'` for a non-ASCII string, which is octal *bytes* and
    // therefore cannot be re-encoded into something else on the way --
    // so the two halves are checked separately: that bish writes what
    // bash writes, and that bish reads its own writing back.
    //
    // The values go to bash as arguments rather than inside the script,
    // which is the only way to hand it a control character without
    // needing the very quoting under test.
    fn nasty() -> Vec<String> {
        let mut out: Vec<String> = [
            "", "abc", "a b", "a'b", "a\"b", "a$b", "a\\b", "a`b", "#abc", "~abc", "a#b", "a~b", "a,b", "a^b", "a{b}c", "a[b]c", "a(b)c", "a?b",
            "a*b", "a;b", "a|b", "a&b", "a<b>c", "a!b", "a=b", "a.b", "a/b", "a:b", "a@b", "a+b", "a%b", "a-b", "a_b", "123", "~", "#",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Every control character, and a few that need real bytes.
        for code in 1u8..0x20 {
            out.push(format!("x{}y", code as char));
        }
        out.push("x\u{7f}y".to_string());
        out.push("\u{e4}\u{f6}\u{e5}".to_string());
        out.push("a \u{e4} b".to_string());
        out.push("\u{1f600}".to_string());
        out
    }

    #[test]
    fn printf_q_writes_what_bash_writes() {
        let values = nasty();
        let mut command = std::process::Command::new("bash");
        command.env("LC_ALL", "C").arg("-c").arg("for v in \"$@\"; do printf '%q' \"$v\"; printf '\\001'; done").arg("bash");
        for value in &values {
            command.arg(value);
        }
        let Ok(out) = command.output() else { return };
        if !out.status.success() {
            return;
        }
        let printed = String::from_utf8_lossy(&out.stdout).into_owned();
        let want: Vec<&str> = printed.split('\u{1}').collect();
        assert_eq!(want.len(), values.len() + 1, "one result per value, plus the empty tail");
        for (value, expected) in values.iter().zip(&want) {
            assert_eq!(&super::printf_quote(value), expected, "printf %q of {value:?}");
        }
    }

    #[test]
    fn what_printf_q_writes_reads_back_as_itself() {
        for value in nasty() {
            let quoted = super::printf_quote(&value);
            let mut shell = super::Shell::new();
            let captured = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
            shell.set_sink_capture(captured.clone());
            shell.run_source_here(&format!("printf '%s' {quoted}\n"), "<quote-round-trip>");
            assert_eq!(captured.borrow().as_str(), value, "{quoted} did not read back");
        }
    }
}

#[cfg(test)]
mod printf_conversion_tests {
    // Checked against real `bash`, on the same specs and the same
    // values, because C's conversions have more corners than anyone's
    // reading of them: `%.0f` of 2.5 is 2 and of 3.5 is 4 (round half
    // to even), `%g` picks its form from the exponent *after*
    // rounding, `%e` writes at least two exponent digits, zero padding
    // goes after the sign but not around an infinity, precision on an
    // integer means minimum digits rather than digits after the point
    // (and silences the `0` flag), `010` is octal, and an argument is
    // read as far as it parses rather than all-or-nothing, so `3.9abc`
    // is 3.9 and `12 34` is 12.
    //
    // Precision stays at or below 17 significant digits on purpose:
    // bash converts through `long double` and bish through `f64`, so
    // the two genuinely diverge past what a double can represent, and
    // there is no fixing that without an 80-bit float.
    //
    // `LC_ALL=C`, or this measures the machine's decimal separator
    // rather than the formatting -- a Finnish `bash` writes `3,14`.
    // Skipped where there is no bash, the same way the git and inflate
    // tests skip.
    fn cases() -> Vec<(String, String)> {
        let specs = [
            "%f", "%.0f", "%.2f", "%8.3f", "%-8.3f", "%05.2f", "%08.3f", "%+f", "% f", "%012.4f", "%e", "%E", "%.0e", "%.2e", "%012.1e", "%+.3e",
            "%08.0E", "%g", "%G", "%.1g", "%.3g", "%.10g", "%+08.0g", "%012g", "%d", "%i", "%5d", "%-5d", "%05d", "%+d", "% d", "%.5d", "%.0d",
            "%5.0d", "%08.5d", "%-8.5d", "%u", "%.5u", "%x", "%X", "%.3x", "%08x", "%o", "%.5o", "%12.4X", "%s", "%.3s", "%8.3s", "%-8.3s", "%c",
            "%b", "%.3b", "%q", "%.3q", "%8q",
        ];
        let values = [
            "0", "1", "-1", "2.5", "3.5", "-0.5", "3.14159", "-3.14159", "1e3", "1e-3", "1e20", "0.0001", "0.00001", "999999", "999999.5", "100000",
            "1000000", "0x10", "-0x10", "31415.9", "0.000123", "inf", "-inf", "nan", "abc", "", "42", "-42", "010", "0XfF", "'A", "+5", " 7 ", "3.9",
            "-3.9", "12 34", "3.9abc", ".5", "1.", "255", "-255", "abcdef", "a b", "a'b", "a\"b", "a$b", "a\\b", "a`b", "a*b", "a;b", "a|b", "a&b",
            "a<b", "a>b", "a#b", "#abc", "abc#", "a~b", "~abc", "a!b", "a,b", "a^b", "a{b", "a[b", "a(b", "a?b", "a=b", "a.b", "a-b", "a_b", "a/b",
            "a:b", "a@b", "a+b", "a%b", "~", "#",
        ];
        specs.iter().flat_map(|s| values.iter().map(move |v| (s.to_string(), v.to_string()))).collect()
    }

    #[test]
    fn conversions_agree_with_bash() {
        let cases = cases();
        // Separated by SOH rather than by newline: `%b` can expand an
        // escape into a newline and `%c` of nothing at all is a NUL, so
        // one-result-per-line is not a protocol either side can keep.
        let script: String = cases
            .iter()
            .map(|(spec, value)| {
                // `'A` is one of the values, so the values cannot go
                // into the script unescaped.
                let (spec, value) = (spec.replace('\'', "'\\''"), value.replace('\'', "'\\''"));
                format!("printf '{spec}' '{value}'; printf '\\001'\n")
            })
            .collect();
        let out = std::process::Command::new("bash").env("LC_ALL", "C").arg("-c").arg(&script).output();
        let Ok(out) = out else { return };
        if !out.status.success() {
            return;
        }
        // bash writes "invalid number" to stderr for a value it cannot
        // read and then uses 0, which is what bish does silently.
        let printed = String::from_utf8_lossy(&out.stdout).into_owned();
        let want: Vec<&str> = printed.split('\u{1}').collect();
        assert_eq!(want.len(), cases.len() + 1, "one result per case, plus the empty tail");

        for ((spec, value), expected) in cases.iter().zip(&want) {
            let mut got = String::new();
            let mut idx = 0;
            let _ = super::printf_format_once(spec, std::slice::from_ref(value), &mut idx, &mut got);
            assert_eq!(&got, expected, "printf '{spec}' '{value}'");
        }
    }
}

// `printf`'s own idea of an integer, which is C's `strtoll` with base 0:
// the longest valid prefix of the argument, in whatever base it
// announces. `3.9` is 3 and `1e3` is 1 -- both stop at the first
// character that cannot continue the number rather than failing -- while
// `010` is octal 8 and `0x10` is hexadecimal 16. Anything with no valid
// prefix at all is 0, and an overflow saturates, both of which bash
// warns about and then does anyway.
//
// `'A` (or `"A`) is bash's own extension on top of that: a quote
// followed by a character means that character's code point.
// Whether printf's own number reading consumed the whole argument --
// which is what bash means by "invalid number". Leading whitespace is
// skipped, the way strtol does; anything left over after the digits is
// not, so `12 ` and `1e3` are both rejected for an integer conversion
// while ` 12` is fine. bash's `'X` (the character's code point) is a
// number by this measure too.
fn printf_number_complete(arg: &str, float: bool) -> bool {
    let text = arg.trim_start();
    if text.starts_with(['\'', '"']) {
        return true;
    }
    if text.is_empty() {
        return false;
    }
    let rest = text.strip_prefix(['-', '+']).unwrap_or(text);
    if float {
        if rest.eq_ignore_ascii_case("inf") || rest.eq_ignore_ascii_case("infinity") || rest.eq_ignore_ascii_case("nan") {
            return true;
        }
        if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
            return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
        }
        return float_prefix_len(text) == text.len() && float_prefix_len(text) > 0;
    }
    let (radix, digits) = int_radix(rest);
    !digits.is_empty() && digits.chars().all(|c| c.is_digit(radix))
}

fn printf_int(arg: &str) -> i64 {
    let text = arg.trim();
    if let Some(rest) = text.strip_prefix(['\'', '"']) {
        return rest.chars().next().map(|c| c as i64).unwrap_or(0);
    }
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (radix, digits) = int_radix(rest);
    let valid = digits.len() - digits.trim_start_matches(|c: char| c.is_digit(radix)).len();
    match i64::from_str_radix(&digits[..valid], radix) {
        Ok(v) if negative => -v,
        Ok(v) => v,
        // No digits at all reads as zero; too many of them saturate at
        // whichever end, which is what `strtoll` does with ERANGE.
        Err(_) if valid == 0 => 0,
        Err(_) if negative => i64::MIN,
        Err(_) => i64::MAX,
    }
}

// The same, for the conversions that read their argument as unsigned:
// `%u`, `%o`, `%x`, `%X`. A negative argument wraps (`-42` is
// `ffffffffffffffd6`), and anything too large for 64 bits saturates
// whichever sign it had.
fn printf_uint(arg: &str) -> u64 {
    let text = arg.trim();
    if let Some(rest) = text.strip_prefix(['\'', '"']) {
        return rest.chars().next().map(|c| c as u64).unwrap_or(0);
    }
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (radix, digits) = int_radix(rest);
    let valid = digits.len() - digits.trim_start_matches(|c: char| c.is_digit(radix)).len();
    match u64::from_str_radix(&digits[..valid], radix) {
        Ok(v) if negative => 0u64.wrapping_sub(v),
        Ok(v) => v,
        Err(_) if valid == 0 => 0,
        Err(_) => u64::MAX,
    }
}

// The base an argument announces: `0x` for hexadecimal, a leading `0`
// for octal, decimal otherwise -- `strtol`'s base 0.
fn int_radix(rest: &str) -> (u32, &str) {
    match rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        Some(hex) => (16, hex),
        None => match rest.len() > 1 && rest.starts_with('0') {
            true => (8, &rest[1..]),
            false => (10, rest),
        },
    }
}

// The `%d`/`%x`/`%o` family's own reading of precision: a *minimum*
// number of digits, zero-filled, rather than the "digits after the
// point" it means for a float. Zero printed at precision zero is
// nothing at all -- `%.0d` of 0 is the empty string, which is how a C
// programmer writes "this column is blank when there is nothing in it".
fn pad_integer_digits(piece: String, precision: usize) -> String {
    let (sign, digits) = match piece.starts_with('-') {
        true => piece.split_at(1),
        false => ("", piece.as_str()),
    };
    if digits == "0" && precision == 0 {
        return sign.to_string();
    }
    let fill = precision.saturating_sub(digits.chars().count());
    format!("{sign}{}{digits}", "0".repeat(fill))
}

// `printf`'s own idea of a number, which is C's `strtod`: a decimal or
// exponent form, a `0x` integer, surrounding space, `inf`/`nan`. `None`
// for anything else, which every caller renders as zero.
fn printf_float(arg: &str) -> f64 {
    let text = arg.trim();
    // bash's `'X` extension is not just for the integer conversions.
    if let Some(rest) = text.strip_prefix(['\'', '"']) {
        return rest.chars().next().map(|c| c as u32 as f64).unwrap_or(0.0);
    }
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, text.strip_prefix('+').unwrap_or(text)),
    };
    if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).map(|v| sign * v as f64).unwrap_or(0.0);
    }
    if rest.eq_ignore_ascii_case("inf") || rest.eq_ignore_ascii_case("infinity") {
        return sign * f64::INFINITY;
    }
    if rest.eq_ignore_ascii_case("nan") {
        return f64::NAN;
    }
    text[..float_prefix_len(text)].parse::<f64>().unwrap_or(0.0)
}

// How much of `text` is a number, the way `strtod` measures it: an
// optional sign, digits with an optional point among them, and an
// optional exponent that only counts if it has digits of its own. `12
// 34` is 12 and `3.9abc` is 3.9; `.` and `abc` are nothing.
fn float_prefix_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mut saw_digit = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        saw_digit = true;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            saw_digit = true;
        }
    }
    if !saw_digit {
        return 0;
    }
    let mantissa_end = i;
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let digits_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        // An `e` with nothing after it is not part of the number.
        i = if j > digits_start { j } else { mantissa_end };
    }
    i
}

// Precision on a string conversion is a maximum length. Cut on a
// character rather than a byte: `String::truncate` panics on a boundary
// it does not own, and `printf '%.1s'` on a string of two-byte
// characters used to take the whole shell down with it.
fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

// `inf`/`nan` print as themselves, in the case the conversion asked
// for, with no digits and no precision applied.
fn non_finite(v: f64, upper: bool) -> Option<String> {
    if v.is_finite() {
        return None;
    }
    let word = match (v.is_nan(), v.is_sign_negative()) {
        (true, _) => "nan",
        (false, true) => "-inf",
        (false, false) => "inf",
    };
    Some(if upper { word.to_uppercase() } else { word.to_string() })
}

// `%f`.
fn format_fixed(v: f64, precision: usize, upper: bool) -> String {
    non_finite(v, upper).unwrap_or_else(|| format!("{v:.precision$}"))
}

// `%e`. Rust's own `{:e}` is close but not the same: it writes
// `3.14159e4` where C writes `3.141590e+04` -- an explicit sign, and at
// least two exponent digits.
fn format_exponential(v: f64, precision: usize, upper: bool) -> String {
    if let Some(word) = non_finite(v, upper) {
        return word;
    }
    let formatted = format!("{v:.precision$e}");
    let (mantissa, exponent) = formatted.split_once('e').unwrap_or((formatted.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let sign = if exponent < 0 { '-' } else { '+' };
    let out = format!("{mantissa}e{sign}{:02}", exponent.abs());
    if upper { out.to_uppercase() } else { out }
}

// `%g`: whichever of `%e` and `%f` is shorter for this value, with
// trailing zeros removed. Precision counts *significant* digits here
// rather than digits after the point, and 0 means 1.
//
// The choice is made on the exponent the value has *after* rounding to
// that many significant digits, not before -- which is why 999999.5 at
// the default precision is `1e+06` and not `1000000`. Asking Rust for
// the `%e` form first and reading the exponent back off it is the
// cheapest way to get that right.
fn format_general(v: f64, precision: usize, upper: bool) -> String {
    if let Some(word) = non_finite(v, upper) {
        return word;
    }
    let significant = precision.max(1);
    let rounded = format!("{v:.*e}", significant - 1);
    let exponent: i32 = rounded.split_once('e').and_then(|(_, e)| e.parse().ok()).unwrap_or(0);
    let out = match exponent < -4 || exponent >= significant as i32 {
        true => format_exponential(v, significant - 1, upper),
        false => format_fixed(v, (significant as i32 - 1 - exponent).max(0) as usize, upper),
    };
    trim_trailing_zeros(&out)
}

// The `%g` cleanup, applied to the digits only: `1.230000e-05` becomes
// `1.23e-05`, `100.000` becomes `100`, and a point left with nothing
// after it goes too.
fn trim_trailing_zeros(text: &str) -> String {
    let (digits, suffix) = match text.find(['e', 'E']) {
        Some(at) => text.split_at(at),
        None => (text, ""),
    };
    if !digits.contains('.') {
        return text.to_string();
    }
    let digits = digits.trim_end_matches('0').trim_end_matches('.');
    format!("{digits}{suffix}")
}

// Runs FORMAT once against `values[*idx..]`, advancing `*idx` past
// however many of them it actually consumed, and appending the result
// to `out` -- see run_printf's own doc comment for the conversions and
// flags supported. Split out from run_printf so the caller can call
// this repeatedly to cycle FORMAT over more arguments than it has
// conversions for.
// One character as its UTF-8 bytes, for a buffer that is assembled as
// bytes so a `\nnn` escape can name one.
fn push_char(buf: &mut Vec<u8>, c: char) {
    let mut bytes = [0u8; 4];
    buf.extend_from_slice(c.encode_utf8(&mut bytes).as_bytes());
}

// Up to `max` hexadecimal digits. Fewer is fine -- `\x7` is a complete
// escape -- which is why this peeks rather than demands.
fn read_hex_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, max: usize) -> u32 {
    let mut value = 0u32;
    for _ in 0..max {
        match chars.peek().and_then(|c| c.to_digit(16)) {
            Some(digit) => {
                value = value * 16 + digit;
                chars.next();
            }
            None => break,
        }
    }
    value
}

// What one pass over a printf format produced besides its output.
// Collected rather than printed here because this is a free function
// with no shell to print through -- run_printf reports them in order.
#[derive(Default)]
pub(crate) struct PrintfOutcome {
    // Stop rerunning the format over the remaining arguments: either a
    // `\c` in a `%b` argument, or an error that ended the pass.
    pub(crate) stop: bool,
    pub(crate) errors: Vec<String>,
    pub(crate) status: i32,
}

// The C length modifiers. bash skips them -- every integer here is
// already 64-bit -- but a format that carries one (`%zu`, `%lld`) has
// to reach the conversion character behind it rather than reading the
// modifier *as* the conversion. `q` (C's quad) is deliberately absent:
// `%q` is bash's own shell-quoting conversion, and that is what a
// script writing it means.
const PRINTF_LENGTH_MODIFIERS: &str = "hlLjzt";

pub(crate) fn printf_format_once(format: &str, values: &[String], idx: &mut usize, out: &mut String) -> PrintfOutcome {
    // Assembled as bytes and decoded once at the end, because `\303\244`
    // has to mean the two bytes of `ä` rather than two Latin-1
    // characters -- the same reason `$'...'` is built this way.
    let mut buf: Vec<u8> = Vec::new();
    let mut outcome = PrintfOutcome::default();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => push_char(&mut buf, '\\'),
                Some('a') => push_char(&mut buf, '\u{7}'),
                Some('b') => push_char(&mut buf, '\u{8}'),
                Some('e') => push_char(&mut buf, '\u{1b}'),
                Some('f') => push_char(&mut buf, '\u{c}'),
                Some('n') => push_char(&mut buf, '\n'),
                Some('r') => push_char(&mut buf, '\r'),
                Some('t') => push_char(&mut buf, '\t'),
                Some('v') => push_char(&mut buf, '\u{b}'),
                Some('"') => push_char(&mut buf, '"'),
                // `\nnn` names a *byte*, in octal, up to three digits
                // counting the one just read. `\0nnn` is the same thing
                // -- `0` is an octal digit.
                Some(d @ '0'..='7') => {
                    let mut value = d.to_digit(8).unwrap();
                    for _ in 0..2 {
                        match chars.peek().and_then(|c| c.to_digit(8)) {
                            Some(next) => {
                                value = value * 8 + next;
                                chars.next();
                            }
                            None => break,
                        }
                    }
                    buf.push(value as u8);
                }
                Some('x') => buf.push(read_hex_escape(&mut chars, 2) as u8),
                // `\u`/`\U` name a code point rather than a byte, so
                // they go in as UTF-8.
                Some(u @ ('u' | 'U')) => {
                    let width = if u == 'u' { 4 } else { 8 };
                    match char::from_u32(read_hex_escape(&mut chars, width)) {
                        Some(c) => push_char(&mut buf, c),
                        None => push_char(&mut buf, '?'),
                    }
                }
                Some(other) => {
                    push_char(&mut buf, '\\');
                    push_char(&mut buf, other);
                }
                None => push_char(&mut buf, '\\'),
            }
            continue;
        }
        if c != '%' {
            push_char(&mut buf, c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            push_char(&mut buf, '%');
            continue;
        }

        // bash's `%(FORMAT)T`: the parenthesised part is a strftime
        // format and the argument is a Unix timestamp, with `-1` (or no
        // argument) meaning now. Read here, before the flag/width
        // parsing below, because `(` is where the conversion's own
        // syntax starts and none of those flags apply to it.
        if chars.peek() == Some(&'(') {
            chars.next();
            let mut time_fmt = String::new();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == ')' {
                    closed = true;
                    break;
                }
                time_fmt.push(c);
            }
            // `%(unterminated` is not a time conversion at all; print it
            // back the way an unknown conversion is printed back.
            if !closed || chars.next() != Some('T') {
                // Not a time conversion after all. Print it back the way
                // an unknown conversion is printed back -- but through
                // the escape expander, since the characters swallowed
                // looking for the `)` never reached the `\n`/`\t`
                // handling at the top of this loop and would otherwise
                // come out as a literal backslash. bash calls this an
                // invalid time format and warns; bish just prints it.
                buf.extend_from_slice("%(".as_bytes());
                buf.extend_from_slice(&echo_expand_escapes(&time_fmt).0.as_bytes());
                if closed {
                    push_char(&mut buf, ')');
                }
                continue;
            }
            let arg = values.get(*idx).cloned().unwrap_or_default();
            if !arg.is_empty() {
                *idx += 1;
            }
            let secs = match arg.trim().parse::<i64>() {
                // bash: -1 is now, -2 is when the shell started. bish
                // has no start-time to report, so both read as now --
                // stated rather than silently wrong.
                Ok(n) if n >= 0 => n,
                _ => unix_now().as_secs() as i64,
            };
            buf.extend_from_slice(&crate::time::strftime_at(&time_fmt, &crate::time::local_time_at(secs), Some(secs)).as_bytes());
            continue;
        }

        let mut left_align = false;
        let mut zero_pad = false;
        let mut plus_sign = false;
        let mut space_sign = false;
        while let Some(&p) = chars.peek() {
            match p {
                '-' => {
                    left_align = true;
                    chars.next();
                }
                '0' => {
                    zero_pad = true;
                    chars.next();
                }
                // `+` always shows a sign, ` ` shows a space where a
                // `+` would go -- both only mean anything for a signed
                // numeric conversion, which is where they are applied.
                '+' => {
                    plus_sign = true;
                    chars.next();
                }
                ' ' => {
                    space_sign = true;
                    chars.next();
                }
                _ => break,
            }
        }
        // `%*d` takes the width from an argument, `%.*f` the
        // precision. Both are how a script prints a column whose width
        // it computed; neither was recognised, so `%*d` reported a
        // missing format character.
        let mut width_digits = String::new();
        let mut star_width: Option<usize> = None;
        if chars.peek() == Some(&'*') {
            chars.next();
            let v = values.get(*idx).cloned().unwrap_or_default();
            *idx += 1;
            star_width = Some(printf_int(&v).unsigned_abs() as usize);
            // A negative `*` width means left-aligned, as `-` does.
            if printf_int(&v) < 0 {
                left_align = true;
            }
        }
        while star_width.is_none() {
            match chars.peek() {
                Some(&p) if p.is_ascii_digit() => {
                    width_digits.push(p);
                    chars.next();
                }
                _ => break,
            }
        }
        let width: usize = star_width.unwrap_or_else(|| width_digits.parse().unwrap_or(0));
        let mut precision: Option<usize> = None;
        if chars.peek() == Some(&'.') {
            chars.next();
            if chars.peek() == Some(&'*') {
                chars.next();
                let v = values.get(*idx).cloned().unwrap_or_default();
                *idx += 1;
                precision = Some(printf_int(&v).max(0) as usize);
            }
            let mut p = String::new();
            while precision.is_none() {
                let Some(&d) = chars.peek() else { break };
                if d.is_ascii_digit() {
                    p.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            if precision.is_none() {
                precision = Some(p.parse().unwrap_or(0));
            }
        }
        // C's length modifiers (`%zu`, `%lld`): skipped, since every
        // integer here is already 64-bit -- but skipped rather than
        // mistaken for the conversion character itself.
        let mut modifiers = String::new();
        while let Some(&m) = chars.peek() {
            if PRINTF_LENGTH_MODIFIERS.contains(m) {
                modifiers.push(m);
                chars.next();
            } else {
                break;
            }
        }
        let spec_so_far = |extra: &str| -> String {
            let mut spec = String::from("%");
            if left_align {
                spec.push('-');
            }
            if zero_pad {
                spec.push('0');
            }
            if plus_sign {
                spec.push('+');
            }
            if space_sign {
                spec.push(' ');
            }
            spec.push_str(&width_digits);
            if let Some(p) = precision {
                spec.push('.');
                spec.push_str(&p.to_string());
            }
            spec.push_str(&modifiers);
            spec.push_str(extra);
            spec
        };
        let Some(conv) = chars.next() else {
            // `printf '%'` / `printf '%5'`: a conversion that never
            // named what to convert. bash reports it and stops.
            outcome.errors.push(format!("`{}': missing format character", spec_so_far("")));
            outcome.status = 1;
            outcome.stop = true;
            out.push_str(&String::from_utf8_lossy(&buf));
            return outcome;
        };
        if !"sbqcdiuoxXfFeEgG".contains(conv) {
            // An unrecognized conversion is the whole format being
            // wrong, not a character to print back -- bash abandons
            // the printf there, and so does this now.
            outcome.errors.push(format!("`{}': missing format character", spec_so_far(&conv.to_string())));
            outcome.status = 1;
            outcome.stop = true;
            out.push_str(&String::from_utf8_lossy(&buf));
            return outcome;
        }

        let mut next_arg = || -> String {
            let v = values.get(*idx).cloned().unwrap_or_default();
            *idx += 1;
            v
        };
        // Collected beside `outcome` rather than into it: `next_arg`
        // already holds a mutable borrow across this match.
        let mut errors: Vec<String> = Vec::new();
        let mut status = 0;
        // What zero-padding and the sign flags apply to. Floats are in
        // both; `u`/`o`/`x`/`X` take the padding but not a sign, which
        // is what C does with them.
        let numeric = matches!(conv, 'd' | 'i' | 'u' | 'o' | 'x' | 'X' | 'f' | 'F' | 'e' | 'E' | 'g' | 'G');
        let signed = matches!(conv, 'd' | 'i' | 'f' | 'F' | 'e' | 'E' | 'g' | 'G');
        // Zero padding needs digits to sit beside; an infinity or a
        // NaN has none, so C pads those with spaces however the flags
        // read. Decided from the value rather than from the text --
        // `1.0e+20` and `ff` both have letters in them and both pad
        // with zeros perfectly happily.
        let mut zero_pad_ok = true;
        let mut piece = match conv {
            // The three that precision measures in characters rather
            // than in digits.
            's' => truncate_chars(&next_arg(), precision.unwrap_or(usize::MAX)),
            'b' => {
                let (expanded, stop) = printf_b_escapes(&next_arg());
                let piece = truncate_chars(&expanded, precision.unwrap_or(usize::MAX));
                if stop {
                    // `\c` in a `%b` argument ends the output -- all of
                    // it, not just this conversion.
                    buf.extend_from_slice(piece.as_bytes());
                    out.push_str(&String::from_utf8_lossy(&buf));
                    outcome.stop = true;
                    return outcome;
                }
                piece
            }
            'q' => truncate_chars(&printf_quote(&next_arg()), precision.unwrap_or(usize::MAX)),
            // An empty argument is still a character: bash writes the
            // NUL it read, and so does this.
            'c' => next_arg().chars().next().unwrap_or('\0').to_string(),
            'd' | 'i' | 'u' | 'o' | 'x' | 'X' => {
                let arg = next_arg();
                // bash still prints whatever prefix of the argument
                // *was* a number (`%d` of `1x` is 1) -- but it says so.
                if !printf_number_complete(&arg, false) {
                    errors.push(format!("{}: invalid number", arg));
                    status = 1;
                }
                match conv {
                    'd' | 'i' => printf_int(&arg).to_string(),
                    'u' => printf_uint(&arg).to_string(),
                    'o' => format!("{:o}", printf_uint(&arg)),
                    'x' => format!("{:x}", printf_uint(&arg)),
                    _ => format!("{:X}", printf_uint(&arg)),
                }
            }
            // The floating-point family. Precision defaults to 6, as C
            // does, and an argument that is not a number reads as 0 --
            // the same silent fallback `%d` above already makes (bash
            // warns and then uses 0; bish has never warned for `%d`
            // either, and one behaviour for both is worth more here
            // than half-matching).
            'f' | 'F' | 'e' | 'E' | 'g' | 'G' => {
                let arg = next_arg();
                if !printf_number_complete(&arg, true) {
                    errors.push(format!("{}: invalid number", arg));
                    status = 1;
                }
                let value = printf_float(&arg);
                zero_pad_ok = value.is_finite();
                let precision = precision.unwrap_or(6);
                match conv {
                    'f' | 'F' => format_fixed(value, precision, conv == 'F'),
                    'e' | 'E' => format_exponential(value, precision, conv == 'E'),
                    _ => format_general(value, precision, conv == 'G'),
                }
            }
            // Excluded by the check above.
            other => unreachable!("unhandled printf conversion {other:?}"),
        };
        // Precision on an integer means minimum digits, and having asked
        // for those the `0` flag has nothing left to say -- C ignores it
        // there, so `%08.5d` of 42 is `   00042` rather than `00000042`.
        let integer = matches!(conv, 'd' | 'i' | 'u' | 'o' | 'x' | 'X');
        if integer && let Some(p) = precision {
            piece = pad_integer_digits(piece, p);
            zero_pad = false;
        }
        if signed && !piece.starts_with('-') {
            if plus_sign {
                piece.insert(0, '+');
            } else if space_sign {
                piece.insert(0, ' ');
            }
        }
        let len = piece.chars().count();
        if len < width {
            let pad = width - len;
            // Zero padding goes *after* the sign -- `%05d` of -42 is
            // `-0042`, not `00-42` -- and C does not zero-pad an
            // infinity or a NaN at all, since there are no digits for
            // the zeros to be part of.
            let padded_with_zeros = zero_pad && numeric && zero_pad_ok;
            if left_align {
                piece.push_str(&" ".repeat(pad));
            } else if padded_with_zeros {
                let (sign, digits) = match piece.starts_with(['-', '+', ' ']) {
                    true => piece.split_at(1),
                    false => ("", piece.as_str()),
                };
                piece = format!("{sign}{}{digits}", "0".repeat(pad));
            } else {
                piece = format!("{}{}", " ".repeat(pad), piece);
            }
        }
        outcome.errors.append(&mut errors);
        if status != 0 {
            outcome.status = status;
        }
        buf.extend_from_slice(&piece.as_bytes());
    }
    out.push_str(&String::from_utf8_lossy(&buf));
    outcome
}

// `read`'s view of a file descriptor: never reads a byte it was not
// asked for.
//
// Any buffering here is visible behaviour, not an implementation
// detail. `{ read -r x; cat; }` must give the first line to `read` and
// the rest to `cat`, and `cat` is a different process inheriting the
// same fd -- so anything `read` pulled into a userspace buffer is gone.
// `BufReader::new(stdin())` lost it at the end of the call;
// `stdin().lock()` kept it inside this process, which is right for a
// `while read` loop in one shell and still wrong the moment the fd is
// handed on. bash reads a byte at a time from a pipe for exactly this
// reason, and so does this.
struct UnbufferedFd {
    fd: i32,
    byte: [u8; 1],
    filled: bool,
}

impl UnbufferedFd {
    fn new(fd: i32) -> Self {
        UnbufferedFd { fd, byte: [0], filled: false }
    }

    // Whether the descriptor is open at all. `read -u 9` on a fd
    // nothing opened has to say so; reading it just returns EBADF, and
    // an unread line and a closed fd are otherwise the same "no input"
    // from the caller's side.
    fn is_open(fd: i32) -> bool {
        unsafe extern "C" {
            // Same three-argument shape the rest of this codebase
            // declares it with (see pty.rs); F_GETFD ignores the third.
            fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
        }
        // F_GETFD
        unsafe { fcntl(fd, 1, 0) != -1 }
    }
}

impl std::io::Read for UnbufferedFd {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        use std::io::BufRead;
        if out.is_empty() {
            return Ok(0);
        }
        if self.fill_buf()?.is_empty() {
            return Ok(0);
        }
        out[0] = self.byte[0];
        self.consume(1);
        Ok(1)
    }
}

impl std::io::BufRead for UnbufferedFd {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if !self.filled {
            // Straight to the fd, not through `std::io::stdin()`, whose
            // own shared buffer is the thing being avoided.
            loop {
                // Asked before reading rather than made non-blocking
                // around it. This reads exactly one byte, so "the
                // descriptor has something" is precisely the condition
                // under which a blocking read cannot block -- one
                // syscall to find out instead of the three it takes to
                // set the flag and put it back, on the hottest path
                // there is: `while read` comes through here per byte.
                if crate::coroutine::in_coroutine() {
                    while !crate::poll::poll_readable_or_eof(self.fd, 0) {
                        crate::scheduler::park_readable(self.fd);
                    }
                }
                let n = unsafe { libc_read(self.fd, self.byte.as_mut_ptr(), 1) };
                if n == 1 {
                    self.filled = true;
                    break;
                }
                if n == 0 {
                    return Ok(&[]);
                }
                let e = std::io::Error::last_os_error();
                match e.kind() {
                    // Nothing there *yet*: the stage upstream has not
                    // written it, and that stage is a coroutine sharing
                    // this thread. Only a pipe between two in-process
                    // stages is ever non-blocking, so outside one of
                    // those this never happens.
                    std::io::ErrorKind::WouldBlock => crate::scheduler::park_readable(self.fd),
                    std::io::ErrorKind::Interrupted => {}
                    _ => return Err(e),
                }
            }
        }
        Ok(&self.byte[..1])
    }

    fn consume(&mut self, amount: usize) {
        if amount > 0 {
            self.filled = false;
        }
    }
}

unsafe extern "C" {
    #[link_name = "read"]
    fn libc_read(fd: i32, buf: *mut u8, count: usize) -> isize;
}

// bash's exit-status convention for a process killed by a signal is
// 128+signum (ExitStatus::code() returns None in that case -- there's no
// normal exit code to report -- so this falls back to the signal via
// ExitStatusExt, matching what `$?`/`wait`/`fg` should actually show).
// `${list:off:len}` over elements. A negative offset counts from the
// end; a negative *length* is an error for a list, where for a string
// bash reads it as "stop this far from the end" -- one of the few
// places the two genuinely disagree, and this follows bash on both.
fn slice_elements(items: Vec<String>, offset: i64, length: Option<i64>) -> Result<Vec<String>, String> {
    let count = items.len() as i64;
    let start = match offset < 0 {
        true => (count + offset).max(0),
        false => offset.min(count),
    };
    let end = match length {
        None => count,
        Some(n) if n < 0 => return Err(format!("{n}: substring expression < 0")),
        Some(n) => (start + n).min(count),
    };
    Ok(items.into_iter().skip(start as usize).take((end - start).max(0) as usize).collect())
}

// The CPU a shell's children have used so far, in seconds. `times(2)`
// rather than `getrusage`, because it is the same call the `times`
// builtin already makes and it reports exactly the two numbers `time`
// wants.
fn child_cpu_times() -> (f64, f64) {
    #[repr(C)]
    struct Tms {
        utime: i64,
        stime: i64,
        cutime: i64,
        cstime: i64,
    }
    unsafe extern "C" {
        fn times(buf: *mut Tms) -> i64;
        fn sysconf(name: i32) -> i64;
    }
    const SC_CLK_TCK: i32 = 2;
    let mut tms = Tms { utime: 0, stime: 0, cutime: 0, cstime: 0 };
    if unsafe { times(&mut tms as *mut Tms) } == -1 {
        return (0.0, 0.0);
    }
    let ticks = unsafe { sysconf(SC_CLK_TCK) }.max(1) as f64;
    (tms.cutime as f64 / ticks, tms.cstime as f64 / ticks)
}

fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| status.signal().map(|s| 128 + s).unwrap_or(1))
}

// Shared by the `read` builtin's two sources (a Box<dyn BufRead> from
// read_input_source, or a borrowed coproc fd via `-u`) -- factored out
// specifically so the `-u` path's borrow of self.coproc_fds can end right
// after this call, before the caller needs `&mut self` again for
// assign_var. Real bash: a line/char-count read that runs into EOF before
// seeing its delimiter (or before nchars is reached) still populates the
// variable(s) with whatever partial data it got, but returns non-zero --
// `clean` (the bool) tracks which case this was.
// `raw` is `read` *without* `-r`, where a backslash escapes the next
// character -- including the delimiter, which then continues the line
// rather than ending it. The escapes themselves are removed later (see
// unescape_read_line), because the splitting has to know which
// characters they protected.
fn read_line_or_chars(reader: &mut dyn std::io::BufRead, nchars: Option<usize>, delim: u8, raw: bool) -> (Option<String>, bool) {
    if let Some(n) = nchars {
        let mut buf = Vec::with_capacity(n);
        let mut hit_eof = false;
        for _ in 0..n {
            let mut b = [0u8; 1];
            match std::io::Read::read(reader, &mut b) {
                Ok(0) => {
                    hit_eof = true;
                    break;
                }
                Ok(_) => buf.push(b[0]),
                Err(_) => {
                    hit_eof = true;
                    break;
                }
            }
        }
        if buf.is_empty() && hit_eof { (None, false) } else { (Some(String::from_utf8_lossy(&buf).into_owned()), !hit_eof) }
    } else {
        let mut buf: Vec<u8> = Vec::new();
        let mut any = false;
        let mut hit_delim = false;
        loop {
            let before = buf.len();
            match reader.read_until(delim, &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    any = true;
                    hit_delim = buf.last() == Some(&delim);
                    if hit_delim {
                        buf.pop();
                        if delim == b'\n' && buf.last() == Some(&b'\r') {
                            buf.pop();
                        }
                    }
                    // A line ending in an *unescaped* backslash is not
                    // finished: the backslash escaped the delimiter, so
                    // both go and the next line joins this one.
                    let trailing = buf[before..].iter().rev().take_while(|b| **b == b'\\').count();
                    if !raw && hit_delim && trailing % 2 == 1 {
                        buf.pop();
                        continue;
                    }
                    break;
                }
                Err(_) => break,
            }
        }
        if !any { (None, false) } else { (Some(String::from_utf8_lossy(&buf).into_owned()), hit_delim) }
    }
}

// Removes `read`'s backslash escapes, and says which characters they
// protected: `escaped[i]` is true for a character that a backslash
// preceded, and such a character is never a field separator however it
// looks. `read a b <<< 'x\ y z'` gives a=`x y`, b=`z`.
fn unescape_read_line(line: &str) -> (String, Vec<bool>) {
    let mut out = String::with_capacity(line.len());
    let mut mask = Vec::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
                mask.push(true);
            }
            continue;
        }
        out.push(c);
        mask.push(false);
    }
    (out, mask)
}

/// The three `trap` targets that are not signals -- the interpreter
/// fires each itself, at a point it already passes through.
/// bash's own name for it, spelled exactly as bash spells it -- a
/// distribution's integration script defines this name and no other.
const COMMAND_NOT_FOUND_HANDLER: &str = "command_not_found_handle";

/// The three `trap` targets that are not signals -- the interpreter
/// fires each itself, at a point it already passes through.
// Wall-clock time since the epoch. `SystemTime` can technically be
// before it if the clock is badly wrong; that reads as 0 rather than
// panicking, since a shell variable is not the place to raise it.
fn unix_now() -> std::time::Duration {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
}

/// The three `trap` targets that are not signals -- the interpreter
/// fires each itself, at a point it already passes through.
#[derive(Clone, Copy, PartialEq, Eq)]
// The discriminants index pseudo_trap_depth.
#[repr(usize)]
enum PseudoTrap {
    Debug = 0,
    Err = 1,
    Return = 2,
}

#[derive(Clone)]
enum TrapAction {
    Ignore,
    Run(String),
}

// dup2(1, 2) in the child, after fork but before exec -- so fd 2 becomes a
// genuine duplicate of whatever fd 1 was just set up to be (Command's own
// stdio setup runs before pre_exec hooks). A single async-signal-safe
// syscall with no allocation, which is exactly what's safe to do in this
// narrow post-fork window; declared directly via extern "C" rather than
// pulling in the `libc` crate, since libc is already linked into any
// dynamically-linked Unix binary regardless.
// $PPID/$UID/$EUID: raw libc calls declared directly via extern "C" (same
// justification as dup2_stderr_to_stdout -- libc is already linked into
// any dynamically-linked Unix binary, no external crate needed) since std
// has no portable getppid/getuid/geteuid wrapper.
unsafe extern "C" {
    fn getppid() -> i32;
    fn getuid() -> u32;
    fn geteuid() -> u32;
    fn umask(mask: u32) -> u32;
}

// POSIX has no query-only umask read -- `umask(new) -> previous` is the
// only primitive -- so this immediately restores whatever it finds,
// leaving no observable side effect (see run_umask's own identical
// reasoning for its `umask -S`/no-args cases).
pub(crate) fn current_umask() -> u32 {
    let cur = unsafe { umask(0) };
    unsafe { umask(cur) };
    cur
}

// run_in_child_shell's own snapshot/restore for fd 0/1/2, the real
// process's own stdin/stdout/stderr -- see its own call site's doc
// comment for why a bare `exec > file` (persistent, no-fork redirect)
// needs this. `dup` (not dup2) gives each a fresh, arbitrary-numbered
// duplicate fd that keeps pointing at whatever 0/1/2 currently mean,
// regardless of what the child does to 0/1/2 themselves afterward; `-1`
// (dup failed -- an exhausted fd table, most likely) is carried through
// as "nothing to restore" rather than erroring, matching how a swallowed
// `save`/`restore` failure elsewhere in this codebase degrades rather
// than aborting the whole call.
// The lowest descriptor at or above 10 that nothing has open. bash
// allocates `{name}` redirects from 10 up, deliberately clear of the
// 0-9 a script may name directly.
fn next_free_fd() -> i32 {
    (10..256).find(|fd| !UnbufferedFd::is_open(*fd)).unwrap_or(10)
}

// The real process environment is shared by every in-process construct
// (`$( )`, `( )`, `<( )`, a redirected compound command), so a foreground
// subshell that exports a variable must not leave it behind for the
// enclosing shell -- a real fork got that isolation from the kernel.
//
// It used to be bought with a full `std::env::vars()` snapshot before and
// a full replay after *every* in-process child, which cost O(env) to take
// and O(env^2) to restore (glibc `setenv` scans `environ` linearly, called
// once per variable). On a 375-variable environment that was ~1ms per
// `$( )` -- worse than the fork it exists to avoid.
//
// An undo journal instead: while a child is running, the handful of places
// that write to the real environment record the pre-image of the name they
// are about to touch, and the restore replays that list backwards. The
// overwhelmingly common case -- a child that exports nothing -- costs one
// `Vec::new()` and one `is_empty()`.
//
// Journals nest: only the innermost frame records, which is enough,
// because an inner frame is always fully unwound before its parent's own
// unwind reads anything.
// Whether a write to the current pipeline stage's own stdout has failed
// because nothing is reading it any more, and whether anyone is asking.
//
// A pipeline stage that is its own process gets SIGPIPE here and dies,
// which is what makes `while true; do echo x; done | head -2` a script
// that terminates. A stage running *inside* this shell cannot be
// allowed to die that way -- it would take the shell with it -- and
// Rust's runtime sets SIGPIPE to SIG_IGN anyway, so the write simply
// returns `EPIPE` and the loop above it runs forever.
//
// `None` means nobody is asking: an `EPIPE` outside a pipeline stage is
// still ignored, exactly as before. Armed only around a stage running
// in this shell, where the death it stands in for is a stage's, not the
// shell's.
thread_local! {
    static BROKEN_PIPE: RefCell<Option<bool>> = const { RefCell::new(None) };
}

fn arm_broken_pipe() {
    BROKEN_PIPE.with(|b| *b.borrow_mut() = Some(false));
}

/// Stops watching, and says whether it happened.
fn disarm_broken_pipe() -> bool {
    BROKEN_PIPE.with(|b| b.borrow_mut().take()).unwrap_or(false)
}

pub(crate) fn note_broken_pipe() {
    BROKEN_PIPE.with(|b| {
        let mut b = b.borrow_mut();
        if b.is_some() {
            *b = Some(true);
        }
    });
}

/// Swaps this thread's broken-pipe watch for another, handing back what
/// was there.
///
/// The watch is per-thread, and pipeline stages share a thread -- so
/// without this, one stage's dead reader would be reported to whichever
/// stage happened to run next. The scheduler swaps each task's own in
/// around every resume, which makes the thread-local a per-coroutine
/// slot without the sink having to know that coroutines exist.
pub(crate) fn swap_broken_pipe(state: Option<bool>) -> Option<bool> {
    BROKEN_PIPE.with(|b| std::mem::replace(&mut *b.borrow_mut(), state))
}

fn broken_pipe_seen() -> bool {
    BROKEN_PIPE.with(|b| *b.borrow() == Some(true))
}

// A cheap order-sensitive fingerprint of a call's positional parameters.
// FNV-1a: no allocation, and the frames only ever compare it against
// another fingerprint taken the same way.
fn args_fingerprint(args: &[String]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for a in args {
        for b in a.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// `save_fd012`/`restore_fd012` for the scheduler, which runs pipeline
/// stages on this shell's own fd 0 and fd 1 and has to give them back.
pub(crate) fn save_fd012_for_scheduler() -> [i32; 3] {
    save_fd012()
}

pub(crate) fn restore_fd012_for_scheduler(saved: [i32; 3]) {
    restore_fd012(saved);
}

fn save_fd012() -> [i32; 3] {
    unsafe extern "C" {
        fn dup(oldfd: i32) -> i32;
    }
    [unsafe { dup(0) }, unsafe { dup(1) }, unsafe { dup(2) }]
}

fn restore_fd012(saved: [i32; 3]) {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    for (target, fd) in saved.into_iter().enumerate() {
        if fd < 0 {
            continue;
        }
        unsafe {
            dup2(fd, target as i32);
            close(fd);
        }
    }
}

// Signal traps (`trap CMD SIGNAL`). A signal handler can only safely do
// async-signal-safe work -- no allocation, no locks, nothing that could
// reenter a libc function the interrupted code was mid-call in -- so the
// handler itself does nothing but flag the signal number in this bitmask;
// the shell's own run_program loop checks it between top-level statements
// (see check_pending_signals) and runs the actual trap code there, in a
// normal, fully-safe context. One consequence: a trap won't fire truly
// asynchronously mid-syscall (e.g. while a script is blocked in `sleep` or
// `wait`) -- only at the next between-statements checkpoint. That's a
// real, deliberate scope boundary (not a bug): true async delivery mid-
// blocking-call needs EINTR-aware retry logic threaded through every
// blocking call site in this file, which is a much bigger change for a
// scripting-focused shell where "runs between statements" already covers
// the overwhelmingly common trap use (cleanup-on-signal, not low-latency
// signal response).
static PENDING_SIGNALS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

extern "C" fn record_pending_signal(sig: i32) {
    if (0..32).contains(&sig) {
        PENDING_SIGNALS.fetch_or(1 << sig, std::sync::atomic::Ordering::SeqCst);
    }
}

// Layout matches glibc's `struct sigaction` on Linux x86_64: handler
// pointer, then the 128-byte sigset_t (16 u64 words -- _NSIG/64 on this
// platform), then the int flags, then the restorer pointer (glibc's own
// sigaction() wrapper fills this in itself before the real syscall; it
// doesn't need to be set here).
#[repr(C)]
struct SigActionRaw {
    sa_handler: usize,
    sa_mask: [u64; 16],
    sa_flags: i32,
    sa_restorer: usize,
}

const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;

fn sigaction_raw(signum: i32, handler: usize) {
    unsafe extern "C" {
        fn sigaction(signum: i32, act: *const SigActionRaw, oldact: *mut SigActionRaw) -> i32;
    }
    let act = SigActionRaw { sa_handler: handler, sa_mask: [0; 16], sa_flags: 0, sa_restorer: 0 };
    unsafe {
        sigaction(signum, &act, std::ptr::null_mut());
    }
}

// SIGWINCH (terminal resize) tracking for the M9 compositor. Deliberately
// a separate flag from PENDING_SIGNALS/traps above, not a `trap`-able
// signal at all from the user's perspective: PENDING_SIGNALS is consumed
// wholesale (swapped to 0) by whichever session's run_program happens to
// call check_pending_signals next, which would silently eat a WINCH
// notification before repl.rs -- the only thing that actually owns
// terminal-size-dependent state (every session's Screen, the tab bar) --
// ever saw it. Same async-signal-safety reasoning as record_pending_signal:
// the handler only stores a bool.
pub const SIGWINCH: i32 = 28;
static WINCH_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn record_winch(_sig: i32) {
    WINCH_FLAG.store(true, std::sync::atomic::Ordering::SeqCst);
}

// Called once, at interactive startup, so the compositor loop in repl.rs
// can later poll take_winch() to notice terminal resizes.
pub fn install_winch_handler() {
    sigaction_raw(SIGWINCH, record_winch as *const () as usize);
}

// True at most once per resize -- clears the flag on read. repl.rs polls
// this once per REPL loop iteration (see the loop's own doc comment for
// why that's "per input iteration", not truly asynchronous, in M9a).
pub fn take_winch() -> bool {
    WINCH_FLAG.swap(false, std::sync::atomic::Ordering::SeqCst)
}

// Name <-> number for the signals scripts actually trap. KILL (9) and
// STOP (19) are intentionally absent -- neither can be caught or ignored,
// matching real bash's own refusal to let `trap` touch them.
pub(crate) const SIGNAL_NAMES: &[(&str, i32)] = &[
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("CHLD", 17),
    ("CONT", 18),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("URG", 23),
    ("XCPU", 24),
    ("XFSZ", 25),
    ("VTALRM", 26),
    ("PROF", 27),
    ("WINCH", 28),
    ("IO", 29),
    ("PWR", 30),
    ("SYS", 31),
];

// The two `trap` must refuse, kept out of SIGNAL_NAMES precisely so
// that list can *be* trap's answer to "may I catch this?" -- and put
// back by everything whose job is only to name a signal, which is what
// `kill -l` and a job's own status line do.
pub(crate) const UNCATCHABLE_SIGNALS: &[(&str, i32)] = &[("KILL", 9), ("STOP", 19)];

/// Every signal by name and number, in numeric order.
pub(crate) fn all_signals() -> Vec<(&'static str, i32)> {
    let mut all: Vec<(&str, i32)> = SIGNAL_NAMES.iter().chain(UNCATCHABLE_SIGNALS.iter()).copied().collect();
    all.sort_by_key(|(_, n)| *n);
    all
}

// Accepts "INT", "SIGINT", or a bare number ("2"); "0"/"EXIT" is handled by
// the caller separately since it isn't a real signal.
// Everything `${NAME}` may name: an identifier, a positional
// parameter's digits, or one of the shell's own special parameters.
fn is_parameter_name(name: &str) -> bool {
    if name.len() == 1 && "@*#?-$!_0".contains(name) {
        return true;
    }
    if name.chars().all(|c| c.is_ascii_digit()) {
        return !name.is_empty();
    }
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(crate) fn signal_number(name: &str) -> Option<i32> {
    let bare = name.strip_prefix("SIG").unwrap_or(name);
    if let Some(&(_, n)) = SIGNAL_NAMES.iter().find(|(n, _)| *n == bare) {
        return Some(n);
    }
    // A number has to be one the kernel could actually deliver: bash
    // rejects `trap x 99999` rather than recording a trap for a signal
    // that can never arrive. 64 is Linux's highest (the real-time
    // range tops out there).
    bare.parse::<i32>().ok().filter(|n| (1..=64).contains(n))
}

fn signal_name(num: i32) -> String {
    all_signals().iter().find(|(_, n)| *n == num).map(|(name, _)| name.to_string()).unwrap_or_else(|| num.to_string())
}

pub(crate) fn send_signal(pid: u32, sig: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid as i32, sig) == 0 }
}

// kill(2) with a negative pid targets the whole process group (POSIX) --
// used to SIGCONT/SIGTERM/etc. a real-job-control job (Job::pgid) as a
// unit, matching how the terminal driver itself would signal it.
pub(crate) fn send_signal_to_pgrp(pgid: u32, sig: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(-(pgid as i32), sig) == 0 }
}

// Real job control (M11): SIGCONT, used to resume a Stopped job (Job::
// stopped) from `fg`/`bg`. Linux/glibc's standard number, same "safe to
// hardcode" reasoning as this file's other signal constants.
pub(crate) const SIGCONT: i32 = 18;
// SIGSTOP: used by FgJob::send_stop instead of SIGTSTP -- see its own
// doc comment for why the catchable version doesn't reliably work here.
const SIGSTOP: i32 = 19;

unsafe extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    pub(crate) fn getpgrp() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
}

// WUNTRACED: makes waitpid additionally report a child that's stopped
// (not just exited/signaled) -- Rust's Child::wait/try_wait never pass
// this, so they can never observe a Ctrl-Z-stopped child (it just looks
// like it's still running to them, forever). Real job control needs to
// tell "stopped" apart from "still running" and "exited", hence this
// raw waitpid wrapper instead.
const WUNTRACED: i32 = 2;
// WNOHANG: don't block if the child hasn't changed state -- used by
// FgJob::poll_untraced, which (unlike waitpid_untraced) is a per-tick
// poll, not a wait.
const WNOHANG: i32 = 1;

// How a real-job-control wait (waitpid_untraced) ended.
pub(crate) enum JobWaitOutcome {
    // Exited normally or was killed by a signal -- either way, this is
    // the process's final status, bash-convention-encoded (128+signum
    // for the killed case, matching exit_code_from_status elsewhere in
    // this file).
    Exited(i32),
    // Stopped (Ctrl-Z or an explicit SIGSTOP/SIGTSTP/SIGTTIN/SIGTTOU),
    // not exited -- still alive, resumable via SIGCONT. Carries the stop
    // signal only for completeness; every caller here treats any stop
    // the same way bash's default job control does, regardless of which
    // of the four stop signals caused it.
    Stopped(i32),
}

// Linux/glibc's raw wait-status bit layout (the same "stable, standard,
// safe to hardcode" reasoning this file already applies to signal
// numbers) -- decodes what plain libc's WIFEXITED/WEXITSTATUS/
// WIFSIGNALED/WTERMSIG/WIFSTOPPED/WSTOPSIG macros would, since Rust's
// std doesn't expose a way to inspect a raw waitpid status at all (only
// ExitStatus, built from the exited/signaled cases exec_code_from_status
// already handles -- a WUNTRACED stop has no ExitStatus representation).
fn wait_status_exited(status: i32) -> bool {
    (status & 0x7f) == 0
}
fn wait_status_exit_code(status: i32) -> i32 {
    (status >> 8) & 0xff
}
fn wait_status_signaled(status: i32) -> bool {
    let low = status & 0x7f;
    low != 0 && low != 0x7f
}
fn wait_status_term_sig(status: i32) -> i32 {
    status & 0x7f
}
fn wait_status_stopped(status: i32) -> bool {
    (status & 0xff) == 0x7f
}
fn wait_status_stop_sig(status: i32) -> i32 {
    (status >> 8) & 0xff
}

// Blocking wait for a single process, but (unlike Job::wait/poll, which
// go through std::process::Child and can never ask for this) able to
// observe it stopping instead of exiting -- see WUNTRACED. Used only for
// pids real job control (Job::pgid) has isolated into their own process
// group; every other job in this shell is still waited on the ordinary
// way, via Child::wait/try_wait.
pub(crate) fn waitpid_untraced(pid: u32) -> JobWaitOutcome {
    loop {
        let mut status: i32 = 0;
        let r = unsafe { waitpid(pid as i32, &mut status, WUNTRACED) };
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // ECHILD or similar (the process is already gone some other
            // way) -- nothing meaningful to report, but this must return
            // *something* rather than loop forever.
            return JobWaitOutcome::Exited(1);
        }
        if wait_status_stopped(status) {
            return JobWaitOutcome::Stopped(wait_status_stop_sig(status));
        }
        if wait_status_signaled(status) {
            return JobWaitOutcome::Exited(128 + wait_status_term_sig(status));
        }
        debug_assert!(wait_status_exited(status));
        return JobWaitOutcome::Exited(wait_status_exit_code(status));
    }
}

// $HOSTNAME: bash populates this at startup from uname(); Linux exposes
// the same value via this proc file, which avoids yet another raw
// syscall. Falls back to the HOSTNAME env var (some environments export
// it already) and then to empty. pub: prompt.rs reuses this for the
// "user@host" segment of the default prompt rather than duplicating the
// lookup.
// A user's home directory, straight out of /etc/passwd -- field 6 of a
// colon-separated line whose field 1 is the name. No NSS, so a network
// directory is not consulted; that is the same limit every other
// /etc-parsing corner of this shell already has.
fn home_of_user(user: &str) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == user).then(|| fields.nth(4).unwrap_or_default().to_string())
    })
}

// How `set -x` shows a value: bare when it can be, single-quoted when
// it cannot. bash's trace is meant to be re-runnable, and quoting
// everything makes it noise -- `+ x=1`, not `+ x='1'`.
fn xtrace_quote(value: &str) -> String {
    let safe = value.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:=@+,%^".contains(c));
    match safe {
        true => value.to_string(),
        false => crate::serialize::quote_literal(value),
    }
}

// The same rule for a command *word*, where an empty one still has to
// be quoted or it vanishes from the trace and `echo ""` reads as
// `+ echo`. bash draws the line exactly here: `''` for an empty
// argument, a bare `x=` for an empty assignment value.
fn xtrace_quote_word(value: &str) -> String {
    match value.is_empty() {
        true => "''".to_string(),
        false => xtrace_quote(value),
    }
}

unsafe extern "C" {
    #[link_name = "getpid"]
    fn getpid_raw() -> i32;
}

// The variables lookup_var answers for without ever storing them.
// Listed here so name enumeration -- `${!prefix*}`, completion -- can
// see them too.
const COMPUTED_VAR_NAMES: &[&str] = &[
    "BASH",
    "BASHPID",
    "BASHOPTS",
    "BASH_COMMAND",
    "BASH_SUBSHELL",
    "BASH_VERSION",
    "EPOCHREALTIME",
    "EPOCHSECONDS",
    "EUID",
    "HOSTNAME",
    "PPID",
    "SECONDS",
    "SHELLOPTS",
    "UID",
];

pub fn get_hostname() -> String {
    if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        return s.trim_end().to_string();
    }
    std::env::var("HOSTNAME").unwrap_or_default()
}

// `${v@P}`'s own `\u`/`\$` helpers -- deliberately separate from
// prompt.rs's own username()/geteuid() (same underlying logic, but
// this transform stays fully self-contained rather than depending on
// the actual-prompt module, which stays untouched).
fn prompt_username() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "user".to_string())
}

fn is_effective_root() -> bool {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() == 0 }
}

// `\l`: basename of the controlling terminal's device name (bash reads
// this from the tty itself). Best-effort -- an unresolvable tty (not a
// real terminal, ttyname_r failing, ...) just gives an empty string,
// same spirit as bash showing nothing useful there either in that case.
fn tty_basename() -> String {
    unsafe extern "C" {
        fn ttyname_r(fd: i32, buf: *mut u8, buflen: usize) -> i32;
    }
    let mut buf = [0u8; 256];
    let ok = unsafe { ttyname_r(0, buf.as_mut_ptr(), buf.len()) == 0 };
    if !ok {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..end]).unwrap_or("").rsplit('/').next().unwrap_or("").to_string()
}

// `read -p`'s prompt only displays when input is coming from a terminal
// (bash-documented behavior), and `read -t`'s timeout is only meaningfully
// pollable against a real fd (stdin), not a shell-internal here-doc/file
// Cursor -- both need to know if fd 0 is an actual tty/pollable descriptor.
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

fn stdin_is_tty() -> bool {
    unsafe extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(0) != 0 }
}

// Polls fd 0 for readability, waiting up to `timeout_ms` (0 = check without
// waiting). Used by `read -t` to implement its timeout without needing a
// second thread or signal handling.
fn stdin_ready(timeout_ms: i32) -> bool {
    const POLLIN: i16 = 0x0001;
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    }
    let mut pfd = PollFd { fd: 0, events: POLLIN, revents: 0 };
    let r = unsafe { poll(&mut pfd as *mut PollFd, 1, timeout_ms) };
    r > 0 && (pfd.revents & POLLIN) != 0
}

// Single pre_exec hook covering both dup_stderr_to_stdout and any
// numbered-fd redirects -- CommandExt::pre_exec isn't documented as safe to
// call more than once per Command, so every fd-dup need for a given spawn
// funnels through this one closure instead of stacking separate calls.
// Real bug, found by tracing a "3>file works for every fd except 3" report
// down to its root cause: `dup2(oldfd, newfd)` is a no-op per POSIX when
// oldfd == newfd -- it does *not* clear FD_CLOEXEC on that descriptor. Rust
// opens files (and pipes, e.g. for coproc) with O_CLOEXEC by default, so
// whenever the file/pipe's own already-assigned fd number happens to
// coincide with the redirect's target fd (a real possibility: both are
// small numbers chosen by "next available", so collisions aren't rare),
// dup2 silently no-ops and the fd -- despite looking open right up to the
// exec() call -- gets closed by the kernel as part of exec's own cloexec
// handling, instead of surviving into the child like every other target
// fd does. Explicitly clearing FD_CLOEXEC after every dup2 (redundant for
// the normal src != dst case, since dup2 already clears it there) makes
// this correct regardless of whether the dup2 was a genuine duplicate or
// this same-fd no-op.
// Applies numbered-fd redirects (dup2/close) directly to *this* process,
// for `exec`'s redirect-only form (`exec 3>file`, `exec 3>&1`, `exec
// 3>&-`) -- unlike every other spawn site in this file, there's no pre_exec
// hook here since nothing is being forked; this just runs the same dup2/
// close logic inline, before continuing (bare exec) or before calling
// CommandExt::exec (exec CMD). Scoped to the numbered-fd forms specifically
// (extra_fds/dup_stderr_to_stdout) -- plain `exec > file` (implicit fd
// 0/1/2) goes through a completely different Option<Stdio>-based resolution
// path elsewhere in this file that isn't wired up to apply to the current
// process, a separate, narrower remaining gap.
fn apply_fds_to_self(actions: Vec<FdAction>) -> Result<(), String> {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    for ef in actions {
        match ef {
            FdAction::Open { fd, file } => {
                let srcfd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
                if unsafe { dup2(srcfd, fd) } == -1 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                clear_cloexec(fd);
                // Unlike apply_fd_redirects' pre_exec path (where the
                // process image gets replaced by execve right after,
                // bypassing Rust-level Drop entirely, .exec() failure
                // notwithstanding -- and even then via _exit, not normal
                // unwinding), this function returns normally and lets its
                // locals drop like any other Rust code. If `file`'s own fd
                // happened to already equal the target (a real
                // possibility, same as the pre_exec case), dropping it
                // here would close the very fd this function exists to
                // keep open -- forget it instead so it isn't closed out
                // from under the persisted redirect. When source != target
                // there's no such risk, and letting `file` drop normally
                // correctly closes the now-redundant source descriptor.
                if srcfd == fd {
                    std::mem::forget(file);
                }
            }
            FdAction::Dup { fd, source } => {
                if unsafe { dup2(source, fd) } == -1 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                clear_cloexec(fd);
            }
            // Closing a descriptor nothing opened is a no-op, not an
            // error: `{ echo a; } 3>&-` is legal in bash whether or not
            // anything ever opened fd 3, and a script that closes
            // defensively should not have to check first.
            FdAction::Close(fd) => unsafe {
                close(fd);
            },
        }
    }
    Ok(())
}

fn clear_cloexec(fd: i32) {
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    }
    const F_SETFD: i32 = 2;
    unsafe {
        fcntl(fd, F_SETFD, 0);
    }
}

fn apply_fd_redirects(command: &mut Command, actions: Vec<FdAction>) {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    unsafe {
        command.pre_exec(move || {
            // bish ignores SIGINT for itself (term::ignore_sigint, called
            // once at interactive startup) so Ctrl-C from its own
            // controlling terminal doesn't kill the shell -- but that
            // disposition is inherited across fork, and POSIX only resets
            // *handled* signals back to SIG_DFL across exec; an ignored
            // (SIG_IGN) disposition is explicitly left unchanged. Without
            // this reset, every external child would silently inherit
            // "ignore SIGINT" too and never respond to Ctrl-C.
            sigaction_raw(2, SIG_DFL);
            for ef in &actions {
                match ef {
                    FdAction::Open { fd, file } => {
                        if dup2(std::os::unix::io::AsRawFd::as_raw_fd(file), *fd) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        clear_cloexec(*fd);
                    }
                    FdAction::Dup { fd, source } => {
                        if dup2(*source, *fd) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        clear_cloexec(*fd);
                    }
                    // See the same arm in apply_fds_to_self: closing a
                    // descriptor nothing opened is a no-op.
                    FdAction::Close(fd) => {
                        close(*fd);
                    }
                }
            }
            Ok(())
        });
    }
}

// Here-strings need a real Stdio for the child process; simplest portable
// way to hand it literal content is a temp file, unlinked immediately after
// opening (the open fd keeps the data alive on unix even once unlinked).

// A capture buffer for `$( )` that never touches a filesystem.
//
// The child of a command substitution runs *in this process*, but it may
// still spawn a real external command, which needs a real file descriptor
// to write into -- so an in-memory `OutputSink` alone is not enough, and a
// pipe would deadlock the moment the output outgrew the pipe buffer with
// nobody on the read end. `memfd_create(2)` gives both: a real fd, backed
// by anonymous memory, with no name, no directory entry, and no unlink.
//
// That replaces, per substitution, an open(O_CREAT) in $TMPDIR, the
// directory-entry write it implies, and an unlink -- measured at ~23us of
// a ~63us `x=$(printf hi)` on tmpfs, and considerably worse when $TMPDIR
// is a real disk. Falls back to the old temp file where the syscall is
// unavailable (pre-3.17 kernels, non-Linux).
fn capture_file() -> Option<std::fs::File> {
    unsafe extern "C" {
        fn memfd_create(name: *const u8, flags: u32) -> i32;
    }
    // MFD_CLOEXEC: this fd is dup2'd onto the child's stdout explicitly
    // where it is wanted, and must not leak into anything else spawned.
    const MFD_CLOEXEC: u32 = 1;
    let fd = unsafe { memfd_create(c"bish-capture".as_ptr() as *const u8, MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    use std::os::fd::FromRawFd;
    Some(unsafe { std::fs::File::from_raw_fd(fd) })
}

// Reads back everything written to a `capture_file`, from the start.
fn read_capture(mut f: std::fs::File) -> String {
    use std::io::{Read, Seek, SeekFrom};
    if f.seek(SeekFrom::Start(0)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn proc_sub_temp_path() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("bish-procsub-{}-{}", std::process::id(), n))
}

fn here_string_file(content: &str) -> Result<std::fs::File, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("bish-herestring-{}-{}", std::process::id(), n));
    std::fs::write(&path, content).map_err(|e| format!("here-string: {}", e))?;
    let f = std::fs::File::open(&path).map_err(|e| format!("here-string: {}", e))?;
    let _ = std::fs::remove_file(&path);
    Ok(f)
}

// `/dev/tcp/HOST/PORT` and `/dev/udp/HOST/PORT`: real bash's own special
// pseudo-device redirect targets (`exec 3<>/dev/tcp/host/80`, most
// commonly) -- recognized here rather than treated as real filesystem
// paths, matching bash itself (these usually don't exist as real files
// at all, and even on a system where /dev/tcp really is populated by a
// kernel module, depending on that would be OS-specific in a way this
// project doesn't want). `None` for anything that isn't exactly this
// shape, so every caller (open_in/open_out) falls through to an
// ordinary file open unchanged. No new dependency needed: on Unix, a
// connected TcpStream/UdpSocket and a plain File are both just thin
// wrappers around one real OS file descriptor supporting the same
// read()/write() -- converting one into the other via its raw fd
// (IntoRawFd/FromRawFd) is exactly what real bash's own C
// implementation does under the hood too (a real socket fd, dup2'd like
// any other).
// `exec [-cl] [-a name] [command [argument ...]] [redirection ...]`.
//
// Three outcomes, because bash's own `exec` has three: run a command
// (with argv[0] and the environment possibly rewritten), fail on a bad
// option, or -- when no command word survives the options -- do nothing
// but apply the redirects to this shell. That last case includes
// `exec -a foo` with no command, which real bash accepts silently and
// treats exactly like a bare `exec`.
struct ExecFlags {
    // `-a NAME`: argv[0] the new process sees, instead of the path.
    arg0: Option<String>,
    // `-c`: start it with an empty environment.
    clear_env: bool,
    // `-l`: prefix argv[0] with a dash, the convention a login shell
    // looks for.
    login: bool,
    // Index in argv of the command word.
    first: usize,
}

enum ExecOpts {
    Run(ExecFlags),
    BadOption(String),
    RedirectsOnly,
}

fn parse_exec_opts(argv: &[String]) -> ExecOpts {
    let mut flags = ExecFlags { arg0: None, clear_env: false, login: false, first: 0 };
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--" {
            i += 1;
            break;
        }
        let Some(letters) = a.strip_prefix('-').filter(|r| !r.is_empty()) else { break };
        let mut chars = letters.chars();
        while let Some(c) = chars.next() {
            match c {
                'c' => flags.clear_env = true,
                'l' => flags.login = true,
                // Like getopt's `a:`, the value may be glued on
                // (`-afoo`) or the next word (`-a foo`).
                'a' => {
                    let glued: String = chars.by_ref().collect();
                    if !glued.is_empty() {
                        flags.arg0 = Some(glued);
                    } else {
                        i += 1;
                        match argv.get(i) {
                            Some(name) => flags.arg0 = Some(name.clone()),
                            None => return ExecOpts::BadOption("-a: option requires an argument".to_string()),
                        }
                    }
                }
                other => return ExecOpts::BadOption(format!("-{}: invalid option", other)),
            }
        }
        i += 1;
    }
    if i >= argv.len() {
        return ExecOpts::RedirectsOnly;
    }
    flags.first = i;
    ExecOpts::Run(flags)
}

// A builtin handed an option it does not have. bash's shape exactly:
// the complaint, then that builtin's own usage line -- which is the
// half that answers the question -- and status 2. Silently ignoring an
// unknown option, which is what this shell used to do everywhere, turns
// a typo into a behaviour change nobody sees.
// The first `-X` in `args` whose letters aren't all in `accepted`,
// spelled the way bash reports it (`-z`, one letter, even when it came
// from a cluster). `--` ends the options, and a lone `-` is an operand
// rather than an option, both as everywhere else.
pub(crate) fn first_unknown_option(args: &[String], accepted: &str) -> Option<String> {
    for a in args {
        if a == "--" {
            return None;
        }
        let Some(letters) = a.strip_prefix('-').filter(|r| !r.is_empty()) else {
            return None;
        };
        if let Some(c) = letters.chars().find(|c| !accepted.contains(*c)) {
            return Some(format!("-{}", c));
        }
    }
    None
}

pub(crate) fn bad_option_status(sh: &mut Shell, who: &str, opt: &str, usage: &str) -> i32 {
    sh_eprintln!(sh, "bish: {}: {}: invalid option", who, opt);
    sh_eprintln!(sh, "{}: usage: {}", who, usage);
    2
}

// Rust's `io::Error` Display appends " (os error N)". No shell prints
// that, and a script that greps a message for "No such file or
// directory" should not have to know about it -- this is the same text
// without the tail, which is what strerror(3), and so bash, gives.
pub(crate) fn os_message(e: &std::io::Error) -> String {
    let text = e.to_string();
    match text.rfind(" (os error ") {
        Some(i) if text.ends_with(')') => text[..i].to_string(),
        _ => text,
    }
}

fn dev_socket_file(path: &str) -> Option<Result<std::fs::File, String>> {
    let (proto, rest) = path.strip_prefix("/dev/tcp/").map(|r| ("tcp", r)).or_else(|| path.strip_prefix("/dev/udp/").map(|r| ("udp", r)))?;
    let (host, port) = rest.split_once('/')?;
    if host.is_empty() || port.is_empty() {
        return None;
    }
    Some(connect_dev_socket(proto, host, port).map_err(|e| format!("{}: {}", path, os_message(&e))))
}

// A builtin's own output redirected onto an arbitrary already-open fd
// (`>&3`/`2>&4`, most commonly a fd `exec N<>/dev/tcp/...` left open --
// see push_builtin_output_sink's own `Redirect::FdDup` handling): `fd`
// itself is owned/managed elsewhere (opened by `exec`'s own persistent
// redirect, eventually closed by `exec N<&-`/FdClose), so this must not
// end up closing it once this one builtin call's own sink is popped.
// `dup()` (unlike `dup2`) always returns a brand-new fd number pointing
// at the same open file description, safe to wrap in an ordinary File
// (and let its own Drop close, same as any other Rc<RefCell<File>> this
// sink already uses) without touching the original at all -- same "dup
// aside, use the dup, close the dup" idiom save_fd012/restore_fd012
// already use for a similar reason. `None` on a bad/closed fd (e.g. a
// typo'd fd number) rather than erroring -- matches this codebase's
// existing tolerance for a swallowed save/restore failure elsewhere.
fn dup_existing_fd(fd: i32) -> Option<std::fs::File> {
    unsafe extern "C" {
        fn dup(oldfd: i32) -> i32;
    }
    let new_fd = unsafe { dup(fd) };
    if new_fd < 0 {
        return None;
    }
    use std::os::unix::io::FromRawFd;
    Some(unsafe { std::fs::File::from_raw_fd(new_fd) })
}

fn connect_dev_socket(proto: &str, host: &str, port: &str) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    let addr = format!("{host}:{port}");
    if proto == "tcp" {
        let stream = std::net::TcpStream::connect(&addr)?;
        return Ok(unsafe { std::fs::File::from_raw_fd(stream.into_raw_fd()) });
    }
    // UDP's own connect() just records the peer address for subsequent
    // send()/recv() (no handshake, unlike TCP) -- never blocks on an
    // unreachable host; a real send is what would fail if nothing's
    // listening. Bind a wildcard on whichever address family the target
    // actually resolved to, not a hardcoded IPv4-only "0.0.0.0" -- an
    // IPv6-only target would otherwise fail to connect for a reason
    // that has nothing to do with the target itself.
    use std::net::ToSocketAddrs;
    let target = addr.to_socket_addrs()?.next().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "could not resolve host"))?;
    let bind_addr = if target.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
    let socket = std::net::UdpSocket::bind(bind_addr)?;
    socket.connect(target)?;
    Ok(unsafe { std::fs::File::from_raw_fd(socket.into_raw_fd()) })
}

// Appends an expansion's value `v` to the in-progress word-split state.
// Quoted values are never split (appended verbatim, like literal text).
// Unquoted values are split on whitespace: interior separators end the
// current field and start a new one; leading/trailing whitespace in `v`
// forces a boundary even when the adjacent side has zero-or-one resulting
// parts (e.g. `pre$x` where x=" y " must split into "pre" and "y", not
// "prey"). A purely empty-or-whitespace unquoted value contributes nothing
// (matching bash: an unquoted unset/empty variable standing alone
// contributes zero arguments, not one empty one).
// Real bash IFS semantics: a run of IFS-whitespace characters (IFS chars
// that are also whitespace) collapses into a single delimiter and is
// trimmed from the very start/end of the value; each occurrence of an
// IFS *non*-whitespace character (e.g. ':') is its own separate delimiter,
// significant even with nothing but another separator on either side (so
// "a::b" with IFS=":" is three fields: "a", "", "b"). A whitespace run
// immediately touching a non-whitespace separator on either side is
// absorbed into that same delimiter ("a : b" with IFS=" :" is "a","b" --
// not "a","","b"). Returns (parts, leading_sep, trailing_sep): the two
// bools tell the caller whether the first/last part is a complete,
// standalone field or should stitch onto whatever's pending from an
// adjacent chunk (see append_splittable).
fn ifs_tokenize(v: &str, ifs: &str) -> (Vec<String>, bool, bool) {
    let is_ws = |c: char| c.is_whitespace() && ifs.contains(c);
    let is_sep = |c: char| ifs.contains(c);
    let chars: Vec<char> = v.chars().collect();
    let n = chars.len();

    let mut i = 0;
    while i < n && is_ws(chars[i]) {
        i += 1;
    }
    let leading_sep = i > 0;
    if i >= n {
        return (Vec::new(), leading_sep, false);
    }

    let mut parts = Vec::new();
    let mut trailing_sep = false;
    loop {
        let start = i;
        while i < n && !is_sep(chars[i]) {
            i += 1;
        }
        parts.push(chars[start..i].iter().collect::<String>());
        if i >= n {
            break;
        }
        while i < n && is_ws(chars[i]) {
            i += 1;
        }
        if i < n && is_sep(chars[i]) {
            i += 1;
            while i < n && is_ws(chars[i]) {
                i += 1;
            }
        }
        if i >= n {
            // A delimiter at the very end of the string is dropped
            // without producing a trailing empty field -- unlike an
            // *embedded* separator (e.g. "a::b" is still "a","","b"),
            // confirmed against real bash: "a:" with IFS=":" splits to
            // exactly one field, "a", not ["a", ""].
            trailing_sep = true;
            break;
        }
    }
    (parts, leading_sep, trailing_sep)
}

// Finds the next IFS-delimited field in `s` (which is assumed to already
// have any leading separator trimmed), returning (field, remainder-with-
// its-own-leading-separator-trimmed). None if `s` contains no further
// delimiter at all. Used by `read NAME1 NAME2 ...`, which needs the
// untouched remainder text for its last variable rather than a fully
// re-tokenized-and-rejoined value.
// ifs_next_field over a line `read` has already unescaped, starting at
// `from`. A character `escaped` marks is not a separator, whatever it
// is. Returns the field and where the remainder starts.
fn ifs_next_field_masked(chars: &[char], escaped: &[bool], ifs: &str, from: usize) -> Option<(String, usize)> {
    let is_sep = |i: usize| !escaped[i] && ifs.contains(chars[i]);
    let is_ws = |i: usize| !escaped[i] && chars[i].is_whitespace() && ifs.contains(chars[i]);
    let n = chars.len();
    let mut i = from;
    while i < n && !is_sep(i) {
        i += 1;
    }
    if i >= n {
        return None;
    }
    let field: String = chars[from..i].iter().collect();
    while i < n && is_ws(i) {
        i += 1;
    }
    if i < n && is_sep(i) {
        i += 1;
        while i < n && is_ws(i) {
            i += 1;
        }
    }
    Some((field, i))
}

// The whole line at once, for `read -a`.
fn ifs_tokenize_masked(chars: &[char], escaped: &[bool], ifs: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut i = 0;
    while i < chars.len() && !escaped[i] && chars[i].is_whitespace() && ifs.contains(chars[i]) {
        i += 1;
    }
    while i < chars.len() {
        match ifs_next_field_masked(chars, escaped, ifs, i) {
            Some((field, next)) => {
                fields.push(field);
                i = next;
            }
            None => {
                fields.push(chars[i..].iter().collect());
                break;
            }
        }
    }
    fields
}

fn append_splittable(fields: &mut Vec<String>, current: &mut Option<String>, v: &str, quoted: bool, ifs: &str) {
    if quoted {
        current.get_or_insert_with(String::new).push_str(v);
        return;
    }
    if v.is_empty() {
        return;
    }
    if ifs.is_empty() {
        // IFS set to the empty string: no splitting at all.
        current.get_or_insert_with(String::new).push_str(v);
        return;
    }
    let (parts, leading_sep, trailing_sep) = ifs_tokenize(v, ifs);
    if parts.is_empty() {
        if let Some(c) = current.take() {
            fields.push(c);
        }
        return;
    }
    if leading_sep {
        if let Some(c) = current.take() {
            fields.push(c);
        }
    }
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            fields.push(current.take().unwrap_or_default());
        }
        current.get_or_insert_with(String::new).push_str(part);
    }
    if trailing_sep {
        fields.push(current.take().unwrap_or_default());
    }
}

// Like append_splittable, but for "$@": the parts are already well-defined
// (one per positional parameter, never re-split even if a param contains
// whitespace) rather than derived by splitting a joined string.
fn append_parts(fields: &mut Vec<String>, current: &mut Option<String>, parts: &[String]) {
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            fields.push(current.take().unwrap_or_default());
        }
        current.get_or_insert_with(String::new).push_str(part);
    }
}

// Pairs append_splittable's field-boundary logic with a second, escaped
// copy of the same value for glob-pattern purposes (see expand_word_split).
// glob::escape only ever inserts backslashes before `*?[\@!+(`, none of
// which are whitespace, so splitting the escaped copy on the same IFS
// lands on the same boundaries as splitting `v` itself -- except in the
// pathological case of an IFS that itself contains one of those
// characters, an accepted, exceedingly rare edge case.
fn append_splittable_glob(
    fields: &mut Vec<String>,
    current: &mut Option<String>,
    patterns: &mut Vec<String>,
    pattern_current: &mut Option<String>,
    v: &str,
    quoted: bool,
    ifs: &str,
) {
    append_splittable(fields, current, v, quoted, ifs);
    let p = if quoted { crate::glob::escape(v) } else { v.to_string() };
    append_splittable(patterns, pattern_current, &p, quoted, ifs);
}

// append_parts' counterpart to append_splittable_glob. "$@"/array-keys
// parts always arrive already-quoted (that's why append_parts exists, as
// opposed to append_splittable), so every part is escaped for the pattern
// copy -- these fields are never glob-eligible.
fn append_parts_glob(
    fields: &mut Vec<String>,
    current: &mut Option<String>,
    patterns: &mut Vec<String>,
    pattern_current: &mut Option<String>,
    parts: &[String],
) {
    append_parts(fields, current, parts);
    let escaped: Vec<String> = parts.iter().map(|p| crate::glob::escape(p)).collect();
    append_parts(patterns, pattern_current, &escaped);
}

fn strip_prefix_glob(s: &str, pattern: &str, longest: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let lens: Vec<usize> = if longest { (0..=chars.len()).rev().collect() } else { (0..=chars.len()).collect() };
    for len in lens {
        let candidate: String = chars[..len].iter().collect();
        if glob::matches(pattern, &candidate) {
            return chars[len..].iter().collect();
        }
    }
    s.to_string()
}

fn strip_suffix_glob(s: &str, pattern: &str, longest: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let lens: Vec<usize> = if longest { (0..=n).rev().collect() } else { (0..=n).collect() };
    for len in lens {
        let candidate: String = chars[n - len..].iter().collect();
        if glob::matches(pattern, &candidate) {
            return chars[..n - len].iter().collect();
        }
    }
    s.to_string()
}

// `${V^pattern}` family. An empty pattern (the common `${V^^}` shape with
// nothing after it) matches every character; otherwise each candidate
// character is matched against the pattern with the same glob matcher
// `case` patterns use. `all` picks every matching char vs just the first.
fn apply_case_convert(cur: &str, pattern: &str, upper: bool, all: bool) -> String {
    let mut result = String::with_capacity(cur.len());
    let mut first = true;
    for ch in cur.chars() {
        let convert = (all || first) && (pattern.is_empty() || glob::matches(pattern, &ch.to_string()));
        if convert {
            if upper {
                result.extend(ch.to_uppercase());
            } else {
                result.extend(ch.to_lowercase());
            }
        } else {
            result.push(ch);
        }
        first = false;
    }
    result
}

// `${V:offset}` / `${V:offset:length}`. Character-indexed (not byte), same
// as everywhere else length/indexing happens in this shell. Negative
// offset counts back from the end; negative length is an end position
// counted from the end too (bash: "an offset from the end of the string"),
// not an error.
fn substring_expand(s: &str, offset: i64, length: Option<i64>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    let start = if offset < 0 { (n + offset).max(0) } else { offset.min(n) };
    let end = match length {
        None => n,
        Some(l) if l < 0 => (n + l).max(start),
        Some(l) => (start + l).min(n),
    };
    if end <= start {
        return String::new();
    }
    chars[start as usize..end as usize].iter().collect()
}

// Finds the leftmost-longest glob match of `pattern` in `chars` at or
// after `start_from`, honoring `anchor` (Start/End restrict the search to
// that specific boundary, matching how `#`/`%` pattern-stripping anchors
// its own search). Returns the matched (start, end) character range.
fn find_glob_match(chars: &[char], start_from: usize, pattern: &str, anchor: ReplaceAnchor) -> Option<(usize, usize)> {
    let n = chars.len();
    match anchor {
        ReplaceAnchor::Start => {
            if start_from > 0 {
                return None;
            }
            (start_from..=n).rev().find_map(|end| {
                let candidate: String = chars[start_from..end].iter().collect();
                glob::matches(pattern, &candidate).then_some((start_from, end))
            })
        }
        ReplaceAnchor::End => (start_from..=n).find_map(|s| {
            let candidate: String = chars[s..n].iter().collect();
            glob::matches(pattern, &candidate).then_some((s, n))
        }),
        ReplaceAnchor::None => {
            for s in start_from..=n {
                if let Some((s0, e0)) = (s..=n).rev().find_map(|end| {
                    let candidate: String = chars[s..end].iter().collect();
                    glob::matches(pattern, &candidate).then_some((s, end))
                }) {
                    return Some((s0, e0));
                }
            }
            None
        }
    }
}

// `${V/pat/repl}` (first match), `${V//pat/repl}` (all matches),
// `${V/#pat/repl}` / `${V/%pat/repl}` (anchored -- always a single check,
// `global` doesn't apply to them since an anchored match can only occur
// once).
fn glob_replace(s: &str, pattern: &str, repl: &str, global: bool, anchor: ReplaceAnchor) -> String {
    let chars: Vec<char> = s.chars().collect();
    if matches!(anchor, ReplaceAnchor::Start | ReplaceAnchor::End) {
        return match find_glob_match(&chars, 0, pattern, anchor) {
            Some((s0, e0)) => {
                let mut out: String = chars[..s0].iter().collect();
                out.push_str(repl);
                out.extend(&chars[e0..]);
                out
            }
            None => s.to_string(),
        };
    }
    let mut out = String::new();
    let mut pos = 0;
    loop {
        match find_glob_match(&chars, pos, pattern, ReplaceAnchor::None) {
            Some((s0, e0)) => {
                out.extend(&chars[pos..s0]);
                out.push_str(repl);
                // An empty match (a pattern like a bare "*" can't produce
                // one here since matching is greedy-longest, but stay
                // defensive) must still advance, or this loops forever.
                pos = if e0 > s0 { e0 } else { e0 + 1 };
                if !global || pos > chars.len() {
                    if pos <= chars.len() {
                        out.extend(&chars[pos..]);
                    }
                    break;
                }
            }
            None => {
                out.extend(&chars[pos..]);
                break;
            }
        }
    }
    out
}

// `declare -p`'s own value quoting: double-quoted with backslash-escaped
// '"'/'\'/'$'/'`' (matching real bash's own `declare -p` output), not
// serialize::quote_literal's single-quote style -- the two aren't
// interchangeable, this one specifically matches what `declare -p`
// itself prints.
fn declare_p_quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

// Only the four pure string-transform operators -- @A/@a/@K/@P all need
// context (name/attributes, or live shell state) this function doesn't
// have, so every call site handles those itself (see eval_var_op/
// eval_array_var_op's own Transform arms) and never reaches here with
// them.
fn apply_transform(cur: &str, kind: TransformKind) -> String {
    match kind {
        TransformKind::Quote => crate::serialize::quote_literal(cur),
        TransformKind::Upper => cur.to_uppercase(),
        TransformKind::UpperFirst => {
            let mut chars = cur.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
        TransformKind::Lower => cur.to_lowercase(),
        TransformKind::Escape => expand_backslash_escapes(cur),
        TransformKind::Attributes | TransformKind::AttributeFlags | TransformKind::KeyValue | TransformKind::Prompt => {
            unreachable!("handled directly in eval_var_op/eval_array_var_op")
        }
    }
}

// Same escape table as the lexer's own $'...' reader (read_ansi_c_string)
// -- an unrecognized backslash sequence passes through literally rather
// than dropping the backslash, matching bash's own $'...' behavior.
fn expand_backslash_escapes(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            Some('a') => out.push('\x07'),
            Some('b') => out.push('\x08'),
            Some('e') => out.push('\x1b'),
            Some('f') => out.push('\x0c'),
            Some('v') => out.push('\x0b'),
            Some('0') => out.push('\0'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

// One line per builtin, for `help`. Kept beside KNOWN_BUILTINS because
// the two have to agree: a test asserts every name in that list has a
// line here and vice versa, so adding a builtin without describing it
// fails the build rather than silently listing a blank.
//
// Deliberately one line each, not bash's paragraphs. bash's `help` is a
// manual; this is an index -- enough to tell you which builtin you
// want, after which `::bish <thing> help` or the real manual has the
// detail. A line that has to wrap is a line nobody reads in a list of
// sixty.
pub(crate) const BUILTIN_HELP: &[(&str, &str)] = &[
    (":", "Do nothing, successfully. Arguments are still expanded."),
    ("true", "Do nothing, successfully."),
    ("false", "Do nothing, unsuccessfully."),
    ("suspend", "Suspend this shell. Needs job control."),
    (".", "Read and run a file in this shell. Same as `source`."),
    ("::bish", "bish's own namespace: theme, window, hook, hl, lsp, map."),
    ("=", "Evaluate an arithmetic expression and print it."),
    ("[", "Test a condition. Same as `test`, but wants a closing `]`."),
    ("[[", "Test a condition, with pattern and regex matching."),
    ("abbr", "Abbreviations that expand as you type."),
    ("alias", "Define or list command aliases."),
    ("bg", "Resume a stopped job in the background."),
    ("bishopt", "Get or set bish's own options."),
    ("break", "Leave a for, while or until loop."),
    ("builtin", "Run a builtin, ignoring any function or `enable -n`."),
    ("caller", "Show where the current function was called from."),
    ("cd", "Change the working directory."),
    ("pwd", "Print the working directory."),
    ("command", "Run a command, ignoring any function of the same name."),
    ("compgen", "Generate the completions a `complete` spec would offer."),
    ("complete", "Say how a command's arguments should be completed."),
    ("compopt", "Change the options of a completion in progress."),
    ("continue", "Start the next turn of a loop."),
    ("declare", "Declare variables and give them attributes."),
    ("dirs", "Show the directory stack."),
    ("disown", "Stop tracking a job, so it outlives this shell."),
    ("e", "Open a file in bish's own editor."),
    ("echo", "Print arguments, separated by spaces."),
    ("enable", "Enable or disable builtins."),
    ("eval", "Join the arguments into one command and run it."),
    ("exec", "Replace this shell with a command, or apply redirects to it."),
    ("exit", "Leave the shell."),
    ("export", "Mark variables so commands started here inherit them."),
    ("fc", "List history entries (`fc -l`)."),
    ("fg", "Bring a job to the foreground."),
    ("getopts", "Parse option arguments in a script."),
    ("hash", "Show or clear the remembered paths of commands."),
    ("help", "Show this index of builtins."),
    ("history", "Show the command history."),
    ("jobs", "List this shell's jobs."),
    ("json", "Parse, pretty-print or query JSON."),
    ("kill", "Send a signal to a job or process."),
    ("let", "Evaluate arithmetic expressions."),
    ("local", "Declare variables local to the current function."),
    ("mapfile", "Read lines of input into an array."),
    ("popd", "Pop a directory off the stack and go there."),
    ("printf", "Print according to a format."),
    ("pushd", "Push a directory onto the stack and go there."),
    ("read", "Read one line into variables."),
    ("readarray", "Read lines of input into an array. Same as `mapfile`."),
    ("readonly", "Mark variables so they cannot be changed."),
    ("return", "Leave a function or a sourced file."),
    ("set", "Set shell options and the positional parameters."),
    ("shift", "Drop positional parameters from the front."),
    ("shopt", "Get or set optional shell behaviour."),
    ("source", "Read and run a file in this shell. Same as `.`."),
    ("test", "Test a condition."),
    ("times", "Show CPU time used by this shell and its children."),
    ("trap", "Run a command when a signal or shell event arrives."),
    ("type", "Say what kind of thing a name is."),
    ("typeset", "Declare variables and give them attributes. Same as `declare`."),
    ("ulimit", "Get or set resource limits."),
    ("umask", "Get or set the file-creation mask."),
    ("unalias", "Remove aliases."),
    ("unset", "Remove variables or functions."),
    ("wait", "Wait for jobs to finish."),
    ("win", "Manage windows and panes. Same as `window`."),
    ("window", "Manage windows and panes."),
];

// Kept in sync with run_single's builtin dispatch match by hand -- used
// only by `command -v`/`type` to classify a name, not to actually run
// anything, so a name missing from this list just means those two
// diagnostic builtins under-report it as an external/not-found rather
// than anything actually breaking.
pub(crate) const KNOWN_BUILTINS: &[&str] = &[
    ":",
    "true",
    "false",
    "suspend",
    "times",
    "caller",
    "enable",
    "help",
    // Every name `dispatch_builtin_or_external_impl` handles has to be
    // here, not just for completion: `run_multi` uses this list to
    // decide whether a pipeline stage needs the self-exec that lets a
    // builtin run as one. A builtin missing from it is spawned as an
    // external program and fails with ENOENT -- which is exactly what
    // `echo hi | json .` did until `json` was added. See
    // `every_dispatched_builtin_is_known`.
    "json",
    "cd",
    "pwd",
    "e",
    "export",
    "let",
    "break",
    "continue",
    "test",
    "[",
    "[[",
    "return",
    "shift",
    "local",
    "exit",
    "read",
    "mapfile",
    "readarray",
    "eval",
    "source",
    ".",
    "trap",
    "jobs",
    "disown",
    "fg",
    "bg",
    "wait",
    "kill",
    "getopts",
    "unset",
    "set",
    "declare",
    "typeset",
    "readonly",
    "exec",
    "command",
    "builtin",
    "type",
    "hash",
    "shopt",
    "umask",
    "pushd",
    "popd",
    "dirs",
    "ulimit",
    "alias",
    "unalias",
    "abbr",
    "=",
    "history",
    "fc",
    "bishopt",
    "::bish",
    "compgen",
    "complete",
    "compopt",
    "window",
    "win",
    "echo",
    "printf",
];

// The names `shopt` itself recognizes, each with its own default on/off
// state absent an explicit `-s`/`-u` override -- mirrors bash 5.x's own
// `shopt` output for an ordinary interactive shell (`bash -c shopt`).
// Needed for run_shopt to enumerate anything at all (bare `shopt`/`shopt
// -s`/`shopt -u` have nothing to list without a registry), and to reject
// a genuinely unknown name the way real bash does, rather than silently
// treating every unrecognized string as "off". As with shopt_options
// itself, most of these have no actual effect on bish's behavior beyond
// being trackable -- extglob is the one name overridden elsewhere (see
// Shell::shopt_is_on) since bish's extglob support is unconditional.
pub(crate) const KNOWN_SHOPT_OPTIONS: &[(&str, bool)] = &[
    ("array_expand_once", false),
    ("assoc_expand_once", false),
    ("autocd", false),
    ("bash_source_fullpath", false),
    ("cdable_vars", false),
    ("cdspell", false),
    ("checkhash", false),
    ("checkjobs", false),
    ("checkwinsize", true),
    ("cmdhist", true),
    ("compat31", false),
    ("compat32", false),
    ("compat40", false),
    ("compat41", false),
    ("compat42", false),
    ("compat43", false),
    ("compat44", false),
    ("complete_fullquote", true),
    ("direxpand", false),
    ("dirspell", false),
    ("dotglob", false),
    ("execfail", false),
    ("expand_aliases", false),
    ("extdebug", false),
    ("extglob", false),
    ("extquote", true),
    ("failglob", false),
    ("force_fignore", true),
    ("globasciiranges", true),
    ("globskipdots", true),
    ("globstar", false),
    ("gnu_errfmt", false),
    ("histappend", false),
    ("histreedit", false),
    ("histverify", false),
    ("hostcomplete", true),
    ("huponexit", false),
    ("inherit_errexit", false),
    ("interactive_comments", true),
    ("lastpipe", false),
    ("lithist", false),
    ("localvar_inherit", false),
    ("localvar_unset", false),
    ("login_shell", false),
    ("mailwarn", false),
    ("no_empty_cmd_completion", false),
    ("nocaseglob", false),
    ("nocasematch", false),
    ("noexpand_translation", false),
    ("nullglob", false),
    ("patsub_replacement", true),
    ("progcomp", true),
    ("progcomp_alias", false),
    ("promptvars", true),
    ("restricted_shell", false),
    ("shift_verbose", false),
    ("sourcepath", true),
    ("varredir_close", false),
    ("xpg_echo", false),
];

pub(crate) fn shopt_default_on(name: &str) -> Option<bool> {
    KNOWN_SHOPT_OPTIONS.iter().find(|(n, _)| *n == name).map(|(_, on)| *on)
}

// `compgen -A setopt`/`complete -A setopt`: valid `set -o NAME` names.
// Kept as its own list (rather than bash's full ~29-entry set) in sync
// with apply_shell_option's own match arms above -- only names that
// actually gate real bish behavior, same "don't advertise a name that
// does nothing" principle KNOWN_BISHOPTS follows for its own registry.
pub(crate) const SET_O_OPTIONS: &[&str] =
    &["errexit", "errtrace", "functrace", "monitor", "noclobber", "noglob", "nounset", "pipefail", "posix", "xtrace"];

// A bishopt option's type, plus its own default value -- for Str and
// Color that's the literal default text (parsed the same way a `--set`
// value would be, see `bishopt_value`); Bool has no stored default of
// its own, see `run_bishopt`'s own doc comment on why "unset" and
// "false" are the same state for a boolean option.
// #[allow(dead_code)]: no Bool entry has landed in KNOWN_BISHOPTS yet
// (Color, for the ui_col_* chrome colours, and Str, for "theme" -- see KNOWN_BISHOPTS'
// own doc comments -- both have real entries), so only Bool isn't
// actually constructed by production code, only by run_bishopt's tests
// (which build their own small registry covering all three). Drop this
// once a real Bool entry exists.
// Not `Copy`: an Int option carries the range it accepts, and a range
// isn't. Cloned at the two places that need an owned default instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BishOptDefault {
    Bool(bool),
    // Whole numbers, with the range they accept -- a column count or a
    // scroll margin is meaningless outside one, and catching that at
    // `--set` beats discovering it as a rendering bug.
    Int(i64, std::ops::RangeInclusive<i64>),
    Str(&'static str),
    Color(&'static str),
}

// A bishopt option's actual current value -- what `Shell::bishopts` maps
// names to once they've been explicitly `--set`. Color keeps the
// original source text alongside the parsed candidate list -- `bishopt
// get` prints that text back verbatim (see run_bishopt's own doc
// comment), not a re-serialization, so `--set accent cornflowerblue`
// reads back as "cornflowerblue" rather than "#6495ed". The list is
// CSS font-family-style fallback (csscolor::parse_terminal_list/pick):
// almost always exactly one candidate, but "#ff0000, -bish-ansi(1),
// -bish-red" is equally valid, with which one actually applies decided
// at render time by the terminal's own detected color support (see
// Shell::bishopt_color) -- not baked in here, since that support could
// in principle differ from whatever it was when `--set` ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BishOptValue {
    Bool(bool),
    Int(i64),
    Str(String),
    Color(String, Vec<crate::csscolor::TermColor>),
}

// bishopt's own known-option registry -- deliberately separate from (and
// deliberately *not* merged into) KNOWN_SHOPT_OPTIONS above: shopt exists
// solely so a real bash script's own defensive `shopt -s foo` preambles
// don't fail, and its namespace is bash's own to define, not bish's --
// mixing bish-specific settings into it risks a script written against
// real bash silently colliding with (or being silently ignored by) a
// bish-only setting sharing the same name.
//
// The `ui_col_*` entries are the first real, behavior-gating ones (no
// more dead-stub problem, unlike shopt_options' own history): each is
// one of bishedit::highlight's own HL_NAMES names, read every
// prompt redraw (see repl.rs's own construction of `highlight_ctx`) to
// build that redraw's ColorOverrides. Their default text is a `-bish-*`
// vendor color (csscolor::parse_terminal) naming the exact ANSI slot
// that HighlightKind's own hardcoded Indexed color (bishedit::highlight::
// default_style) already used -- so a fresh install renders identically
// to before this option existed, still resolved by the *user's own*
// terminal palette rather than some fixed guess at what "yellow" ought
// to look like. `--set`-ing an ordinary CSS color (or color-mix, hsl(),
// ...) is what actually pins it to a real, terminal-independent color.
// Shell::change_directory's own "refused by `set -r`" answer -- matched
// by the `cd` builtin so it can keep printing the message it always has.
pub(crate) const RESTRICTED: &str = "restricted";

/// The bash release bish answers `$BASH_VERSION` with, and reports
/// itself compatible with in `--version`. `BASH_VERSINFO` is this same
/// version taken apart, since scripts branch on `${BASH_VERSINFO[0]}`
/// far more often than they parse the string.
pub const BASH_VERSION: &str = "5.2.21(1)-release";
pub(crate) const BASH_VERSINFO: &[&str] = &["5", "2", "21", "1", "release", "x86_64-pc-linux-gnu"];

const KNOWN_BISHOPTS: &[(&str, BishOptDefault)] = &[
    // Which `::bish theme`-declared theme (if any) is currently active --
    // an ordinary Str option, set/unset/read the normal way (`bishopt
    // --set theme NAME`/`--unset theme`/`bishopt theme`), *outside* any
    // `::bish theme begin`/`end` declaration (setting it *inside* one
    // instead names the theme being declared -- see run_bish_theme_end's
    // own doc comment). Empty default = no active theme, same as every
    // other Str option's "never set" state. bishopt_value special-cases
    // this exact name to never additionally consult `self.themes` the
    // way every other option does -- a theme naming itself would be
    // circular and meaningless.
    ("theme", BishOptDefault::Str("")),
    // ...and the same for bish's own chrome rather than for the text it
    // shows: the browser's entry types, rendered markdown, diagnostics.
    // Identical machinery (see theme.rs, which mirrors
    // bishedit::highlight's) so `ui_col_directory` behaves exactly as
    // `::bish hl`'s own names do, and both land in a `::bish theme`
    // declaration without either knowing about themes.
    ("ui_col_directory", BishOptDefault::Color("-bish-blue")),
    ("ui_col_symlink", BishOptDefault::Color("-bish-cyan")),
    ("ui_col_archive", BishOptDefault::Color("-bish-magenta")),
    ("ui_col_executable", BishOptDefault::Color("-bish-green")),
    ("ui_col_heading", BishOptDefault::Color("-bish-yellow")),
    ("ui_col_code", BishOptDefault::Color("-bish-green")),
    ("ui_col_link", BishOptDefault::Color("-bish-cyan")),
    ("ui_col_quote", BishOptDefault::Color("-bish-blue")),
    ("ui_col_error", BishOptDefault::Color("-bish-red")),
    ("ui_col_warning", BishOptDefault::Color("-bish-yellow")),
    ("ui_col_info", BishOptDefault::Color("-bish-blue")),
    ("ui_col_hint", BishOptDefault::Color("-bish-cyan")),
    // How the file editor handles a line wider than the pane. `wrap`
    // off is the current behaviour and vim's `nowrap`: the line scrolls
    // sideways. On, it is broken across as many rows as it needs.
    //
    // The three that shape a wrapped line default *on* where vim
    // defaults them off, which is a deliberate divergence: vim's
    // defaults are a 1991 artifact that essentially every vimrc
    // overrides, and breaking mid-word with no indent and no marker is
    // not something anyone chooses. They do nothing at all while `wrap`
    // is off.
    ("wrap", BishOptDefault::Bool(false)),
    ("linebreak", BishOptDefault::Bool(true)),
    ("breakindent", BishOptDefault::Bool(true)),
    // What a continued row opens with. Empty for none.
    ("showbreak", BishOptDefault::Str("\u{21B3} ")),
    // Wrap at this column rather than at the pane's edge -- VS Code's
    // own `wordWrapColumn`. 0 means the pane's width; anything else is
    // capped by it, so a narrow pane still wraps at the pane.
    ("wrap_column", BishOptDefault::Int(0, 0..=1000)),
    // vim's own: keep this many lines visible above and below the
    // cursor rather than letting it sit against the top or bottom of
    // the pane. Applies with or without wrapping, unlike its horizontal
    // counterpart just below. Capped at half the pane, since a margin
    // wider than that has no middle left to keep the cursor in.
    ("scrolloff", BishOptDefault::Int(0, 0..=200)),
    // No zero, deliberately: bash's HISTFILESIZE=0 truncates the file to
    // nothing, which is a footgun for anyone who reads 0 as "unlimited".
    // The top of the range is the unlimited-in-practice answer instead.
    ("history_size", BishOptDefault::Int(10_000, 100..=1_000_000)),
    // The extra characters -- beyond letters and digits, which are
    // always word characters in any script -- that `w`/`b`/`e`, `iw`,
    // `*` and `Ctrl-W` treat as part of a word. `_` is what every
    // motion has always assumed; `-` suits kebab-case, `$` shell
    // variables.
    ("iskeyword", BishOptDefault::Str("_")),
    // ...and the two that matter while `wrap` is *off*. `sidescrolloff`
    // is vim's own: keep this many columns visible either side of the
    // cursor rather than letting it sit against the edge.
    ("sidescrolloff", BishOptDefault::Int(0, 0..=200)),
    // vim's `listchars` `extends`/`precedes`: shown in the last/first
    // column when a line continues off that edge. Empty by default, so
    // rendering is unchanged until asked for.
    ("extends", BishOptDefault::Str("")),
    ("precedes", BishOptDefault::Str("")),
    // Which languages get a tabular display -- columns lined up on
    // screen without a character of the file changing. A language glob,
    // the same shape and the same matcher `abbr --lang` uses, so
    // `!(csv)` turns it off for CSV and leaves it on elsewhere.
    //
    // `*` by default, and that is not the same as "on for everything":
    // a language with no tabular form of its own is simply left alone
    // (bishedit::tabular::style says which have one), so matching
    // everything means "wherever this is implemented, use it".
    ("tabular", BishOptDefault::Str("*")),
    // Whether bish pays attention to `.gitignore` at all. On, the file
    // browser leaves ignored files out of its listing until `i` asks
    // for them; off, there is no such thing as an ignored file anywhere
    // and every listing is what is actually on disk.
    //
    // Named for the file rather than for the browser (`browser_
    // gitignore`, say) on purpose: the question it answers is "does
    // bish honour .gitignore", which is the thing a person goes looking
    // for, and the answer should stay one option as more of bish comes
    // to ask it -- the fuzzy finder next.
    ("gitignore", BishOptDefault::Bool(true)),
    ("lsp", BishOptDefault::Bool(true)),
    ("lsp_timeout_ms", BishOptDefault::Int(1000, 0..=10000)),
    // Whether a URL, a markdown link's destination or a path that
    // resolves is drawn as a real OSC 8 terminal hyperlink -- clickable
    // where the terminal supports it. On: a terminal that doesn't know
    // OSC 8 skips the sequence and shows the text exactly as before, so
    // there is nothing to detect and nothing to degrade. Off is for
    // wanting the text and only the text.
    ("hyperlinks", BishOptDefault::Bool(true)),
    // How much of a split's own space its pane dividers may take before
    // the panes furthest from the focus fold away behind a single
    // divider that stands for all of them (see repl.rs's
    // collapsed_runs). A percentage; 100 turns folding off entirely and
    // lets the dividers have the whole split if that is what the pane
    // count comes to.
    ("divider_budget", BishOptDefault::Int(25, 0..=100)),
    // vim's own `relativenumber`: number each line by its distance from
    // the cursor's, which is what makes `12j` or `d8k` something you
    // read off the screen instead of counting.
    //
    // The cursor's own line keeps its absolute number -- vim gets that
    // by setting `number` *and* `relativenumber` together, and bish has
    // no `number` to turn off (the gutter always numbers), so this is
    // the only reading of it that leaves the option meaningful.
    ("relativenumber", BishOptDefault::Bool(false)),
    ("ignorecase", BishOptDefault::Bool(false)),
    ("inlayhints", BishOptDefault::Bool(true)),
    ("smartcase", BishOptDefault::Bool(false)),
    // Whether a project's own `.editorconfig` wins over the settings
    // just below. On, it does -- a project's conventions are the point
    // of the file, and the settings here are your own preference for
    // everywhere *else*. Off ignores `.editorconfig` entirely, which is
    // how you override one from the colon line without editing it:
    // `:bishopt --set editorconfig off` and your own settings take the
    // buffer back on the next redraw.
    ("editorconfig", BishOptDefault::Bool(true)),
    // Whether bish asks the terminal to report mouse events at all.
    // On, clicking places the cursor and dragging selects. Off is not
    // just "ignore the mouse": reporting is never turned on, which is
    // what gives the *terminal's own* selection back -- with reporting
    // on, dragging is bish's gesture and there is no way to sweep a
    // region for the system clipboard.
    ("mouse", BishOptDefault::Bool(true)),
    // Whether the terminal's cursor changes shape to show the mode: a
    // block in Normal and Visual, a bar in Insert, an underline in
    // Replace. Off leaves the cursor however the terminal draws it, for
    // a terminal that renders one of the shapes badly or not at all.
    ("cursorshape", BishOptDefault::Bool(true)),
    // vim's names for the four things `.editorconfig` calls
    // `indent_style`, `indent_size`, `tab_width` and
    // `insert_final_newline`. `trim_trailing_whitespace` keeps
    // EditorConfig's own name, since vim has nothing to borrow.
    ("expandtab", BishOptDefault::Bool(true)),
    ("shiftwidth", BishOptDefault::Int(4, 1..=64)),
    ("tabstop", BishOptDefault::Int(4, 1..=64)),
    ("fixendofline", BishOptDefault::Bool(true)),
    ("trim_trailing_whitespace", BishOptDefault::Bool(false)),
    // vim's `fileformat`, and the one option here whose default is
    // *nothing*: an empty value means "whatever this file already had",
    // which is the only answer that doesn't rewrite every line of a
    // CRLF file the first time you save it.
    ("fileformat", BishOptDefault::Str("")),
];

// Every bishopt option name, in registry order -- the source of truth
// for Tab completion (bishedit::completion::bishopt_candidates).
//
// Deliberately *not* hand-duplicated into bishedit the way KNOWN_BUILTINS
// is. That duplication exists to keep the editor's syntax *analysis*
// independent of the execution engine, and a name drifting out of sync
// there costs one word's colour. Here the whole feature is "tell me which
// options exist", so a list that can drift from the registry would
// advertise options that aren't real and hide ones that are -- worse than
// having no completion at all.
pub fn bishopt_names() -> Vec<&'static str> {
    KNOWN_BISHOPTS.iter().map(|(name, _)| *name).collect()
}

// The values `bishopt --set NAME` accepts, when they are a fixed set.
// Only a boolean has one; a string, colour or number is free text, and
// offering a guess at it would be inventing options rather than
// reporting them.
pub fn bishopt_values(name: &str) -> &'static [&'static str] {
    match KNOWN_BISHOPTS.iter().find(|(n, _)| *n == name) {
        Some((_, BishOptDefault::Bool(_))) => &["on", "off"],
        _ => &[],
    }
}

// The `::bish` namespace's own vocabulary, for Tab completion
// (bishedit::completion::bish_candidates).
//
// Here rather than in bishedit for the same reason `bishopt_names` is:
// these subcommands are dispatched a few hundred lines above in
// `run_bish`/`run_lsp`/`run_hl`, and a hand-copied list in another
// module would advertise subcommands that do not exist and hide ones
// that do. `every_bish_subcommand_is_dispatched` is what keeps the two
// honest about each other.
//
// Only the canonical spellings. The aliases (`win`, `list`, `remove`,
// `prev`) are all understood, but a completion list is a menu, and a
// menu should show one name per thing -- the same call
// `bishopt_candidates` already makes about `-s`/`--set`.
pub fn bish_subcommands() -> &'static [&'static str] {
    &["theme", "window", "hook", "hl", "lsp"]
}

// The second level: what follows `::bish <sub>`. Empty for a
// subcommand that takes something other than a fixed word.
pub fn bish_sub_subcommands(sub: &str) -> &'static [&'static str] {
    match sub {
        "theme" => &["begin", "end"],
        "window" | "win" => &["next", "previous", "new", "rename", "ls", "select"],
        "hook" => &["ls", "add", "rm", "help"],
        "lsp" => &["ls", "add", "rm", "status", "log", "restart", "help"],
        "map" => &["--mode=", "--erase", "--list", "help"],
        _ => &[],
    }
}

// The flags `::bish lsp add` takes, in the spelling its own usage line
// uses. `=`-terminated because each takes a value, and the completion
// menu offering `--lang=` rather than `--lang` puts the cursor where
// the next thing to type goes.
pub fn lsp_add_flags() -> &'static [&'static str] {
    &["--lang=", "--root=", "--root-cmd=", "--apply-edits=", "--setting="]
}

// The values `--apply-edits=` accepts -- the one flag there with a
// fixed set, the same rule `bishopt_values` follows.
pub fn lsp_apply_edits_values() -> &'static [&'static str] {
    &["scoped", "never", "always"]
}

// One line about each bishopt, for `bishopt --describe` and the
// `:help options` page.
//
// A parallel table rather than a third field on KNOWN_BISHOPTS: adding
// one would touch every consumer of that tuple for a string only two
// call sites read. The obvious risk of a parallel table is drift -- an
// option nobody wrote a line for -- and that is what
// `every_bishopt_is_described` removes, by failing the moment the two
// lists disagree.
//
// One sentence each, saying what the option *does* rather than
// restating its name. The type, default and legal range come from
// KNOWN_BISHOPTS and are printed alongside, so none of that is repeated
// here.
const BISHOPT_HELP: &[(&str, &str)] = &[
    ("theme", "The active `::bish theme` declaration, if any."),
    ("ui_col_directory", "Interface colour: directories in the file browser."),
    ("ui_col_symlink", "Interface colour: symlinks in the file browser."),
    ("ui_col_archive", "Interface colour: archives in the file browser."),
    ("ui_col_executable", "Interface colour: executables in the file browser."),
    ("ui_col_heading", "Interface colour: headings in rendered markdown."),
    ("ui_col_code", "Interface colour: code spans and blocks in rendered markdown."),
    ("ui_col_link", "Interface colour: links in rendered markdown."),
    ("ui_col_quote", "Interface colour: the bar beside a rendered block quote."),
    ("ui_col_error", "Interface colour: errors, in the gutter and under the text."),
    ("ui_col_warning", "Interface colour: warnings, in the gutter and under the text."),
    ("ui_col_info", "Interface colour: informational findings, in the gutter and under the text."),
    ("ui_col_hint", "Interface colour: hints, in the gutter and under the text."),
    ("wrap", "Break a line too long for the pane across rows instead of scrolling sideways."),
    ("linebreak", "While wrapping, break at word boundaries rather than mid-word."),
    ("breakindent", "While wrapping, indent continued rows under the line they belong to."),
    ("showbreak", "What a continued row opens with. Empty for nothing."),
    ("wrap_column", "Wrap at this column rather than at the pane's edge. 0 means the pane."),
    ("scrolloff", "Keep this many lines visible above and below the cursor."),
    ("history_size", "How many commands the history file keeps. Older ones are dropped oldest-first."),
    ("iskeyword", "Characters besides letters and digits that count as part of a word."),
    ("sidescrolloff", "Keep this many columns visible either side of the cursor, while not wrapping."),
    ("extends", "Shown in the last column when a line continues off the right edge."),
    ("precedes", "Shown in the first column when a line continues off the left edge."),
    ("tabular", "Which languages draw their columns lined up. A language glob, as `abbr --lang` uses."),
    ("gitignore", "Honour `.gitignore`: the browser and completion leave ignored files out."),
    ("lsp", "Use the language servers registered with `::bish lsp add`."),
    ("lsp_timeout_ms", "How long to wait for a language server to answer a question like hover."),
    ("hyperlinks", "Emit OSC 8 terminal hyperlinks for URLs, links and resolved paths."),
    ("divider_budget", "How much of a split its pane dividers may take, as a percentage, before panes fold away."),
    ("relativenumber", "Number lines by their distance from the cursor's."),
    ("ignorecase", "Ignore case when searching, unless smartcase says otherwise."),
    ("inlayhints", "Draw the language server's inline parameter-name and inferred-type hints."),
    ("smartcase", "With ignorecase on, an uppercase letter in the pattern makes that one search case-sensitive again."),
    ("editorconfig", "Let a project's `.editorconfig` override the settings below it."),
    ("mouse", "Ask the terminal to report mouse events. Off gives the terminal's own selection back."),
    ("cursorshape", "Change the terminal's cursor shape to show the editor's mode."),
    ("expandtab", "Indent with spaces rather than a literal tab."),
    ("shiftwidth", "How many columns one indent is."),
    ("tabstop", "How many columns a literal tab draws as."),
    ("fixendofline", "Give a file a final newline on save if it hasn't got one."),
    ("trim_trailing_whitespace", "Strip whitespace from the end of every line on save."),
    ("fileformat", "Line endings to write: `unix`, `dos`, `mac`. Empty keeps whatever the file had."),
];

// Best-effort terminal color-capability detection via the same
// environment variables most terminal-aware CLI tools already check --
// no terminfo database dependency. COLORTERM=truecolor/24bit is the de
// facto signal modern terminal emulators set for full 24-bit support;
// TERM containing "256color" is the traditional signal for an indexed
// 256-color palette; TERM=dumb means "assume nothing, not even the
// basic 16"; anything else assumes the basic 16 ANSI colors, safe even
// for a terminal from decades ago.
fn detect_color_support() -> crate::csscolor::ColorSupport {
    use crate::csscolor::ColorSupport;
    if matches!(std::env::var("COLORTERM").as_deref(), Ok("truecolor") | Ok("24bit")) {
        return ColorSupport::Truecolor;
    }
    match std::env::var("TERM").as_deref() {
        Ok("dumb") => ColorSupport::None,
        Ok(term) if term.contains("256color") => ColorSupport::Ansi256,
        Ok(_) => ColorSupport::Ansi16,
        Err(_) => ColorSupport::None,
    }
}

fn is_known_builtin(name: &str) -> bool {
    KNOWN_BUILTINS.contains(&name)
}

/// Where `name` is found, searching `path_var` -- which is the *shell's*
/// `PATH`, handed in rather than read from the process environment.
///
/// The two are the same until a script assigns `PATH`, and a shell
/// variable's home is the shell.
pub(crate) fn resolve_in_path(name: &str, path_var: &str) -> Option<String> {
    if name.contains('/') {
        return if std::path::Path::new(name).is_file() { Some(name.to_string()) } else { None };
    }
    for dir in path_var.split(':') {
        let candidate = std::path::Path::new(dir).join(name);
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(&candidate) {
                Ok(meta) if meta.permissions().mode() & 0o111 != 0 => {
                    return Some(candidate.to_string_lossy().into_owned());
                }
                _ => continue,
            }
        }
        #[cfg(not(unix))]
        {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// `base` and `target` joined and then normalised *textually* -- `.`
/// dropped, `..` cancelling the component in front of it -- without
/// asking the filesystem anything.
///
/// This is what makes a shell's `cd` remember the route rather than the
/// destination. `a/link/..` is `a` here, where the kernel would answer
/// with the parent of whatever `link` points at; both are defensible
/// and a shell picks the first, which is why `cd -P` exists to ask for
/// the other.
fn logical_path(base: &std::path::Path, target: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let joined = if target.is_absolute() { target.to_path_buf() } else { base.join(target) };
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in joined.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => parts.clear(),
            Component::CurDir => {}
            // At the root there is nothing above to go to, and bash
            // leaves `/..` as `/`.
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_os_string()),
        }
    }
    let mut out = std::path::PathBuf::from("/");
    for part in parts {
        out.push(part);
    }
    out
}

pub(crate) fn command_own_redirects(cmd: &parser::Command) -> &[Redirect] {
    match cmd {
        parser::Command::If { redirects, .. } => redirects,
        parser::Command::While { redirects, .. } => redirects,
        parser::Command::For { redirects, .. } => redirects,
        parser::Command::CFor { redirects, .. } => redirects,
        parser::Command::Select { redirects, .. } => redirects,
        parser::Command::Case { redirects, .. } => redirects,
        parser::Command::Group(_, redirects) => redirects,
        parser::Command::Subshell(_, redirects) => redirects,
        parser::Command::Arith(_, redirects) => redirects,
        parser::Command::Test(_, redirects) => redirects,
        _ => &[],
    }
}

/// A plain `pipe(2)`, as two owned fds.
///
/// `Stdio::piped()` cannot serve here: it hands the read end back only
/// as a spawned child's `ChildStdout`, and the stage on the writing end
/// of this one is not a child -- it is this shell.
fn make_pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    unsafe extern "C" {
        fn pipe2(fds: *mut i32, flags: i32) -> i32;
    }
    // O_CLOEXEC, and it is load-bearing rather than hygiene. Both ends
    // are held by this shell while the *other* stages are spawned, so
    // without it every one of them inherits the write end -- and a
    // reader downstream then waits forever for an end-of-input that
    // cannot arrive, because a process that never writes is holding the
    // pipe open. `echo x | cat` printed its `x` and hung.
    //
    // The two places this shell does want them inherited both go
    // through `dup2`, which clears the flag on the new descriptor: the
    // read end onto a spawned stage's fd 0, and the write end onto fd 1
    // for the stage running here.
    const O_CLOEXEC: i32 = 0o2000000;
    let mut fds = [0i32; 2];
    if unsafe { pipe2(fds.as_mut_ptr(), O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    use std::os::fd::FromRawFd;
    Ok(unsafe { (std::os::fd::OwnedFd::from_raw_fd(fds[0]), std::os::fd::OwnedFd::from_raw_fd(fds[1])) })
}

/// `make_pipe`, for the scheduler's own tests: they need a real pipe
/// with the same close-on-exec treatment, and this is where that lives.
#[cfg(test)]
pub(crate) fn make_pipe_for_test() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    make_pipe()
}

fn kill_all(children: Vec<(usize, std::process::Child)>) {
    for (_, mut c) in children {
        let _ = c.kill();
        let _ = c.wait();
    }
}

// Shared by write_command_error and unset's readonly-variable diagnostic:
// writes to `target`'s file (append) if set, else falls back to the
// session's own sink (still real stderr today, see OutputSink's doc
// comment). A free function, not a Shell method, so it takes the sink
// explicitly rather than through self.sink_err/sh_eprintln! -- both of
// its callers already have a `&Shell`/`&mut Shell` in scope to pass
// `.sink` from.
pub(crate) fn write_diagnostic(target: &Option<String>, msg: &str, sink: OutputSink) {
    match target {
        Some(path) => {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                use std::io::Write;
                let _ = writeln!(f, "{}", msg);
            } else {
                sink.write_err(&format!("{}\n", msg));
            }
        }
        None => sink.write_err(&format!("{}\n", msg)),
    }
}

// new_virtual_child's real callers now: repl.rs's window/pane-split code,
// and Shell::run_in_child_shell (every foreground subshell/command-
// substitution/proc-sub). This is still useful as a headless, no-
// rendering/REPL-needed exercise of the primitive itself. `cargo test`
// needs no external crate, matching the zero-dependency stance elsewhere.
// Every process the shell starts on a script's behalf must inherit that
// shell's environment, which means going through `Shell::command`. This
// is what makes that true rather than customary.
//
// A `Command::new` that skips the helper produces a child with a stale
// environment and no complaint from anyone -- the failure is a wrong
// value somewhere downstream, not a crash, which is the worst shape a
// mistake can have. So the rule is checked rather than remembered: this
// reads the crate's own source and fails on any occurrence outside the
// list below.
//
// The list is keyed by count as well as by file, so *adding* a spawn to
// a file that already has one trips it too. An entry is a claim that
// those spawns are bish's own tooling rather than the script's, and it
// has to be argued for in the `why` column.
//
// Test modules are not counted: a test that spawns something is not the
// shell starting a process for a script. Everything from the first
// `#[cfg(test)]` in a file onwards is skipped, which is where this
// codebase puts them.
#[cfg(test)]
mod spawn_guard {
    /// `(file, how many, why it is not a shell spawn)`.
    const ALLOWED: &[(&str, usize, &str)] = &[
        ("src/exec.rs", 1, "`Shell::command` itself, the one place that builds a child's environment"),
        (
            "src/git.rs",
            7,
            "runs `git` to answer questions the prompt and the editor ask about the repository -- bish's own tooling, inheriting bish's own environment as any of its subprocesses does",
        ),
        ("src/lspclient.rs", 1, "starts a language server for the editor, which is bish's, not the script's"),
        ("src/bishedit/registers.rs", 2, "hands the system clipboard to and from the editor"),
        (
            "src/session.rs",
            1,
            "the session daemon re-execs this binary to bootstrap itself; it has no Shell yet, and the one it builds afterwards seeds from the environment it was given",
        ),
        (
            "src/repl.rs",
            1,
            "asks a fresh bish for a language server's project root, one memoised spawn per directory -- a question about the machine rather than a command the script wrote",
        ),
        (
            "src/compgen.rs",
            1,
            "re-execs bish to run a completion function -- the shell's variables reach that child through the preamble as source text, not through its environment",
        ),
    ];

    #[test]
    fn every_process_the_shell_starts_goes_through_command() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found: Vec<(String, usize)> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("src is readable").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("a source file is readable");
                // Everything from the first test module onwards is not
                // the shell starting anything.
                let body = match source.find("\n#[cfg(test)]\n") {
                    Some(cut) => &source[..cut],
                    None => &source[..],
                };
                let count = body.matches("Command::new(").count();
                if count > 0 {
                    let relative = path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap_or(&path);
                    found.push((relative.to_string_lossy().replace('\\', "/"), count));
                }
            }
        }
        found.sort();

        let mut problems: Vec<String> = Vec::new();
        for (file, count) in &found {
            match ALLOWED.iter().find(|(f, ..)| f == file) {
                None => problems.push(format!(
                    "{file} starts {count} process(es) without `Shell::command`, so they would not inherit the shell's environment.\n     \
                     Use `shell.command(..)`, or add an entry to ALLOWED saying why this one is bish's own and not the script's."
                )),
                Some((_, allowed, why)) if allowed != count => problems.push(format!(
                    "{file} has {count} `Command::new` where ALLOWED says {allowed}.\n     \
                     The existing ones are there because: {why}\n     \
                     If the new one is a shell spawn it needs `shell.command(..)`; if it is not, update the count and extend the reason."
                )),
                Some(_) => {}
            }
        }
        for (file, ..) in ALLOWED {
            if !found.iter().any(|(f, _)| f == file) {
                problems.push(format!("{file} is in ALLOWED but no longer starts anything -- remove its entry."));
            }
        }
        assert!(problems.is_empty(), "\n  - {}", problems.join("\n  - "));
    }
}

#[cfg(test)]
mod tests {
    // The property the whole cwd change is for: a path a script names
    // resolves against the *shell's* directory, and the process's is
    // not consulted. Indistinguishable in a single shell -- the two are
    // kept in step -- so this sets them apart deliberately, which is
    // the state two interleaved pipeline stages are permanently in.
    // Pinned to UTC so the expected strings do not depend on where this
    // runs -- the same `TZ`+`tzset` trick git.rs's own date test uses.
    fn utc_printf(format: &str, args: &[&str]) -> String {
        unsafe extern "C" {
            fn tzset();
        }
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("TZ").ok();
        unsafe { std::env::set_var("TZ", "UTC0") };
        unsafe { tzset() };
        let values: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let (mut idx, mut out) = (0, String::new());
        let _ = super::printf_format_once(format, &values, &mut idx, &mut out);
        match previous {
            Some(tz) => unsafe { std::env::set_var("TZ", tz) },
            None => unsafe { std::env::remove_var("TZ") },
        }
        unsafe { tzset() };
        out
    }

    #[test]
    fn a_disabled_builtin_is_not_a_builtin_to_anything_that_asks() {
        // The dispatch already ran the external; everything that
        // *classifies* a name had to agree, or `type` called it a
        // builtin while `/usr/bin/echo` was what ran. Checked against
        // real bash, which reports the external for all of these.
        let mut sh = Shell::new();
        assert!(sh.is_active_builtin("echo"));
        crate::builtins::shell::run_enable(&mut sh, &["-n".to_string(), "echo".to_string()]);
        assert!(!sh.is_active_builtin("echo"), "type, command -v and the pipeline self-exec all ask this");
        assert!(is_known_builtin("echo"), "still a builtin bish knows about -- `enable echo` has to find it");
        crate::builtins::shell::run_enable(&mut sh, &["echo".to_string()]);
        assert!(sh.is_active_builtin("echo"));
    }

    #[test]
    fn type_t_prints_the_kind_and_nothing_else() {
        // `[ "$(type -t x)" = function ]` is how a script asks, and a
        // sentence fails that test however true it reads.
        let mut sh = Shell::new();
        let out = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        sh.set_sink_capture(out.clone());
        sh.run_source_here("f() { :; }\ntype -t f\ntype -t echo\ntype -t nosuchthing_at_all\n", "<test>");
        let text = out.borrow().clone();
        assert!(text.contains("function"), "{text:?}");
        assert!(text.contains("builtin"), "{text:?}");
        // A name it does not know says nothing at all -- the empty
        // answer is the answer.
        assert!(!text.contains("not found"), "{text:?}");
    }

    #[test]
    fn funcname_bash_source_and_bash_lineno_describe_the_stack() {
        // Checked against real bash on this exact pair of files. The
        // sourced library is the point: BASH_SOURCE names where each
        // function is *defined*, BASH_LINENO the line in the file one
        // level out where it was called, and those stop being the same
        // file the moment anything is sourced.
        let dir = std::env::temp_dir().join(format!("bish-funcname-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lib = dir.join("lib.sh");
        let main = dir.join("main.sh");
        std::fs::write(&lib, "inner() {\n  n=\"${FUNCNAME[*]}\"\n  s=\"${BASH_SOURCE[*]}\"\n  l=\"${BASH_LINENO[*]}\"\n}\nouter() {\n  inner\n}\n")
            .unwrap();
        std::fs::write(&main, format!("source {}\nouter\ntop=\"${{FUNCNAME[*]}}\"\n", lib.display())).unwrap();

        let mut sh = Shell::new();
        sh.set_script_args(main.display().to_string(), Vec::new());
        // This test *is* the "running a script file" case, which is
        // what puts the outermost `main` frame on the stack -- `bish -c`
        // has no such frame, and neither does `bash -c`. See
        // Shell::running_a_script.
        sh.running_a_script = true;
        sh.run_source_here(&std::fs::read_to_string(&main).unwrap(), &main.display().to_string());

        assert_eq!(sh.debug_peek_var("n").as_deref(), Some("inner outer main"));
        assert_eq!(
            sh.debug_peek_var("s").as_deref(),
            Some(format!("{} {} {}", lib.display(), lib.display(), main.display()).as_str()),
            "where each function is defined, not where it was called"
        );
        // `inner` is called on line 7 of the library, `outer` on line 2
        // of the script, and 0 stands for the script itself.
        assert_eq!(sh.debug_peek_var("l").as_deref(), Some("7 2 0"));
        // Outside every function bash has no FUNCNAME at all, so a
        // script testing ${FUNCNAME[0]} sees nothing rather than "main".
        assert_eq!(sh.debug_peek_var("top").as_deref(), Some(""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn caller_walks_out_through_the_calls_that_got_here() {
        // Checked line-for-line against real bash on the same script.
        let dir = std::env::temp_dir().join(format!("bish-caller-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("c.sh");
        std::fs::write(&script, "a() {\n  r0=$(caller 0)\n  r1=$(caller 1)\n  r2=$(caller 2)\n  rb=$(caller)\n}\nb() {\n  a\n}\nb\n").unwrap();
        let mut sh = Shell::new();
        let src = script.display().to_string();
        // Run as a script rather than via `source`: `caller` reports the
        // file a frame's line is in, and `source` does not update
        // `script_name`, so a sourced file still reports the outer one.
        // Noted rather than fixed here -- it is bash's `BASH_SOURCE`
        // that bish has none of, and that is its own work.
        sh.set_script_args(src.clone(), Vec::new());
        sh.run_source_here(&std::fs::read_to_string(&script).unwrap(), &src);
        // The innermost call: the line it is on, the function that line
        // is in, and the file.
        assert_eq!(sh.debug_peek_var("r0").as_deref(), Some(format!("8 b {src}").as_str()));
        // ...then the call to *that* function. Nothing called `b`, and
        // bash names that `main`.
        assert_eq!(sh.debug_peek_var("r1").as_deref(), Some(format!("10 main {src}").as_str()));
        // Past the top is empty, which is what stops `while caller $i`.
        assert_eq!(sh.debug_peek_var("r2").as_deref(), Some(""));
        // Bare `caller` is the short form: no function name.
        assert_eq!(sh.debug_peek_var("rb").as_deref(), Some(format!("8 {src}").as_str()));

        // Outside any function there is nothing to report.
        let mut sh = Shell::new();
        assert_eq!(crate::builtins::shell::run_caller(&mut sh, &[]), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enable_takes_a_builtin_out_of_service_and_puts_it_back() {
        let mut sh = Shell::new();
        assert!(!sh.disabled_builtins.contains("echo"));
        assert_eq!(crate::builtins::shell::run_enable(&mut sh, &["-n".to_string(), "echo".to_string()]), 0);
        assert!(sh.disabled_builtins.contains("echo"));
        assert_eq!(crate::builtins::shell::run_enable(&mut sh, &["echo".to_string()]), 0);
        assert!(!sh.disabled_builtins.contains("echo"));

        // A name that is not a builtin is an error, and does not become
        // one by being named.
        assert_eq!(crate::builtins::shell::run_enable(&mut sh, &["-n".to_string(), "nosuch".to_string()]), 1);
        assert!(!sh.disabled_builtins.contains("nosuch"));
        // Dynamic loading is refused by name rather than ignored.
        assert_eq!(crate::builtins::shell::run_enable(&mut sh, &["-f".to_string(), "mod".to_string()]), 2);
    }

    #[test]
    fn every_builtin_is_described_and_every_description_is_a_builtin() {
        // The two tables have to agree, and neither is derived from the
        // other -- so adding a builtin without a line here fails the
        // build rather than quietly listing a blank one.
        use std::collections::HashSet;
        let known: HashSet<&str> = KNOWN_BUILTINS.iter().copied().collect();
        let described: HashSet<&str> = BUILTIN_HELP.iter().map(|(n, _)| *n).collect();
        let mut missing: Vec<&&str> = known.difference(&described).collect();
        let mut extra: Vec<&&str> = described.difference(&known).collect();
        missing.sort_unstable();
        extra.sort_unstable();
        assert!(missing.is_empty(), "builtins with no help line: {missing:?}");
        assert!(extra.is_empty(), "help lines for things that are not builtins: {extra:?}");
        // One line each, and short enough to read in a list of sixty.
        for (name, summary) in BUILTIN_HELP {
            assert!(!summary.contains('\n'), "{name}: one line");
            assert!(summary.len() <= 72, "{name}: {} chars is too long for an index", summary.len());
            assert!(summary.ends_with('.'), "{name}: a sentence");
        }
    }

    #[test]
    fn globignore_drops_matches_under_pathname_rules() {
        // Checked against real bash. The second case is the one that
        // matters: `*.o` leaves `sub/x.o` alone, because `*` does not
        // cross a `/` here -- the opposite of what "matched against the
        // filename" first suggests.
        // `cd` moves the real process, so this holds the same lock every
        // other cwd-touching test here holds, and puts it back.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("bish-globignore-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for n in ["a.c", "a.o", "sub/x.o", "sub/y.c"] {
            std::fs::write(dir.join(n), "").unwrap();
        }
        let run = |script: &str| {
            let mut sh = Shell::new();
            sh.run_source_here(&format!("cd {}; {script}", dir.display()), "<test>");
            sh.debug_peek_var("r").unwrap_or_default()
        };
        assert_eq!(run("r=$(echo *)"), "a.c a.o sub");
        assert_eq!(run("GLOBIGNORE='*.o'; r=$(echo *)"), "a.c sub");
        assert_eq!(run("GLOBIGNORE='*.o'; r=$(echo sub/*)"), "sub/x.o sub/y.c", "* does not cross a /");
        assert_eq!(run("GLOBIGNORE='sub/*.o'; r=$(echo sub/*)"), "sub/y.c");
        // Everything ignored means the pattern matched nothing, and
        // nullglob-off leaves an unmatched pattern as its own text.
        assert_eq!(run("GLOBIGNORE='*'; r=$(echo *)"), "*");
        restore_cwd();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ifs_starts_out_set_to_the_default_the_way_bash_does() {
        // Not cosmetic. The standard way to change IFS for a moment is
        // `old=$IFS; IFS=,; ...; IFS=$old`, and with IFS merely unset
        // that captured the empty string and restored it as "split on
        // nothing" -- so everything after it stopped splitting at all.
        let mut sh = Shell::new();
        sh.run_source_here("old=$IFS; IFS=,; IFS=$old; s=\"a b c\"; set -- $s; n=$#", "<test>");
        assert_eq!(sh.debug_peek_var("n").as_deref(), Some("3"), "splitting survives a save/restore");
        assert_eq!(sh.debug_peek_var("IFS").as_deref(), Some(" \t\n"));

        // The two states that must stay distinguishable: empty means
        // "do not split", and unset still means the default.
        let mut sh = Shell::new();
        sh.run_source_here("IFS=; s=\"a b c\"; set -- $s; n=$#", "<test>");
        assert_eq!(sh.debug_peek_var("n").as_deref(), Some("1"), "IFS= disables splitting");

        let mut sh = Shell::new();
        sh.run_source_here("unset IFS; s=\"a b c\"; set -- $s; n=$#", "<test>");
        assert_eq!(sh.debug_peek_var("n").as_deref(), Some("3"), "unset still falls back to the default");
    }

    #[test]
    fn a_prefix_assignment_reaches_a_builtin_and_then_goes_away() {
        // `IFS=: read a b` used to split on the old IFS: the assignment
        // was passed as *environment*, which only an external command
        // ever sees, and a builtin reads the shell's own variables.
        // Checked against real bash, which gives x/y and leaves IFS
        // alone.
        let mut sh = Shell::new();
        sh.run_source_here("IFS=: read a b <<< \"x:y\"", "<test>");
        assert_eq!(sh.debug_peek_var("a").as_deref(), Some("x"));
        assert_eq!(sh.debug_peek_var("b").as_deref(), Some("y"));

        // Gone afterwards, and restored to what it was rather than to
        // empty -- an assignment that outlived its command would be a
        // worse bug than the one being fixed.
        let mut sh = Shell::new();
        sh.run_source_here("FOO=original; FOO=temp true; echo done", "<test>");
        assert_eq!(sh.debug_peek_var("FOO").as_deref(), Some("original"));

        // A name that was unset comes back unset, not set-to-empty:
        // `${X-default}` and `set -u` both tell the difference.
        let mut sh = Shell::new();
        sh.run_source_here("NEVER_SET=temp true; echo hi", "<test>");
        assert_eq!(sh.debug_peek_var("NEVER_SET"), Option::None);
    }

    #[test]
    fn printf_formats_a_timestamp_the_way_bash_does() {
        // Checked against real bash with the same inputs.
        assert_eq!(utc_printf("%(%Y-%m-%d)T", &["0"]), "1970-01-01");
        assert_eq!(utc_printf("%(%F %T)T", &["1000000000"]), "2001-09-09 01:46:40");
        // `%s` is the one directive a broken-down time cannot answer on
        // its own, so the timestamp is threaded through for it.
        assert_eq!(utc_printf("%(%s)T", &["1234567890"]), "1234567890");
        // Directives the PS1 escapes never needed.
        assert_eq!(utc_printf("%(%R|%D|%u|%w|%C)T", &["1000000000"]), "01:46|09/09/01|7|0|20");
        assert_eq!(utc_printf("%(%z)T", &["0"]), "+0000");
    }

    #[test]
    fn a_malformed_time_conversion_is_printed_back_rather_than_swallowed() {
        // bash calls this an invalid time format and warns; either way
        // the text has to come out, and the escapes in it still count --
        // they never reached the escape handling at the top of the loop.
        assert_eq!(utc_printf("no-time %(unterminated\n", &[]), "no-time %(unterminated\n");
        assert_eq!(utc_printf("%(%F)X", &["0"]), "%(%F)");
    }

    // `change_directory` moves the real process, so these must not run
    // beside anything else that reads or sets the cwd -- same shared-
    // mutex fix, and the same poisoned-lock recovery, as session.rs's
    // own ENV_LOCK.
    // Held by every test that moves the process's directory -- and by
    // every test that *depends* on it, which is the half that is easy
    // to forget. A spawn carries the shell's cwd, so a test that runs
    // an external command while a cwd-moving test is between its own
    // `cd` and its cleanup spawns into a directory that is about to
    // stop existing, and fails with a "No such file or directory" that
    // names the program rather than the directory.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // Where a cwd-moving test puts the process back. Not "wherever it
    // was when this test started": another test that does not take
    // CWD_LOCK may have left the process inside a temp directory it
    // then deleted, and restoring *that* fails. The crate root is
    // always there while the tests are running.
    fn restore_cwd() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::env::set_current_dir(root).expect("the crate root is a real directory");
    }

    // The bug this pins was found by driving the file browser through a
    // real pty: Ctrl-Y moved the shell correctly, and then `cd -` went
    // somewhere else entirely. `sync_real_state_in` reapplies the
    // session's env snapshot before every command, so an `OLDPWD`
    // written only to the real environment -- which is all a change made
    // *outside* a command can do -- lasted exactly until the next one.
    #[test]
    fn changing_directory_updates_the_session_snapshot_not_just_the_environment() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let root = std::env::temp_dir().join(format!("bish-cd-test-{}", std::process::id()));
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let root_real = std::fs::canonicalize(&root).unwrap();
        let inner_real = std::fs::canonicalize(&inner).unwrap();

        let mut shell = Shell::new();
        shell.change_directory(&root_real).expect("the temp root is a real directory");
        shell.sync_real_state_out();
        shell.change_directory(&inner_real).expect("the inner directory is real too");

        assert_eq!(shell.cwd, inner_real);
        // Asserted through the shell rather than through
        // `std::env::var`: the real environment is process-global, and
        // any other test running a shell concurrently reapplies its own
        // snapshot over it (CWD_LOCK only serialises the tests that
        // know to take it). What the bug was actually about is whether
        // *this session* still knows where it was, which is what
        // `lookup_var` and `env_snapshot` answer.
        assert_eq!(shell.lookup_var("PWD"), inner_real.to_string_lossy());
        assert_eq!(shell.lookup_var("OLDPWD"), root_real.to_string_lossy());

        // The part that was broken: after the session restores its own
        // remembered environment, `cd -` still has somewhere to go.
        shell.sync_real_state_in();
        assert_eq!(
            shell.env_snapshot.get("OLDPWD").map(String::as_str),
            Some(root_real.to_string_lossy().as_ref()),
            "OLDPWD must survive the snapshot being reapplied"
        );
        assert_eq!(shell.env_snapshot.get("PWD").map(String::as_str), Some(inner_real.to_string_lossy().as_ref()));
        assert_eq!(shell.lookup_var("OLDPWD"), root_real.to_string_lossy());

        restore_cwd();
        std::fs::remove_dir_all(&root).ok();
    }

    // Restricted mode refuses at the write path, so nothing that can
    // move the shell -- including the file browser -- can route around
    // it.
    #[test]
    fn a_restricted_shell_refuses_to_change_directory_at_all() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        restore_cwd();
        let before = std::env::current_dir().expect("a working directory");
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let err = shell.change_directory(std::path::Path::new("/")).unwrap_err();
        assert_eq!(err, RESTRICTED);
        assert_eq!(std::env::current_dir().unwrap(), before, "the real process must not have moved");
    }

    use super::*;

    // The property the whole cwd change is for: a path a script names
    // resolves against the *shell's* directory, and the process's is
    // not consulted. Indistinguishable in a single shell -- the two are
    // kept in step -- so this sets them apart deliberately, which is
    // the state two interleaved pipeline stages are permanently in.
    #[test]
    fn a_path_resolves_against_the_shells_cwd_not_the_processs() {
        let dir = std::env::temp_dir().join(format!("bish-cwd-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::fs::write(dir.join("elsewhere/target.txt"), b"found me").unwrap();

        let mut shell = Shell::new();
        // The shell is told it lives in `elsewhere`; the process is not,
        // and is never told.
        shell.cwd = dir.join("elsewhere");
        let process_cwd_before = std::env::current_dir().ok();

        assert_eq!(shell.resolve_path("target.txt"), dir.join("elsewhere/target.txt"));
        assert_eq!(shell.resolve_path("/absolute/stays"), std::path::Path::new("/absolute/stays"));

        let mut opened = shell.open_in("target.txt").expect("a relative name resolves against the shell's cwd");
        let mut text = String::new();
        std::io::Read::read_to_string(&mut opened, &mut text).unwrap();
        assert_eq!(text, "found me");

        assert_eq!(std::env::current_dir().ok(), process_cwd_before, "and the process was never moved to make that work");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn virtual_child_shares_jobs_and_promotion_but_not_vars() {
        let mut parent = Shell::new();
        parent.arrays.insert("x".to_string(), std::collections::BTreeMap::from([(0, "parent".to_string())]));

        let mut child = parent.new_virtual_child();

        // Independent snapshot: the child inherits a copy of the parent's
        // vars/cwd at creation time, but the two evolve independently
        // afterward.
        assert!(child.arrays.contains_key("x"), "child should inherit a snapshot of the parent's arrays");
        child.arrays.insert("y".to_string(), std::collections::BTreeMap::from([(0, "child".to_string())]));
        assert!(!parent.arrays.contains_key("y"), "parent must not see the child's later mutations");

        child.cwd = std::path::PathBuf::from("/child/only");
        assert_ne!(parent.cwd, child.cwd, "cwd must diverge independently per session");

        // Shared: a job pushed from the child is immediately visible from
        // the parent, and vice versa -- same underlying JobTable, not a
        // copy.
        let real_child = Command::new("true").spawn().expect("spawn true");
        child.push_job(vec![real_child], "true &".to_string());
        assert_eq!(parent.jobs.borrow().jobs.len(), 1, "parent must see the job the child backgrounded");
        assert_eq!(child.jobs.borrow().jobs.len(), 1);

        // Shared: promotion flipped from one session is visible from the
        // other -- it's a whole-terminal concept, not per-session.
        assert!(!parent.is_promoted());
        child.promoted.set(true);
        assert!(parent.is_promoted(), "promotion must be visible from every session sharing this root");

        // Reap the spawned process so the test doesn't leak it.
        parent.jobs.borrow_mut().jobs[0].wait();
    }

    // Records every (line, depth) DebugHook::on_statement was called with,
    // always returning Continue -- enough to prove the hook fires for
    // every top-level statement, at every nesting depth, including inside
    // a converted foreground subshell/command-substitution (which is a
    // genuinely separate Shell, only reachable here because new_
    // virtual_child shares debug_hook via Rc::clone).
    struct RecordingHook {
        calls: Vec<(usize, DebugDepth)>,
    }

    impl DebugHook for RecordingHook {
        fn on_statement(&mut self, line: usize, depth: DebugDepth, _shell: &mut Shell) -> DebugAction {
            self.calls.push((line, depth));
            DebugAction::Continue
        }
    }

    #[test]
    fn debug_hook_sees_every_statement_including_inside_a_converted_subshell() {
        let hook = Rc::new(RefCell::new(RecordingHook { calls: Vec::new() }));
        let mut shell = Shell::new();
        shell.debug_hook = Some(hook.clone());
        let _buf = capture_output(&mut shell);
        shell.run_source_here("echo top\n(echo inside_subshell)\nx=$(echo inside_cmd_sub)\n", "<test>");

        let calls = hook.borrow().calls.clone();
        // 5 calls: the 3 real top-level statements, *plus* one more for
        // each of the two constructs' own single statement running inside
        // its converted (in-process, but genuinely separate Shell) child.
        assert_eq!(calls.len(), 5, "{:?}", calls);
        assert_eq!(calls[0], (1, DebugDepth { subshell_depth: 0, call_depth: 0 }), "echo top");
        assert_eq!(calls[1], (2, DebugDepth { subshell_depth: 0, call_depth: 0 }), "the subshell statement itself");
        // Known, accepted limitation (see plan.md): a subshell/command-
        // substitution's raw captured text is re-lexed from a bare
        // String with no memory of its own starting file line, so its
        // own statement(s) report a line number relative to that capture
        // (always starting back at 1), not the real absolute file line.
        assert_eq!(calls[2], (1, DebugDepth { subshell_depth: 1, call_depth: 0 }), "echo inside_subshell (relative line)");
        assert_eq!(calls[3], (3, DebugDepth { subshell_depth: 0, call_depth: 0 }), "the x=$(...) assignment itself");
        assert_eq!(calls[4], (1, DebugDepth { subshell_depth: 1, call_depth: 0 }), "echo inside_cmd_sub (relative line)");
    }

    #[test]
    fn debug_hook_quit_unwinds_without_killing_the_process() {
        struct QuitAfterOne {
            n: u32,
        }
        impl DebugHook for QuitAfterOne {
            fn on_statement(&mut self, _line: usize, _depth: DebugDepth, _shell: &mut Shell) -> DebugAction {
                self.n += 1;
                if self.n >= 2 { DebugAction::Quit } else { DebugAction::Continue }
            }
        }
        let mut shell = Shell::new();
        shell.debug_hook = Some(Rc::new(RefCell::new(QuitAfterOne { n: 0 })));
        let buf = capture_output(&mut shell);
        shell.run_source_here("echo one\necho two\necho three\n", "<test>");
        assert_eq!(buf.borrow().as_str(), "one\n", "should stop after the first statement, never reaching the third");
    }

    // The `command` builtin has its own separate external-process spawn
    // path (distinct from run_single's), which used to always default to
    // Stdio::inherit() -- invisible to a converted foreground subshell/
    // command-substitution's own capture (run_in_child_shell's own
    // stdio_override), so a real command's real output would land
    // straight on the real terminal instead of being captured. Caught via
    // a real shell script (`mise`'s own bash activation shadows itself
    // with a `mise()` function that reaches the real binary via
    // `command "$__MISE_EXE" ...`, so every `$(mise ...)` from inside its
    // own hooks went straight through this path).
    #[test]
    fn command_builtin_honors_an_enclosing_command_substitution_capture() {
        // Spawns a real program, so it needs the process's directory to
        // stay put -- see CWD_LOCK.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x=$(command /bin/echo captured); echo "got:$x""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "got:captured\n");
    }

    // `exec CMD` inside a converted foreground subshell/command-
    // substitution used to call a literal execve, replacing the one real
    // process the whole interactive shell (every window/pane) shares --
    // now falls back to spawning CMD as a real, separate child instead,
    // same as a real forked subshell's own exec would actually replace.
    // Uses /bin/true (no output) rather than asserting on what it prints:
    // a *spawned external process's* own stdout goes straight to a real
    // fd, bypassing capture_output's OutputSink::Capture entirely (that
    // only intercepts this Shell's own builtin writes) -- exactly the
    // distinction exec_cmd_inside_a_command_substitution_is_captured
    // just below exists to cover instead, via the real capture mechanism
    // command substitution actually uses.
    #[test]
    fn exec_cmd_inside_a_subshell_does_not_kill_the_real_process() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"(exec /bin/true); echo "still alive: $?""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "still alive: 0\n");
    }

    #[test]
    fn exec_cmd_inside_a_command_substitution_is_captured() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x=$(exec /bin/echo captured); echo "got:$x""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "got:captured\n");
    }

    // Bare `exec > file` (no command word -- a persistent, no-fork
    // redirect of *this shell's own* fd 0/1/2, applied via a real dup2 on
    // the real process) used to leak past the subshell it ran in: since
    // it's no longer a real separate process, nothing undid the real fd
    // 1 repoint once the subshell finished, so *everything* printed
    // afterward -- even in the enclosing real shell -- silently kept
    // going into that file too. save_fd012/restore_fd012 (run_in_child_
    // shell) now save and restore the real fd 0/1/2 around the whole
    // call, the same way cwd/env/umask already were. Deliberately not a
    // Rust unit test here: verifying it needs a real fd 1 (OutputSink::
    // Real / an external process's inherited stdio, neither of which
    // capture_output's OutputSink::Capture touches), and manipulating
    // the real fd 1 mid-test would risk corrupting whichever *other*
    // test happens to be writing to its own stdout in parallel in this
    // same test binary. Confirmed manually instead: `(exec > file; echo
    // hi); echo after` at a real prompt -- "after" reaches the real
    // terminal, only "hi" lands in the file.

    fn strs(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn abbr_add_registers_a_multi_word_expansion_joined_by_single_spaces() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "gco", "git", "checkout"])), 0);
        assert_eq!(shell.abbrs, vec![Abbr::new("gco", "git checkout")]);
    }

    #[test]
    fn abbr_add_without_the_dash_a_flag_still_adds() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["ll", "ls", "-la"])), 0);
        assert_eq!(shell.abbrs, vec![Abbr::new("ll", "ls -la")]);
    }

    #[test]
    fn abbr_add_redefines_an_existing_name_in_place_rather_than_duplicating() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "gco", "git", "checkout"]));
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "gco", "git", "switch"]));
        assert_eq!(shell.abbrs, vec![Abbr::new("gco", "git switch")]);
    }

    #[test]
    fn abbr_erase_removes_a_known_name_and_reports_status_1_for_an_unknown_one() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "gco", "git", "checkout"]));
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-e", "gco"])), 0);
        assert!(shell.abbrs.is_empty());
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-e", "gco"])), 1);
    }

    #[test]
    fn abbr_query_is_zero_only_when_every_named_abbreviation_exists() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "gco", "git", "checkout"]));
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "gs", "git", "status"]));
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-q", "gco", "gs"])), 0);
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-q", "gco", "nope"])), 1);
    }

    #[test]
    fn abbr_list_and_show_report_status_0_without_mutating_the_table() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "gco", "git", "checkout"]));
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-l"])), 0);
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-s"])), 0);
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &[]), 0);
        assert_eq!(shell.abbrs, vec![Abbr::new("gco", "git checkout")]);
    }

    // The trailing-integer-run spelling for placeholder order is gone
    // -- order lives in the expansion now, as `$1`/`$2` -- so trailing
    // integers are plain expansion words again, with nothing to
    // disambiguate.
    #[test]
    fn abbr_add_keeps_trailing_integers_as_expansion_words() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["--add", "foo", "bar -x $1 -y $2", "2", "1"])), 0);
        assert_eq!(shell.abbrs, vec![Abbr::new("foo", "bar -x $1 -y $2 2 1")]);
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "e12", "echo", "1", "2"])), 0);
        assert_eq!(shell.abbrs[1], Abbr::new("e12", "echo 1 2"));
    }

    #[test]
    fn abbr_show_round_trips_an_expansion_verbatim() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "foo", "bar -x ${1:x} -y $2"]));
        let out = capture_output(&mut shell);
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-s"]));
        assert_eq!(out.borrow().as_str(), "abbr -a 'foo' 'bar -x ${1:x} -y $2'\n");
    }

    #[test]
    fn a_background_jobs_output_is_routed_to_the_grid_that_spawned_it() {
        // Regression: a `cmd &` wrote straight to the real terminal --
        // painting over whatever the pane was showing, then being wiped
        // by the next repaint, since the session's grid never saw a byte
        // of it -- or, for the pty-attached case, never appeared at all
        // because nothing drained the far end. Both now land in the grid
        // of the session that started the job.
        let mut shell = Shell::new();
        let screen = Rc::new(RefCell::new(crate::vt100::Screen::new(6, 40)));
        // In this order, matching repl.rs's own ensure_promoted:
        // promote_if_needed writes the alternate-screen switch through
        // the sink, and that switch belongs to the *real* terminal, not
        // to this grid -- with the grid installed first it would land in
        // the grid instead and leave it stuck on its own alternate
        // buffer, which is exactly the state the drain skips.
        shell.promote_if_needed();
        shell.set_sink_grid(screen.clone());
        assert!(!screen.borrow().using_alternate, "the session's grid is never the one promotion switches");
        shell.run_source_here("sh -c 'echo from-a-background-job' &\n", "<test>");
        // The child writes as soon as it can; drain until it shows up
        // rather than racing it (the real caller does this every idle
        // tick).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let row0 = loop {
            shell.drain_background_output();
            let text: String = (0..40).map(|c| screen.borrow().cell(0, c).ch).collect::<String>().trim_end().to_string();
            if !text.is_empty() || std::time::Instant::now() > deadline {
                break text;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(row0, "from-a-background-job");
    }

    #[test]
    fn a_redirected_background_job_still_records_the_grid_for_its_other_stream() {
        // Regression: a command with its own output redirect runs under a
        // `Builtin` sink for its duration, so reading `self.sink`
        // directly at push time found no grid and the stream that
        // *wasn't* redirected stayed invisible -- `cmd >file &` losing
        // every line of stderr. sink_grid looks through that sink to the
        // real one underneath.
        let mut shell = Shell::new();
        let screen = Rc::new(RefCell::new(crate::vt100::Screen::new(6, 40)));
        shell.set_sink_grid(screen.clone());
        assert!(shell.sink_grid().is_some());
        let file = Rc::new(RefCell::new(std::fs::File::create("/dev/null").unwrap()));
        shell.sink = OutputSink::Builtin {
            previous: Box::new(std::mem::replace(&mut shell.sink, OutputSink::Real)),
            stdout: SinkStream::File(file),
            stderr: SinkStream::OuterErr,
        };
        assert!(shell.sink_grid().is_some(), "a per-command redirect sink must not hide the session's own grid");
    }

    #[test]
    fn abbr_lang_scopes_an_abbreviation_and_the_same_name_can_mean_two_things() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "p", "echo bash"])), 0);
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["--lang=rust", "-a", "p", "println!(\"%s\")"])), 0);
        assert_eq!(shell.abbrs.len(), 2, "same name, different language -- two entries, not a redefinition");
        // Redefining still replaces in place, keyed on both.
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["--lang=rust", "-a", "p", "dbg!(%s)"])), 0);
        assert_eq!(shell.abbrs.len(), 2);
        assert_eq!(shell.abbrs[1].expansion, "dbg!(%s)");
        assert_eq!(shell.abbrs[0].lang, "bash");
    }

    #[test]
    fn abbr_erase_without_a_lang_erases_the_name_everywhere() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "p", "one"]));
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["--lang=rust", "-a", "p", "two"]));
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "q", "three"]));
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-e", "p"])), 0);
        assert_eq!(shell.abbrs.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["q"]);
    }

    #[test]
    fn abbr_erase_with_a_lang_erases_only_that_one() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "p", "one"]));
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["--lang=rust", "-a", "p", "two"]));
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["--lang=rust", "-e", "p"])), 0);
        assert_eq!(shell.abbrs.len(), 1);
        assert_eq!(shell.abbrs[0].lang, "bash");
        // ...and erasing a language it isn't defined for is a miss.
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["--lang=go", "-e", "p"])), 1);
    }

    #[test]
    fn abbr_query_can_ask_about_one_language() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["--lang=rust", "-a", "p", "two"]));
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-q", "p"])), 0, "any language counts without --lang");
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["--lang=rust", "-q", "p"])), 0);
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["--lang=bash", "-q", "p"])), 1);
    }

    #[test]
    fn abbr_show_round_trips_a_language() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["--lang=!(bash)", "-a", "p", "note %s"]));
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "plain", "echo hi"]));
        let out = capture_output(&mut shell);
        crate::builtins::bish::run_abbr(&mut shell, &strs(&["-s"]));
        assert_eq!(
            out.borrow().as_str(),
            // The default language is left off, so an abbreviation that
            // never mentioned one still shows the way it always did.
            "abbr -a --lang='!(bash)' 'p' 'note %s'\nabbr -a 'plain' 'echo hi'\n"
        );
    }

    #[test]
    fn abbr_add_with_no_expansion_is_a_usage_error() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_abbr(&mut shell, &strs(&["-a", "gco"])), 2);
        assert!(shell.abbrs.is_empty());
    }

    #[test]
    fn new_virtual_child_inherits_a_snapshot_of_the_parents_abbrs() {
        let mut parent = Shell::new();
        crate::builtins::bish::run_abbr(&mut parent, &strs(&["-a", "gco", "git", "checkout"]));
        let mut child = parent.new_virtual_child();
        assert_eq!(child.abbrs, parent.abbrs);
        crate::builtins::bish::run_abbr(&mut child, &strs(&["-a", "gs", "git", "status"]));
        assert!(!parent.abbrs.iter().any(|a| a.name == "gs"), "parent must not see the child's later abbr additions");
    }

    #[test]
    fn shopt_reports_each_names_own_default_before_any_override() {
        let shell = Shell::new();
        assert!(!shell.shopt_is_on("nullglob"), "nullglob defaults off");
        assert!(shell.shopt_is_on("cmdhist"), "cmdhist defaults on");
        assert!(shell.shopt_is_on("extglob"), "extglob is always on regardless of its own listed default");
    }

    #[test]
    fn shopt_s_and_u_override_a_names_default_either_direction() {
        let mut shell = Shell::new();
        assert!(!shell.shopt_is_on("nullglob"));
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &strs(&["-s", "nullglob"])), 0);
        assert!(shell.shopt_is_on("nullglob"));

        assert!(shell.shopt_is_on("cmdhist"));
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &strs(&["-u", "cmdhist"])), 0);
        assert!(!shell.shopt_is_on("cmdhist"));
    }

    #[test]
    fn shopt_q_reports_status_from_every_names_effective_state() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &strs(&["-q", "cmdhist", "extglob"])), 0);
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &strs(&["-q", "cmdhist", "nullglob"])), 1);
    }

    #[test]
    fn shopt_rejects_an_unknown_option_name() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &strs(&["bogus_option"])), 1);
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &strs(&["-s", "bogus_option"])), 1);
        assert!(shell.shopt_options.is_empty(), "a rejected name must not be recorded");
    }

    #[test]
    fn bare_shopt_and_s_and_u_alone_enumerate_every_known_option() {
        let mut shell = Shell::new();
        // Bare `shopt` (no -s/-u, no names): every known option is a
        // valid target, so this must not error and must not mutate
        // anything -- this is the bug this whole patch exists to fix
        // ("shopt without arguments does nothing").
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &[]), 0);
        assert!(shell.shopt_options.is_empty());

        // `shopt -s`/`shopt -u` alone list, not toggle -- no names means
        // nothing to turn on/off, unlike `shopt -s NAME`.
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &strs(&["-s"])), 0);
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &strs(&["-u"])), 0);
        assert!(shell.shopt_options.is_empty(), "-s/-u alone must only list, never mutate");
    }

    // A small local registry, so these exercise run_bishopt's own logic
    // for every value kind without depending on which production
    // options happen to exist.
    fn test_bishopts() -> Vec<(&'static str, BishOptDefault)> {
        vec![
            ("verbose", BishOptDefault::Bool(false)),
            ("width", BishOptDefault::Int(7, 0..=100)),
            ("greeting", BishOptDefault::Str("hi")),
            ("accent", BishOptDefault::Color("red")),
        ]
    }

    #[test]
    fn bishopt_lists_only_names_with_no_args() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &[], &test_bishopts()), 0);
    }

    #[test]
    fn bishopt_get_on_a_bool_reports_its_value_via_exit_status_either_way() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["verbose"]), &test_bishopts()), 1, "unset bool defaults to off");
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "verbose"]), &test_bishopts());
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["verbose"]), &test_bishopts()), 0);
    }

    #[test]
    fn bishopt_quiet_flag_behaves_like_the_bare_get_but_without_printing() {
        let mut shell = Shell::new();
        // -q/--quiet only changes whether get *prints* -- the exit status
        // (what shopt -q itself is for) is identical to the bare get's.
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["-q", "verbose"]), &test_bishopts()), 1);
        assert_eq!(
            crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--quiet", "greeting"]), &test_bishopts()),
            0,
            "a Str's mere existence is enough under -q"
        );
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "verbose"]), &test_bishopts());
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["-q", "verbose"]), &test_bishopts()), 0);
    }

    #[test]
    fn bishopt_set_accepts_on_and_off_as_an_alternative_to_unset_for_a_bool() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "verbose", "on"]), &test_bishopts()), 0);
        assert_eq!(shell.bishopt_value(&test_bishopts(), "verbose"), Some(BishOptValue::Bool(true)));
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "verbose", "off"]), &test_bishopts()), 0);
        assert_eq!(shell.bishopt_value(&test_bishopts(), "verbose"), Some(BishOptValue::Bool(false)));
    }

    #[test]
    fn bishopt_get_on_a_str_prints_its_value_and_exits_0() {
        let mut shell = Shell::new();
        assert_eq!(shell.bishopt_value(&test_bishopts(), "greeting"), Some(BishOptValue::Str("hi".to_string())));
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["greeting"]), &test_bishopts()), 0);
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "greeting", "hey"]), &test_bishopts());
        assert_eq!(shell.bishopt_value(&test_bishopts(), "greeting"), Some(BishOptValue::Str("hey".to_string())));
    }

    #[test]
    fn bishopt_set_rejects_a_value_on_a_bool_and_a_missing_value_on_a_str() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "verbose", "true"]), &test_bishopts()), 2);
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "greeting"]), &test_bishopts()), 2);
        assert_eq!(shell.bishopts, std::collections::HashMap::new(), "a rejected set must not be recorded");
    }

    #[test]
    fn bishopt_unset_reverts_to_each_types_own_default() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "verbose"]), &test_bishopts());
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "greeting", "hey"]), &test_bishopts());
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "blue"]), &test_bishopts());
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--unset", "verbose"]), &test_bishopts()), 0);
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--unset", "greeting"]), &test_bishopts()), 0);
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--unset", "accent"]), &test_bishopts()), 0);
        assert_eq!(shell.bishopt_value(&test_bishopts(), "verbose"), Some(BishOptValue::Bool(false)));
        assert_eq!(shell.bishopt_value(&test_bishopts(), "greeting"), Some(BishOptValue::Str("hi".to_string())));
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))]))
        );
    }

    #[test]
    fn bishopt_get_on_a_color_prints_the_original_text_not_a_re_serialization() {
        let mut shell = Shell::new();
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))]))
        );

        let buf = Rc::new(RefCell::new(String::new()));
        shell.set_sink_capture(buf.clone());
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["accent"]), &test_bishopts()), 0);
        assert_eq!(buf.borrow().as_str(), "red\n", "must echo back the registered default's own text, not \"#ff0000\"");

        buf.borrow_mut().clear();
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "cornflowerblue"]), &test_bishopts());
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["accent"]), &test_bishopts()), 0);
        assert_eq!(buf.borrow().as_str(), "cornflowerblue\n", "must echo back what --set was actually given, not \"#6495ed\"");

        assert_eq!(
            crate::builtins::bish::run_bishopt(&mut shell, &strs(&["-q", "accent"]), &test_bishopts()),
            0,
            "no boolean meaning, but must not error"
        );
    }

    #[test]
    fn bishopt_set_accepts_any_valid_css_color_syntax_including_color_mix() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "#00ff00"]), &test_bishopts()), 0);
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("#00ff00".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(0, 255, 0, 255))]))
        );

        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "rgb(0 0 255)"]), &test_bishopts()), 0);
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("rgb(0 0 255)".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(0, 0, 255, 255))]))
        );

        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "color-mix(in srgb, red, blue)"]), &test_bishopts()), 0);
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color(
                "color-mix(in srgb, red, blue)".to_string(),
                vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(128, 0, 128, 255))]
            ))
        );
    }

    #[test]
    fn bishopt_set_rejects_an_invalid_color_and_does_not_mutate() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "not-a-color"]), &test_bishopts()), 2);
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))])),
            "a rejected set must not overwrite the default"
        );
    }

    #[test]
    fn bishopt_rejects_an_unregistered_name_everywhere() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["nope"]), &test_bishopts()), 1);
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "nope"]), &test_bishopts()), 1);
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--unset", "nope"]), &test_bishopts()), 1);
    }

    #[test]
    fn new_virtual_child_inherits_a_snapshot_of_the_parents_bishopts() {
        let mut parent = Shell::new();
        crate::builtins::bish::run_bishopt(&mut parent, &strs(&["--set", "verbose"]), &test_bishopts());
        let mut child = parent.new_virtual_child();
        assert_eq!(child.bishopts, parent.bishopts);
        crate::builtins::bish::run_bishopt(&mut child, &strs(&["--set", "greeting", "yo"]), &test_bishopts());
        assert!(!parent.bishopts.contains_key("greeting"), "parent must not see the child's later bishopt changes");
    }

    // `::bish theme begin`/`end`'s own tests -- every `bishopt --set`
    // inside a declaration goes through &test_bishopts(), exactly like every
    // other run_bishopt test above; "theme" itself is a real KNOWN_BISHOPTS
    // entry (bishopt_value/store_bishopt's own fallback logic doesn't
    // depend on which registry a *different* option came from), so these
    // set/read it directly rather than needing their own parallel entry
    // in TEST_BISHOPTS.
    #[test]
    fn bish_theme_declares_a_named_theme_without_applying_its_opts_live() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "begin"])).status(), 0);
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "dark"]), KNOWN_BISHOPTS), 0);
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "blue"]), &test_bishopts()), 0);
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "end"])).status(), 0);

        // Neither "theme" nor "accent" was actually applied live -- both
        // still read as whatever they were before the declaration.
        assert_eq!(shell.bishopt_value(KNOWN_BISHOPTS, "theme"), Some(BishOptValue::Str(String::new())));
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))]))
        );
        // But the theme itself was registered, "theme" entry excluded.
        let dark = shell.themes.get("dark").expect("theme must be registered");
        assert_eq!(
            dark.opts.get("accent"),
            Some(&BishOptValue::Color("blue".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(0, 0, 255, 255))]))
        );
        assert!(!dark.opts.contains_key("theme"), "a theme's own opts must not include a self-referential \"theme\" entry");
    }

    #[test]
    fn bish_theme_end_without_ever_naming_it_discards_the_whole_declaration() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "begin"])).status();
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "blue"]), &test_bishopts());
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "end"])).status(), 0);
        assert!(shell.themes.is_empty(), "no name was ever declared, so nothing should be registered");
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))])),
            "still not applied live either"
        );
    }

    #[test]
    fn activating_a_declared_theme_makes_its_opts_the_new_fallback_default() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "begin"])).status();
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "dark"]), KNOWN_BISHOPTS);
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "blue"]), &test_bishopts());
        crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "end"])).status();

        // Registering "dark" doesn't activate it by itself.
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))]))
        );

        // Activating it (an ordinary set, outside any declaration) makes
        // its opts the new fallback wherever nothing else was set.
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "dark"]), KNOWN_BISHOPTS), 0);
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("blue".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(0, 0, 255, 255))]))
        );

        // An explicit override still wins over the active theme.
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "green"]), &test_bishopts());
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("green".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(0, 128, 0, 255))]))
        );
    }

    #[test]
    fn bish_theme_begin_refuses_to_nest() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "begin"])).status(), 0);
        assert_eq!(
            crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "begin"])).status(),
            1,
            "a second begin while one is already in progress must be refused"
        );
        // The original declaration must still be intact -- a set right
        // after the refused nested begin still lands in it.
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "t"]), KNOWN_BISHOPTS);
        crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "end"])).status();
        assert!(shell.themes.contains_key("t"));
    }

    #[test]
    fn bish_theme_end_without_a_begin_is_an_error() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "end"])).status(), 1);
    }

    #[test]
    fn bish_unset_still_applies_live_even_mid_declaration() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "blue"]), &test_bishopts());
        crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "begin"])).status();
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--unset", "accent"]), &test_bishopts()), 0);
        crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "end"])).status();
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))])),
            "--unset must not have been diverted into the pending theme"
        );
    }

    #[test]
    fn bish_and_bish_theme_reject_unknown_subcommands() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["nonsense"])).status(), 2);
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&[])).status(), 2);
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "nonsense"])).status(), 2);
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme"])).status(), 2);
    }

    #[test]
    fn double_colon_bish_dispatches_as_a_real_shell_command() {
        let mut shell = Shell::new();
        shell.run_source_here("::bish theme begin; bishopt --set theme fromshell; ::bish theme end", "<test>");
        assert!(shell.themes.contains_key("fromshell"), "::bish must parse and dispatch as an ordinary command word");
    }

    // Every name bish's own highlighting produces has to be spellable
    // at `::bish hl`. Not an exhaustive list of what that command
    // accepts -- the namespace is open -- but these are the ones that
    // do something today, so a typo'd or duplicated entry here is a
    // colour nobody can set.
    #[test]
    fn every_highlight_kind_has_a_distinct_settable_name() {
        let mut names: Vec<&str> = crate::bishedit::highlight::HL_NAMES.iter().map(|(_, n)| *n).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "two highlight kinds share a name");
        let mut shell = Shell::new();
        for name in names {
            assert!(!name.is_empty() && !name.starts_with('-'), "{name} is not spellable as a `::bish hl` name");
            assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", name, "#123456"])), 0, "{name} must be settable");
            assert_eq!(shell.hl_color(name), Some(vt100::Color::Rgb(0x12, 0x34, 0x56)));
        }
    }

    #[test]
    fn an_unset_highlight_colour_is_none_so_the_renderer_keeps_its_own_default() {
        let mut shell = Shell::new();
        // No registry and no defaults: `::bish hl`'s namespace is open,
        // so the only thing that can be said about a name nobody has
        // mentioned is that it is unset -- and the caller then uses
        // `highlight::default_style`, which is what a fresh install
        // rendered with before any of this existed.
        assert_eq!(shell.hl_color("string"), None);
        assert_eq!(shell.hl_color("something_no_one_has_named"), None);
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "string", "#123456"])), 0);
        assert_eq!(shell.hl_color("string"), Some(vt100::Color::Rgb(0x12, 0x34, 0x56)));
        // An open namespace takes a name nothing produces yet, which is
        // what lets a server's semantic token types be coloured before
        // bish knows about them.
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "lsp_type_parameter", "#abcdef"])), 0);
        assert_eq!(shell.hl_color("lsp_type_parameter"), Some(vt100::Color::Rgb(0xab, 0xcd, 0xef)));
        // Unsetting takes it back to "nothing said".
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--unset", "string"])), 0);
        assert_eq!(shell.hl_color("string"), None);
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--unset", "string"])), 1, "unsetting what is not set says so");
    }

    // The point of `::bish hl` being its own command but not its own
    // *concept*: a theme is one thing you switch to, and it carries the
    // palette along with the options.
    #[test]
    fn a_theme_captures_highlight_colours_alongside_bishopts() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bish_theme(&mut shell, &strs(&["begin"])), 0);
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "midnight"]), KNOWN_BISHOPTS);
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "ui_col_directory", "#111111"]), KNOWN_BISHOPTS);
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "string", "#222222"])), 0);
        assert_eq!(crate::builtins::bish::run_bish_theme(&mut shell, &strs(&["end"])), 0);

        // Declaring is not switching, so nothing has changed yet.
        assert_eq!(shell.hl_color("string"), None);
        // Switching brings both halves.
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "midnight"]), KNOWN_BISHOPTS);
        assert_eq!(shell.bishopt_color("ui_col_directory"), Some(vt100::Color::Rgb(0x11, 0x11, 0x11)));
        assert_eq!(shell.hl_color("string"), Some(vt100::Color::Rgb(0x22, 0x22, 0x22)));

        // Something set directly still wins over the theme, the same
        // precedence a bishopt has.
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "string", "#333333"])), 0);
        assert_eq!(shell.hl_color("string"), Some(vt100::Color::Rgb(0x33, 0x33, 0x33)));
        // ...and the listing shows the theme's entries too, so `::bish
        // hl` with no arguments answers "what is in force", not "what
        // did I type".
        let listed = shell.hl_colors();
        assert!(listed.iter().any(|(n, v)| n == "string" && v == "#333333"), "{listed:?}");
    }

    #[test]
    fn a_bad_highlight_colour_is_refused_and_nothing_is_stored() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "string", "not-a-colour"])), 2);
        assert_eq!(shell.hl_color("string"), None);
    }

    #[test]
    fn bishopt_color_resolves_the_default_then_an_override_then_none_for_unknown() {
        let mut shell = Shell::new();
        // The chrome colours stayed bishopts, and still have defaults:
        // `-bish-blue` is ANSI slot 4, terminal-resolved, not a fixed
        // RGB -- so a fresh install renders as it always did.
        assert_eq!(shell.bishopt_color("ui_col_directory"), Some(vt100::Color::Indexed(4)));
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "ui_col_directory", "#123456"]), KNOWN_BISHOPTS);
        assert_eq!(shell.bishopt_color("ui_col_directory"), Some(vt100::Color::Rgb(0x12, 0x34, 0x56)));
        assert_eq!(shell.bishopt_color("not_a_real_option"), None);
    }

    #[test]
    fn bishopt_color_accepts_a_vendor_ansi_reference_and_reads_it_back_verbatim() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "-bish-bright-red"]), &test_bishopts()), 0);
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("-bish-bright-red".to_string(), vec![crate::csscolor::TermColor::Ansi(9)]))
        );

        let buf = Rc::new(RefCell::new(String::new()));
        shell.set_sink_capture(buf.clone());
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["accent"]), &test_bishopts());
        assert_eq!(buf.borrow().as_str(), "-bish-bright-red\n");
    }

    #[test]
    fn bishopt_set_rejects_a_vendor_color_used_inside_color_mix() {
        let mut shell = Shell::new();
        assert_eq!(
            crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "color-mix(in srgb, -bish-red, blue)"]), &test_bishopts()),
            2
        );
    }

    #[test]
    fn bishopt_set_accepts_a_font_family_style_fallback_list() {
        let mut shell = Shell::new();
        assert_eq!(
            crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "#ff0000, -bish-ansi(1), -bish-red"]), &test_bishopts()),
            0
        );
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color(
                "#ff0000, -bish-ansi(1), -bish-red".to_string(),
                vec![
                    crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255)),
                    crate::csscolor::TermColor::Ansi(1),
                    crate::csscolor::TermColor::Ansi(1),
                ]
            ))
        );

        // Echoed back verbatim on get, same as any other Color -- the
        // whole list as typed, not whichever candidate happened to win.
        let buf = Rc::new(RefCell::new(String::new()));
        shell.set_sink_capture(buf.clone());
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["accent"]), &test_bishopts());
        assert_eq!(buf.borrow().as_str(), "#ff0000, -bish-ansi(1), -bish-red\n");
    }

    #[test]
    fn bishopt_set_rejects_a_fallback_list_with_any_invalid_candidate() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "red, not-a-color, blue"]), &test_bishopts()), 2);
    }

    #[test]
    fn bishopt_color_for_picks_the_best_candidate_the_terminals_support_allows() {
        use crate::csscolor::ColorSupport;
        let mut shell = Shell::new();
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "ui_col_directory", "#ff0000, -bish-ansi(200), -bish-red"]), KNOWN_BISHOPTS);
        assert_eq!(shell.bishopt_color_for("ui_col_directory", ColorSupport::Truecolor), Some(vt100::Color::Rgb(255, 0, 0)));
        assert_eq!(shell.bishopt_color_for("ui_col_directory", ColorSupport::Ansi256), Some(vt100::Color::Indexed(200)));
        assert_eq!(shell.bishopt_color_for("ui_col_directory", ColorSupport::Ansi16), Some(vt100::Color::Indexed(1)));
        // Nothing in this particular list suits ColorSupport::None -- the
        // least-demanding candidate (last in the list) is still used.
        assert_eq!(shell.bishopt_color_for("ui_col_directory", ColorSupport::None), Some(vt100::Color::Indexed(1)));
        // And the same tiering through `::bish hl`, which shares the
        // candidate-picking with bishopt rather than re-deriving it.
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "string", "#ff0000, -bish-ansi(200), -bish-red"])), 0);
        assert_eq!(shell.hl_color_for("string", ColorSupport::Truecolor), Some(vt100::Color::Rgb(255, 0, 0)));
        assert_eq!(shell.hl_color_for("string", ColorSupport::Ansi16), Some(vt100::Color::Indexed(1)));
    }

    fn capture_output(shell: &mut Shell) -> Rc<RefCell<String>> {
        let buf = Rc::new(RefCell::new(String::new()));
        shell.set_sink_capture(buf.clone());
        buf
    }

    // Every exit-status/output expectation in this block was checked
    // against real bash (`bash -c 'compgen ...'`) before being asserted
    // here -- see run_compgen's own doc comment.

    #[test]
    fn compgen_wordlist_preserves_input_order_and_filters_by_prefix() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-W", "banana apple cherry apple"])), 0);
        assert_eq!(buf.borrow().as_str(), "banana\napple\ncherry\napple\n", "no sort, no dedup -- matches real bash");

        buf.borrow_mut().clear();
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-W", "banana apple cherry", "--", "a"])), 0);
        assert_eq!(buf.borrow().as_str(), "apple\n");
    }

    #[test]
    fn compgen_exit_status_is_1_only_when_a_source_was_given_and_yielded_nothing() {
        let mut shell = Shell::new();
        let _buf = capture_output(&mut shell);
        // No source at all -- always exits 0, even with a trailing word and
        // even though nothing gets printed.
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &[]), 0);
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["zzz"])), 0);
        // A real source that produced zero matches -- exits 1.
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-W", "", "--", "x"])), 1);
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-W", "abc def", "--", "zzz"])), 1);
        // A real source with a match -- exits 0.
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-W", "abc def", "--", "a"])), 0);
    }

    #[test]
    fn compgen_x_filter_excludes_by_default_and_keeps_only_matches_when_negated() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-W", "apple banana avocado", "-X", "a*"]));
        assert_eq!(buf.borrow().as_str(), "banana\n");

        buf.borrow_mut().clear();
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-W", "apple banana avocado", "-X", "!a*"]));
        assert_eq!(buf.borrow().as_str(), "apple\navocado\n");
    }

    #[test]
    fn compgen_prefix_and_suffix_wrap_every_candidate() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-P", "<", "-S", ">", "-W", "a b"]));
        assert_eq!(buf.borrow().as_str(), "<a>\n<b>\n");
    }

    #[test]
    fn compgen_keyword_action_lists_every_reserved_word() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "keyword"]));
        let owned: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        assert!(owned.iter().any(|n| n == "if"), "{owned:?}");
        assert!(owned.iter().any(|n| n == "done"), "{owned:?}");
        assert!(owned.iter().any(|n| n == "[["), "{owned:?}");
        // `!` and `time` were left out on the grounds that bish's
        // grammar did not reserve them. It does -- `! false` and
        // `time f` both parse -- and `type -t` has to answer "keyword"
        // for them, which it does off this same list.
        assert!(owned.iter().any(|n| n == "!"), "{owned:?}");
        assert!(owned.iter().any(|n| n == "time"), "{owned:?}");
    }

    #[test]
    fn compgen_signal_action_includes_exit_pseudo_signal_and_sig_prefixed_names() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "signal"]));
        let names: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        assert_eq!(names.first().map(String::as_str), Some("EXIT"));
        assert!(names.iter().any(|n| n == "SIGTERM"), "{names:?}");
        assert!(names.iter().any(|n| n == "SIGINT"), "{names:?}");
        // The bare (non-"SIG"-prefixed) form SIGNAL_NAMES itself stores
        // must not leak through here.
        assert!(!names.iter().any(|n| n == "TERM"), "{names:?}");
    }

    #[test]
    fn compgen_builtin_and_b_flag_agree_and_match_known_builtins() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "builtin"]));
        let via_a: Vec<String> = buf.borrow().lines().map(str::to_string).collect();

        buf.borrow_mut().clear();
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-b"]));
        let via_flag: Vec<String> = buf.borrow().lines().map(str::to_string).collect();

        assert_eq!(via_a, via_flag);
        assert!(via_a.iter().any(|n| n == "cd"), "{via_a:?}");
        assert!(via_a.iter().any(|n| n == "compgen"), "{via_a:?}");
    }

    #[test]
    fn compgen_shorthand_flags_combine_into_one_token() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-ab"]));
        let names: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        assert!(names.iter().any(|n| n == "cd"), "builtin action missing: {names:?}");
    }

    #[test]
    fn compgen_setopt_and_shopt_actions_mirror_their_own_registries() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "setopt"]));
        assert_eq!(buf.borrow().lines().collect::<Vec<_>>(), SET_O_OPTIONS.to_vec());

        buf.borrow_mut().clear();
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "shopt"]));
        let names: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        let expected: Vec<String> = KNOWN_SHOPT_OPTIONS.iter().map(|(n, _)| n.to_string()).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn compgen_rejects_an_unknown_action_name_and_an_unknown_option() {
        let mut shell = Shell::new();
        let _buf = capture_output(&mut shell);
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "bogus"])), 2);
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-Z"])), 2);
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-o", "bogus"])), 2);
        assert_eq!(
            crate::builtins::completion::run_compgen(&mut shell, &strs(&["-o", "nosort", "-W", "a"])),
            0,
            "a recognized -o name must not error"
        );
    }

    #[test]
    fn compgen_f_option_with_a_nonexistent_function_errors_and_exits_1() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-F", "not_a_real_function", "--", "a"])), 1);
        // Capture is a combined stdout+stderr sink (see OutputSink::Capture's
        // own doc comment), so the error message lands right here too.
        assert_eq!(buf.borrow().as_str(), "bish: compgen: not_a_real_function: function not found\n");
    }

    #[test]
    fn compgen_v_option_stores_into_an_indexed_array_instead_of_printing() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-V", "myarr", "-W", "a b c", "--", "a"])), 0);
        assert_eq!(buf.borrow().as_str(), "", "must not print when -V is given");
        assert_eq!(shell.arrays.get("myarr").map(|m| m.values().cloned().collect::<Vec<_>>()), Some(vec!["a".to_string()]));

        assert_eq!(
            crate::builtins::completion::run_compgen(&mut shell, &strs(&["-V", "myarr", "-W", "a b c", "--", "zzz"])),
            1,
            "same had-a-source-and-got-nothing rule applies under -V"
        );
        assert_eq!(shell.arrays.get("myarr").map(|m| m.len()), Some(0));
    }

    #[test]
    fn compgen_disabled_is_always_empty_and_enabled_matches_known_builtins() {
        let mut shell = Shell::new();
        // Same "a source was given and yielded nothing" rule as any other
        // empty action -- `disabled` is a real, meaningfully-empty source,
        // not "no source at all", so this still exits 1.
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "disabled"])), 1);
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "enabled"]));
        let names: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        assert_eq!(names.len(), KNOWN_BUILTINS.len());
    }

    #[test]
    fn compgen_arrayvar_action_lists_both_indexed_and_associative_array_names() {
        let mut shell = Shell::new();
        shell.arrays.insert("idxarr".to_string(), std::collections::BTreeMap::new());
        shell.assoc_arrays.insert("assocarr".to_string(), OrderedMap::default());
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "arrayvar"]));
        let names: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        assert!(names.iter().any(|n| n == "idxarr"), "{names:?}");
        assert!(names.iter().any(|n| n == "assocarr"), "{names:?}");
    }

    #[test]
    fn compgen_variable_action_sees_both_a_global_and_a_local_scope_var() {
        let mut shell = Shell::new();
        // Set through the shell rather than with `std::env::set_var`:
        // since variables moved off the process environment (see the
        // `globals` field), a name something else puts in the real
        // environment behind the shell's back is deliberately not one
        // of its variables -- that cross-talk is what the move removed.
        shell.run_source_here("export BISH_COMPGEN_TEST_VAR=1", "<test>");
        shell.var_scopes.push(HashMap::from([("local_only_var".to_string(), Some("x".to_string()))]));
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "variable"]));
        let names: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        unsafe { std::env::remove_var("BISH_COMPGEN_TEST_VAR") };
        assert!(names.iter().any(|n| n == "BISH_COMPGEN_TEST_VAR"), "{names:?}");
        assert!(names.iter().any(|n| n == "local_only_var"), "{names:?}");
    }

    #[test]
    fn compgen_alias_and_function_actions() {
        let mut shell = Shell::new();
        shell.aliases.push(("myalias".to_string(), "ls -la".to_string()));
        shell.run_source_here("myfunc() { :; }", "<test>");
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "alias"]));
        assert_eq!(buf.borrow().as_str(), "myalias\n");

        buf.borrow_mut().clear();
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-A", "function"]));
        assert_eq!(buf.borrow().as_str(), "myfunc\n");
    }

    #[test]
    fn compgen_g_globpat_expands_against_a_real_directory() {
        let dir = std::env::temp_dir().join(format!("bish-compgen-glob-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), b"").unwrap();
        std::fs::write(dir.join("b.txt"), b"").unwrap();

        // An absolute pattern (rather than relying on shell.cwd) --
        // glob::expand resolves a bare pattern against the real process
        // cwd, not shell.cwd (same as ordinary command-word globbing
        // elsewhere in this file), so an absolute pattern is what actually
        // exercises this without a racy process-wide chdir under parallel
        // test execution.
        let pattern = format!("{}/*.rs", dir.display());
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        let status = crate::builtins::completion::run_compgen(&mut shell, &strs(&["-G", &pattern]));

        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(status, 0);
        assert_eq!(buf.borrow().as_str(), format!("{}/a.rs\n", dir.display()));
    }

    #[test]
    fn compgen_directory_and_file_actions_split_on_the_final_path_segment_and_do_not_hide_dotfiles() {
        let dir = std::env::temp_dir().join(format!("bish-compgen-path-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("inner.txt"), b"").unwrap();
        std::fs::write(dir.join(".hidden"), b"").unwrap();

        let mut shell = Shell::new();
        shell.cwd = dir.clone();
        let buf = capture_output(&mut shell);
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-f", "--", "sub/in"]));
        let split_result = buf.borrow().clone();

        buf.borrow_mut().clear();
        crate::builtins::completion::run_compgen(&mut shell, &strs(&["-f"]));
        let bare_result = buf.borrow().clone();

        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(split_result, "sub/inner.txt\n", "the directory part must stay a literal prefix of the result");
        assert!(bare_result.lines().any(|l| l == ".hidden"), "real bash's own -f does not hide dotfiles: {bare_result:?}");
    }

    // Neither -F's nor -C's own subprocess round-trip is unit-tested here
    // (only -F's up-front "does this function even exist" pre-check,
    // above, which never spawns anything): compgen::run_external
    // re-invokes std::env::current_exe(), and under `cargo test` that
    // resolves to the *test harness* binary, not a real bish -- its own
    // arg parsing has nothing to do with `-c script` and just errors.
    // run_command_substitution/run_proc_sub_in/run_proc_sub_out (the
    // pre-existing features this reuses the exact same self-reexec
    // pattern from) have the same gap for the same reason, hence no unit
    // tests for `$(...)` either -- verified instead via a live pty smoke
    // test against the actually-compiled `bish` binary:
    // `compgen -C "echo hi" -- curword` printed "hi compgen curword " (the
    // trailing space from the appended empty previous-word arg), and
    // `compgen -F myfunc -- xyz` (myfunc: `COMPREPLY=(foo bar "$2")`)
    // printed "foo\nbar\nxyz", both matching real bash exactly.

    // Regression tests for a real, pre-existing bug this session's own
    // compgen/complete work surfaced: lexer.rs's `keyword()` has no notion
    // of command position, so a bare word exactly matching a reserved word
    // (if/then/.../function/[[/]]) always became its keyword token
    // regardless of where it appeared -- `echo function` failed to parse
    // at all ("expected function name, got None"), since the leftover
    // Tok::KwFunction got reinterpreted as starting a *new* function
    // definition once parse_simple_command's own word-collecting loop
    // stopped at it. Fixed via keyword_text (lexer.rs) reversing that
    // mapping wherever the parser is already past the point where a bare
    // word could legitimately start a new command.
    #[test]
    fn a_keyword_shaped_word_in_argument_position_is_an_ordinary_argument() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here("echo function", "<test>");
        assert_eq!(buf.borrow().as_str(), "function\n");
        assert_eq!(shell.last_status, 0);

        buf.borrow_mut().clear();
        shell.run_source_here("echo if while case", "<test>");
        assert_eq!(buf.borrow().as_str(), "if while case\n");
    }

    #[test]
    fn a_for_loops_own_wordlist_accepts_keyword_shaped_items_including_do_and_done() {
        // The plan's own worked example against real bash: even "do"/
        // "done" are literal wordlist items here, since the wordlist only
        // ends at the `;` right before the *real* `do` that opens the
        // loop body.
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"for x in if while do done; do echo "[$x]"; done"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "[if]\n[while]\n[do]\n[done]\n");
    }

    #[test]
    fn a_case_pattern_and_subject_can_be_keyword_shaped_and_still_match() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"case "if" in if) echo matched;; *) echo nomatch;; esac"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "matched\n");
    }

    #[test]
    fn a_double_bracket_test_operand_can_be_keyword_shaped() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x=function; [[ $x == function ]] && echo yes"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "yes\n");
    }

    #[test]
    fn an_assignment_prefix_suppresses_reserved_word_status_for_the_next_word() {
        // Confirmed against real bash: `FOO=bar if` really does run a
        // command literally named "if" (fails with "command not found"),
        // it does not open an if-block. Exit 127 either way (command not
        // found) is enough to confirm this parsed as Command::Simple, not
        // as an (invalid, dangling) Command::If.
        let mut shell = Shell::new();
        shell.run_source_here("FOO=bar if", "<test>");
        assert_eq!(shell.last_status, 127);
    }

    #[test]
    fn a_genuinely_bare_unmatched_marker_keyword_is_still_a_syntax_error() {
        // The one case the fix must NOT paper over: a `then`/`do`/`in`/...
        // with no assignment prefix and no preceding word at all (i.e.
        // truly the first token of a new simple command) is a syntax
        // error in real bash (`true; then echo hi` -> "unexpected token
        // `then'"), not a command literally named "then" to look up.
        let mut shell = Shell::new();
        let result = shell.run_source_here("true; then echo hi", "<test>");
        assert!(matches!(result, ExecResult::Status(2)), "{result:?}");
    }

    #[test]
    fn var_transform_q_shell_quotes_the_value() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"v="hello world"; echo "${v@Q}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "'hello world'\n");
    }

    #[test]
    fn var_transform_u_and_l_change_case_of_the_whole_value() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"v="Hello World"; echo "${v@U}"; echo "${v@L}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "HELLO WORLD\nhello world\n");
    }

    #[test]
    fn var_transform_e_expands_backslash_escapes_like_ansi_c_quoting() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"v='a\tb\nc'; printf '%s' "${v@E}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "a\tb\nc");
    }

    #[test]
    fn var_transform_e_leaves_an_unrecognized_escape_untouched() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"v='a\zb'; printf '%s' "${v@E}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "a\\zb");
    }

    #[test]
    fn an_unrecognized_at_transform_letter_is_a_expansion_failed() {
        // `${v@Z}` isn't one of the implemented transform letters, and
        // parse_brace_content falls back to reading the whole `${...}`
        // as a literal variable name -- which is a name nothing can
        // ever set, so check_param_name catches it. bash calls it a bad
        // substitution too; expanding it to the empty string, which is
        // what this used to do, is how the typo went unnoticed.
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        let result = shell.run_source_here(r#"v=hi; echo "[${v@Z}]""#, "<test>");
        assert!(matches!(result, ExecResult::Exit(1)), "{result:?}");
        // The capture takes stderr too -- the diagnostic is all there
        // is; the `echo` never ran.
        assert_eq!(buf.borrow().as_str(), "bish: ${v@Z}: bad substitution\n");
    }

    #[test]
    fn array_element_transform_q_quotes_just_that_element() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"a=("one two" three); echo "${a[0]@Q}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "'one two'\n");
    }

    #[test]
    fn var_transform_capital_a_reconstructs_a_scalar_assignment() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x=1; echo "${x@A}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "x='1'\n");
    }

    #[test]
    fn var_transform_capital_a_includes_attribute_flags_when_present() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"export ex=5; echo "${ex@A}"; readonly ro=hi; echo "${ro@A}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "declare -x ex='5'\ndeclare -r ro='hi'\n");
    }

    #[test]
    fn var_transform_capital_a_on_an_array_reconstructs_every_element() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"arr=(1 2 3); echo "${arr[@]@A}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "declare -a arr=([0]=\"1\" [1]=\"2\" [2]=\"3\")\n");
    }

    #[test]
    fn var_transform_capital_a_on_a_specific_array_index_keeps_the_array_flag() {
        // Confirmed against real bash: ${arr[0]@A} -> declare -a arr='1'
        // (still shows the array's own -a flag, but only that element).
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"arr=(1 2 3); echo "${arr[0]@A}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "declare -a arr='1'\n");
    }

    #[test]
    fn var_transform_lowercase_a_gives_just_the_attribute_letters() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x=1; echo "[${x@a}]"; export ex=1; echo "${ex@a}"; arr=(1); echo "${arr@a}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "[]\nx\na\n");
    }

    #[test]
    fn var_transform_capital_k_on_a_scalar_behaves_like_capital_q() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x="has space"; echo "${x@K}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "'has space'\n");
    }

    #[test]
    fn var_transform_capital_k_on_an_array_gives_key_value_pairs() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"arr=("has space" b); echo "${arr[@]@K}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "0 \"has space\" 1 \"b\"\n");
    }

    #[test]
    fn var_transform_capital_k_on_an_assoc_array_gives_key_value_pairs() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"declare -A m=([a]=1 [b]=2); echo "${m[@]@K}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "a \"1\" b \"2\"\n");
    }

    #[test]
    fn var_transform_capital_p_expands_common_prompt_escapes() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"p='\u@\h:\w\$ '; echo "${p@P}""#, "<test>");
        let expected = format!(
            "{}@{}:{}{} \n",
            prompt_username(),
            get_hostname().split('.').next().unwrap_or(""),
            shell.prompt_cwd(false),
            if is_effective_root() { "#" } else { "$" }
        );
        assert_eq!(buf.borrow().as_str(), expected);
    }

    #[test]
    fn var_transform_capital_p_strips_non_printing_sequence_brackets() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"p='[\[\e[1m\]hi\[\e[0m\]]'; echo "${p@P}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "[\x1b[1mhi\x1b[0m]\n");
    }

    #[test]
    fn var_transform_capital_p_octal_escape() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"p='\101\102'; echo "${p@P}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "AB\n");
    }

    #[test]
    fn var_transform_capital_p_custom_strftime_format() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"p='\D{%Y}'; echo "${p@P}""#, "<test>");
        // Just check it's a plausible 4-digit year, since we can't
        // control the wall clock from a unit test.
        let out = buf.borrow().clone();
        assert_eq!(out.trim_end().len(), 4);
        assert!(out.trim_end().chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn var_transform_capital_p_unrecognized_escape_passes_through() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"p='\z\q'; echo "${p@P}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "\\z\\q\n");
    }

    #[test]
    fn var_transform_capital_p_job_count() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"p='\j'; echo "${p@P}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "0\n");
    }

    #[test]
    fn prefix_names_expansion_lists_matching_variable_and_array_names_sorted() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"BFOO_B=1; BFOO_A=1; BFOO_ARR=(x); echo "${!BFOO_*}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "BFOO_A BFOO_ARR BFOO_B\n");
    }

    #[test]
    fn prefix_names_at_form_splits_into_separate_fields_when_quoted() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"CFOO_A=1; CFOO_B=2; out=(); for n in "${!CFOO_@}"; do out+=("[$n]"); done; echo "${out[@]}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "[CFOO_A] [CFOO_B]\n");
    }

    #[test]
    fn prefix_names_expansion_is_empty_when_nothing_matches() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"echo "[${!DEFINITELY_NOT_A_REAL_PREFIX_XYZ*}]""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "[]\n");
    }

    #[test]
    fn declare_g_forces_a_global_write_from_inside_a_function() {
        let mut shell = Shell::new();
        shell.run_source_here(r#"f() { declare -g dtest_g_var=5; }; f"#, "<test>");
        assert_eq!(shell.lookup_var("dtest_g_var"), "5");
    }

    #[test]
    fn local_g_forces_a_global_write_from_inside_a_function() {
        let mut shell = Shell::new();
        shell.run_source_here(r#"f() { local -g ltest_g_var=5; }; f"#, "<test>");
        assert_eq!(shell.lookup_var("ltest_g_var"), "5");
    }

    #[test]
    fn plain_declare_without_g_auto_localizes_inside_a_function() {
        // The bug the -g fix above was found alongside: before this, a
        // plain (non-`-g`) declare inside a function leaked its
        // assignment to the global scope exactly like an ordinary
        // assignment does, instead of auto-localizing like `local`.
        let mut shell = Shell::new();
        shell.run_source_here(r#"f() { declare dtest_local_var=5; }; f"#, "<test>");
        assert!(!shell.var_is_set("dtest_local_var"));
    }

    #[test]
    fn declare_p_prints_a_scalar_array_and_assoc_array_declaration() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x=1; a=(1 2 3); declare -p x a"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "declare -- x=\"1\"\ndeclare -a a=([0]=\"1\" [1]=\"2\" [2]=\"3\")\n");
    }

    #[test]
    fn declare_p_reflects_exported_and_readonly_attributes() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"export EE=5; readonly RR=hi; declare -p EE RR"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "declare -x EE=\"5\"\ndeclare -r RR=\"hi\"\n");
    }

    #[test]
    fn declare_p_on_an_unset_name_errors_and_exits_1() {
        let mut shell = Shell::new();
        let status = crate::builtins::vars::run_declare(&mut shell, "declare", &strs(&["-p", "DEFINITELY_NOT_SET_XYZ"]), &[]);
        assert_eq!(status, 1);
    }

    #[test]
    fn declare_f_prints_a_reparsable_function_definition() {
        let mut shell = Shell::new();
        shell.run_source_here("foo() { echo hi; }", "<test>");
        let buf = capture_output(&mut shell);
        assert_eq!(crate::builtins::vars::run_declare(&mut shell, "declare", &strs(&["-f", "foo"]), &[]), 0);
        let printed = buf.borrow().clone();
        assert!(printed.contains("foo"), "{printed:?}");

        // Round-trip: what got printed should itself be valid bish that
        // (re)defines the same function.
        let mut shell2 = Shell::new();
        let result = shell2.run_source_here(&printed, "<test>");
        assert!(matches!(result, ExecResult::Status(0)), "{result:?}");
        assert!(shell2.functions.contains_key("foo"));
    }

    #[test]
    fn declare_capital_f_prints_only_the_function_name() {
        let mut shell = Shell::new();
        shell.run_source_here("foo() { echo hi; }", "<test>");
        let buf = capture_output(&mut shell);
        // Named: the bare name, which is what this test has always been
        // called and what bash prints. The assertion said `declare -f
        // foo` -- the long form -- so the name described bash and the
        // assertion pinned what bish did instead.
        crate::builtins::vars::run_declare(&mut shell, "declare", &strs(&["-F", "foo"]), &[]);
        assert_eq!(buf.borrow().as_str(), "foo\n");
        // Unnamed: a re-readable declaration line per function, which
        // is the other half of bash's rule and the reason the two
        // forms differ at all.
        buf.borrow_mut().clear();
        crate::builtins::vars::run_declare(&mut shell, "declare", &strs(&["-F"]), &[]);
        assert_eq!(buf.borrow().as_str(), "declare -f foo\n");
    }

    #[test]
    fn declare_f_on_an_unknown_function_errors_and_exits_1() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::vars::run_declare(&mut shell, "declare", &strs(&["-f", "not_a_real_function"]), &[]), 1);
    }

    #[test]
    fn arithmetic_comma_operator_sequences_left_to_right_keeping_the_last_value() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"echo $(( a=1, a=2, a=3 )); echo "$a""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "3\n3\n");
    }

    #[test]
    fn arithmetic_comma_operator_works_inside_a_parenthesized_grouping() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"echo $(( (a=1, b=2) + a + b ))"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "5\n");
    }

    #[test]
    fn arithmetic_comma_operator_works_in_a_double_paren_statement() {
        let mut shell = Shell::new();
        shell.run_source_here(r#"((a=1, b=2, c=a+b))"#, "<test>");
        assert_eq!(shell.lookup_var("a"), "1");
        assert_eq!(shell.lookup_var("b"), "2");
        assert_eq!(shell.lookup_var("c"), "3");
    }

    #[test]
    fn a_function_shadows_a_same_named_builtin() {
        // Moves the real process cwd (`cd` is a real chdir -- see
        // change_directory), so it has to take the same lock every
        // other cwd-moving test does.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Confirmed against real bash: a user function of the same name
        // as an ordinary builtin (even a POSIX "special" one like
        // export/return/break) wins, with `builtin`/`command` as the
        // explicit bypasses.
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"cd() { echo "fake cd $1"; }; cd /somewhere"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "fake cd /somewhere\n");
    }

    #[test]
    fn builtin_bypasses_a_shadowing_function() {
        // Moves the real process cwd (`cd` is a real chdir -- see
        // change_directory), so it has to take the same lock every
        // other cwd-moving test does.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"cd() { echo "fake cd $1"; }; builtin cd /tmp"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "", "the function must not have run");
        assert_eq!(shell.cwd.to_string_lossy(), "/tmp");
    }

    #[test]
    fn builtin_on_an_unrecognized_name_errors_without_spawning_external() {
        let mut shell = Shell::new();
        let result = shell.run_source_here("builtin not_a_real_builtin_or_external_xyz", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
    }

    #[test]
    fn bare_builtin_is_a_silent_noop() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        let result = shell.run_source_here("builtin", "<test>");
        assert!(matches!(result, ExecResult::Status(0)), "{result:?}");
        assert_eq!(buf.borrow().as_str(), "");
    }

    #[test]
    fn restrict_to_builtins_still_blocks_a_shadowing_function() {
        // Moves the real process cwd (`cd` is a real chdir -- see
        // change_directory), so it has to take the same lock every
        // other cwd-moving test does.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // restrict_to_builtins (command mode's colon-line) means "only
        // real builtins run here" -- unlike ordinary dispatch, it must
        // NOT let a same-named function preempt the actual builtin.
        let mut shell = Shell::new();
        shell.restrict_to_builtins = true;
        let buf = capture_output(&mut shell);
        // Defining the function itself still works (declare/functions
        // aren't gated); only invoking it as a command is.
        shell.restrict_to_builtins = false;
        shell.run_source_here("cd() { echo fake; }", "<test>");
        shell.restrict_to_builtins = true;
        shell.run_source_here("cd /tmp", "<test>");
        assert_eq!(buf.borrow().as_str(), "");
        assert_eq!(shell.cwd.to_string_lossy(), "/tmp");
    }

    #[test]
    fn restrict_to_builtins_still_errors_on_a_genuinely_unknown_command() {
        let mut shell = Shell::new();
        shell.restrict_to_builtins = true;
        let result = shell.run_source_here("not_a_real_builtin_or_function_xyz", "<test>");
        assert!(matches!(result, ExecResult::Status(127)), "{result:?}");
    }

    #[test]
    fn array_literal_explicit_keys_are_sparse_indices_not_sequential() {
        let mut shell = Shell::new();
        shell.run_source_here("arr=([2]=x [5]=y)", "<test>");
        assert_eq!(shell.arrays.get("arr").and_then(|m| m.get(&2)).map(String::as_str), Some("x"));
        assert_eq!(shell.arrays.get("arr").and_then(|m| m.get(&5)).map(String::as_str), Some("y"));
        assert_eq!(shell.arrays.get("arr").map(|m| m.len()), Some(2));
    }

    #[test]
    fn array_literal_mixed_positional_and_keyed_resumes_the_running_index() {
        // Matches real bash: (1 [5]=x 2) -> indices 0, 5, 6.
        let mut shell = Shell::new();
        shell.run_source_here("arr=(1 [5]=x 2)", "<test>");
        let arr = shell.arrays.get("arr").cloned().unwrap_or_default();
        assert_eq!(arr.get(&0).map(String::as_str), Some("1"));
        assert_eq!(arr.get(&5).map(String::as_str), Some("x"));
        assert_eq!(arr.get(&6).map(String::as_str), Some("2"));
    }

    #[test]
    fn array_literal_append_continues_past_the_current_max_index() {
        let mut shell = Shell::new();
        shell.run_source_here("arr=(1 2 3); arr+=(4 5)", "<test>");
        let arr = shell.arrays.get("arr").cloned().unwrap_or_default();
        assert_eq!(arr.values().cloned().collect::<Vec<_>>(), vec!["1", "2", "3", "4", "5"]);
    }

    #[test]
    fn array_literal_assoc_prefix_assignment_populates_declared_keys() {
        let mut shell = Shell::new();
        shell.run_source_here("declare -A m; m=([a]=1 [b]=2)", "<test>");
        assert_eq!(shell.assoc_arrays.get("m").and_then(|m| m.get("a")).map(String::as_str), Some("1"));
        assert_eq!(shell.assoc_arrays.get("m").and_then(|m| m.get("b")).map(String::as_str), Some("2"));
    }

    #[test]
    fn declare_capital_a_with_an_inline_array_literal_as_a_later_word() {
        // The actual gap this whole batch of work was about: `declare -A
        // m=([a]=1 [b]=2)` used to mis-parse entirely (the parenthesized
        // body got read as an unrelated subshell command) since array-
        // literal recognition only applied in leading-prefix-assignment
        // position, not to a later word of one of these five builtins.
        let mut shell = Shell::new();
        shell.run_source_here("declare -A m=([a]=1 [b]=2)", "<test>");
        assert_eq!(shell.assoc_arrays.get("m").and_then(|m| m.get("a")).map(String::as_str), Some("1"));
        assert_eq!(shell.assoc_arrays.get("m").and_then(|m| m.get("b")).map(String::as_str), Some("2"));
    }

    #[test]
    fn declare_lowercase_a_with_an_inline_array_literal_as_a_later_word() {
        let mut shell = Shell::new();
        shell.run_source_here("declare -a arr=(1 2 3)", "<test>");
        let arr = shell.arrays.get("arr").cloned().unwrap_or_default();
        assert_eq!(arr.values().cloned().collect::<Vec<_>>(), vec!["1", "2", "3"]);
    }

    #[test]
    fn typeset_accepts_the_same_inline_array_literal_as_declare() {
        let mut shell = Shell::new();
        shell.run_source_here("typeset -A m2=([x]=y)", "<test>");
        assert_eq!(shell.assoc_arrays.get("m2").and_then(|m| m.get("x")).map(String::as_str), Some("y"));
    }

    #[test]
    fn local_with_an_inline_array_literal_is_properly_function_scoped() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"f() { local -A m=([a]=1 [b]=2); echo "${m[a]} ${m[b]}"; }; f"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "1 2\n");
        assert!(!shell.assoc_names.contains("m"), "m must not leak out of the function");
    }

    #[test]
    fn local_lowercase_a_with_an_inline_array_literal_is_properly_function_scoped() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"f() { local -a arr=(1 2 3); echo "${arr[@]}"; }; f"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "1 2 3\n");
        assert!(!shell.arrays.contains_key("arr"), "arr must not leak out of the function");
    }

    #[test]
    fn restricted_mode_blocks_cd() {
        // Moves the real process cwd (`cd` is a real chdir -- see
        // change_directory), so it has to take the same lock every
        // other cwd-moving test does.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let result = shell.run_source_here("cd /tmp", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
    }

    #[test]
    fn restricted_mode_is_a_one_way_latch() {
        // Moves the real process cwd (`cd` is a real chdir -- see
        // change_directory), so it has to take the same lock every
        // other cwd-moving test does.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut shell = Shell::new();
        shell.run_source_here("set -r; set +r", "<test>");
        let result = shell.run_source_here("cd /tmp", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
    }

    #[test]
    fn restricted_mode_blocks_a_slash_in_a_command_name() {
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let result = shell.run_source_here("/bin/echo hi", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
    }

    #[test]
    fn restricted_mode_blocks_command_dash_p() {
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let result = shell.run_source_here("command -p echo hi", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
    }

    #[test]
    fn restricted_mode_protects_shell_path_env_bash_env() {
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        for name in ["SHELL", "PATH", "ENV", "BASH_ENV"] {
            // Fatal, not merely refused: a write to a readonly name
            // stops a non-interactive shell, which is what real bash
            // does here too (`bash -r -c 'SHELL=/x; echo after'`
            // prints the error and nothing else, exiting 1).
            let result = shell.run_source_here(&format!("{name}=/tmp"), "<test>");
            assert!(matches!(result, ExecResult::Exit(1)), "{name}: {result:?}");
        }
        // Still readable/unchanged, just refused as a write target.
        assert_ne!(shell.lookup_var("PATH"), "/tmp");
    }

    #[test]
    fn restricted_mode_blocks_unsetting_path() {
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        shell.run_source_here("unset PATH", "<test>");
        assert!(shell.var_is_set("PATH"));
    }

    #[test]
    fn restricted_mode_blocks_output_redirection_for_an_external_command() {
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let result = shell.run_source_here("cat /etc/hostname > /tmp/bish_restricted_test_xyz", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
        assert!(!std::path::Path::new("/tmp/bish_restricted_test_xyz").exists());
    }

    #[test]
    fn restricted_mode_allows_input_redirection() {
        // Plain `<` must not be treated as a write and refused --
        // confirmed by exit status alone (an external command's own
        // stdout goes straight to this process's real inherited stdio,
        // not through capture_output's sink, so its content isn't
        // observable from a unit test here).
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let result = shell.run_source_here("cat < /etc/hostname", "<test>");
        assert!(matches!(result, ExecResult::Status(0)), "{result:?}");
    }

    #[test]
    fn restricted_mode_blocks_exec_with_a_command() {
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let result = shell.run_source_here("exec ls", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
    }

    #[test]
    fn restricted_mode_allows_bare_exec_and_its_own_redirects() {
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let result = shell.run_source_here("exec", "<test>");
        assert!(matches!(result, ExecResult::Status(0)), "{result:?}");
    }

    #[test]
    fn restricted_mode_blocks_source_with_a_slash() {
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let result = shell.run_source_here(". /etc/hostname", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
        let result2 = shell.run_source_here("source /etc/hostname", "<test>");
        assert!(matches!(result2, ExecResult::Status(1)), "{result2:?}");
    }

    #[test]
    fn restricted_mode_persists_into_a_virtual_child() {
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let child = shell.new_virtual_child();
        assert!(child.opt_restricted);
    }

    #[test]
    fn restricted_mode_shows_up_in_dollar_dash() {
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        assert!(shell.lookup_var("-").contains('r'));
    }

    #[test]
    fn set_o_posix_is_recognized_and_toggleable() {
        let mut shell = Shell::new();
        let result = shell.run_source_here("set -o posix", "<test>");
        assert!(matches!(result, ExecResult::Status(0)), "{result:?}");
        assert!(shell.opt_posix);
        shell.run_source_here("set +o posix", "<test>");
        assert!(!shell.opt_posix);
    }

    // Regression: a `${var op word}` expansion's raw-word scan used to stop
    // at the first literal '}', even one reached only through a quoted
    // nested expansion (`${x:-"${x}"}`, the `${VAR+"${VAR}"}` idiom mise's
    // own activation script uses) -- it would mistake that inner '}' for
    // the outer terminator and then fail with "unterminated double quote".
    #[test]
    fn var_op_word_with_a_nested_quoted_expansion_finds_the_real_closing_brace() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x=hi; echo ${y:-"${x}"}"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "hi\n");
    }

    #[test]
    fn var_op_word_plain_unescaped_braces_still_count_toward_depth() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x=; echo "${x:-{}}""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "{}\n");
    }

    #[test]
    fn var_op_word_an_escaped_brace_does_not_count_toward_depth() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x=; echo ${x:-\{}"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "{\n");
    }

    #[test]
    fn var_op_word_a_quoted_brace_never_counts_toward_depth() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"y=; echo ${y:-"a}b"}"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "a}b\n");
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"y=; echo ${y:-"a{b"}"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "a{b\n");
    }

    // Regression: `declare -f`/`-F` on a name that isn't a function used to
    // print "declare: NAME: not found" to stderr -- real bash fails
    // silently there (unlike `declare -p` on a nonexistent variable, which
    // does print a message). This surfaced as a spurious message on every
    // bish startup once the nested-expansion fix above let mise's own
    // `declare -F _mise_hook >/dev/null && unset -f _mise_hook`-style guards
    // actually run.
    #[test]
    fn declare_f_on_a_nonexistent_function_is_silent() {
        let mut shell = Shell::new();
        // capture_output's sink backs both stdout and stderr, so an empty
        // buffer here means the builtin wrote nothing at all.
        let buf = capture_output(&mut shell);
        let result = shell.run_source_here("declare -F not_a_real_function_xyz", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
        assert_eq!(buf.borrow().as_str(), "");
    }

    // Regression: builtins used to ignore their own per-command redirects
    // entirely (their output always went straight to the shell's own
    // sink) -- confirmed as a real problem via mise's own bash activation
    // script, which relies on `declare -p foo >/dev/null 2>&1`/`declare -F
    // foo >/dev/null`-style guards actually being silenced. These check
    // push_builtin_output_sink/pop_builtin_output_sink (installed around
    // every dispatch_builtin_or_external call) against real bash's own
    // observed behavior for each redirect form.
    #[test]
    fn builtin_stderr_redirected_to_dev_null_leaves_the_shells_own_sink_untouched() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here("declare -p not_a_real_var_xyz >/dev/null 2>&1; echo done", "<test>");
        assert_eq!(buf.borrow().as_str(), "done\n");
    }

    #[test]
    fn builtin_stderr_redirected_to_a_file_is_captured_there_not_in_the_shells_sink() {
        let dir = std::env::temp_dir().join(format!("bish_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("err.txt");
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(&format!("declare -p not_a_real_var_xyz 2>{}; echo done", path.display()), "<test>");
        assert_eq!(buf.borrow().as_str(), "done\n");
        assert!(std::fs::read_to_string(&path).unwrap().contains("not_a_real_var_xyz"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_stdout_redirected_to_a_file_with_append_writes_there_not_in_the_shells_sink() {
        let dir = std::env::temp_dir().join(format!("bish_test_{}", std::process::id() + 1));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.txt");
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(&format!("echo one >{p}; echo two >>{p}", p = path.display()), "<test>");
        assert_eq!(buf.borrow().as_str(), "");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_both_stdout_and_stderr_redirected_via_ampersand_gt_land_in_the_same_file() {
        let dir = std::env::temp_dir().join(format!("bish_test_{}", std::process::id() + 2));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("both.txt");
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(&format!("declare -p not_a_real_var_xyz &>{}", path.display()), "<test>");
        assert_eq!(buf.borrow().as_str(), "");
        assert!(std::fs::read_to_string(&path).unwrap().contains("not_a_real_var_xyz"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_with_no_output_redirect_still_goes_through_the_shells_own_sink() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here("echo hi", "<test>");
        assert_eq!(buf.borrow().as_str(), "hi\n");
    }

    // The one thing a parallel table can get wrong: an option nobody
    // wrote a line for, or a line for an option that no longer exists.
    #[test]
    fn every_bishopt_is_described() {
        let mut options: Vec<&str> = KNOWN_BISHOPTS.iter().map(|(n, _)| *n).collect();
        let mut described: Vec<&str> = BISHOPT_HELP.iter().map(|(n, _)| *n).collect();
        options.sort_unstable();
        described.sort_unstable();
        assert_eq!(options, described, "KNOWN_BISHOPTS and BISHOPT_HELP disagree");
        assert!(BISHOPT_HELP.iter().all(|(_, d)| !d.is_empty()), "an empty description is not a description");
    }

    #[test]
    fn describe_reports_what_an_option_accepts_and_what_it_is_set_to() {
        let mut shell = Shell::new();
        let lines = shell.describe_bishopts(KNOWN_BISHOPTS, Some("shiftwidth")).join("\n");
        assert!(lines.contains("shiftwidth"), "{lines}");
        assert!(lines.contains("How many columns one indent is"), "{lines}");
        assert!(lines.contains("accepts: 1-64"), "{lines}");
        assert!(lines.contains("default: 4"), "{lines}");
        assert!(lines.contains("now: 4"), "{lines}");
        // ...and it follows the live value.
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "shiftwidth", "2"]), KNOWN_BISHOPTS), 0);
        assert!(shell.describe_bishopts(KNOWN_BISHOPTS, Some("shiftwidth")).join("\n").contains("now: 2"));
    }

    #[test]
    fn describing_everything_covers_every_option() {
        let shell = Shell::new();
        let all = shell.describe_bishopts(KNOWN_BISHOPTS, None);
        assert_eq!(all.len(), KNOWN_BISHOPTS.len() * 3, "three lines each");
    }

    #[test]
    fn describing_an_option_that_does_not_exist_fails() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--describe", "nonsense"]), KNOWN_BISHOPTS), 1);
    }

    fn hook_ids(shell: &mut Shell) -> Vec<u64> {
        shell.hooks.iter().map(|h| h.id).collect()
    }

    #[test]
    fn adding_a_hook_returns_an_id_that_removes_it() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "editor:file:open", "__setup"])), 0);
        assert_eq!(crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "editor:file:close", "__teardown"])), 0);
        assert_eq!(hook_ids(&mut shell), vec![1, 2], "ids come from a counter, in order");
        assert_eq!(crate::builtins::bish::run_hook(&mut shell, &strs(&["rm", "1"])), 0);
        assert_eq!(hook_ids(&mut shell), vec![2]);
        // ...and an id is never reused, so `rm` can't hit the wrong one.
        assert_eq!(crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "editor:file:open", "__again"])), 0);
        assert_eq!(hook_ids(&mut shell), vec![2, 3]);
    }

    fn lsp_ids(shell: &Shell) -> Vec<u64> {
        shell.lsp_servers.iter().map(|s| s.id).collect()
    }

    // `KNOWN_BUILTINS` is not just the completion list: `run_multi`
    // reads it to decide whether a pipeline stage needs the self-exec
    // that lets a builtin run as one. A name the dispatcher handles but
    // this list omits gets spawned as an external program instead and
    // dies with ENOENT -- which is what `echo hi | json .` did, while
    // `json .` on its own worked, so nothing else noticed.
    //
    // Read out of the source because there is no table to compare
    // against: the dispatcher is a `match`, and the two halves can only
    // be kept honest by looking at it. A new arm shaped differently
    // enough to escape this regex is a false negative, not a false
    // failure; a false *failure* means the arm is genuinely absent from
    // the list, and adding it there is the fix.
    // The completion menu is only worth having if it lists what is real.
    // Asked of the dispatcher itself rather than of a second copy of the
    // list: `::bish` says "unknown subcommand" for anything it does not
    // handle, so running each one and looking for that answers the
    // question directly.
    #[test]
    fn every_offered_bish_subcommand_is_really_dispatched() {
        let unknown = |args: &[&str]| {
            let mut shell = Shell::new();
            // `Capture` takes stderr too, which is where the
            // "unknown subcommand" line goes.
            let out = capture_output(&mut shell);
            crate::builtins::bish::run_bish(&mut shell, &strs(args));
            let seen = out.borrow().clone();
            seen.contains("unknown subcommand")
        };
        // The negative control: a name nothing dispatches really does
        // say so, or this test proves nothing.
        assert!(unknown(&["bishNoSuchSubcommand"]), "the probe itself works");

        for sub in bish_subcommands() {
            assert!(!unknown(&[sub]), "::bish {sub} is offered but not dispatched");
            for second in bish_sub_subcommands(sub) {
                assert!(!unknown(&[sub, second]), "::bish {sub} {second} is offered but not dispatched");
            }
        }
    }

    // Same question for the one flag with a fixed set of values behind
    // it: an offered value that `add` then rejects would be worse than
    // offering nothing.
    #[test]
    fn every_offered_apply_edits_value_is_accepted() {
        for value in lsp_apply_edits_values() {
            let mut shell = Shell::new();
            assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", &format!("--apply-edits={value}"), "x"])), 0, "{value}");
            assert_eq!(shell.lsp_servers[0].apply_edits, *value);
        }
    }

    #[test]
    fn every_dispatched_builtin_is_known() {
        let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/exec.rs")).expect("exec.rs is readable from its own tests");
        let start = source.find("fn dispatch_builtin_or_external_impl(").expect("the dispatcher is still called that");
        let body = &source[start..];
        let end = body[10..].find("\n    fn ").expect("the dispatcher ends somewhere") + 10;
        let mut missing: Vec<String> = Vec::new();
        for line in body[..end].lines() {
            // Arms of the form `            "a" | "b" => {`, at the
            // dispatcher's own match indentation.
            let Some(rest) = line.strip_prefix("            \"") else { continue };
            let Some(names) = rest.split("=>").next() else { continue };
            if !line.contains("=>") {
                continue;
            }
            for name in names.split('|') {
                let name = name.trim().trim_matches('"');
                if name.is_empty() || name.contains(' ') {
                    continue;
                }
                if !KNOWN_BUILTINS.contains(&name) {
                    missing.push(name.to_string());
                }
            }
        }
        assert!(missing.is_empty(), "these are dispatched as builtins but missing from KNOWN_BUILTINS, so they break in a pipeline: {missing:?}");
    }

    #[test]
    fn adding_a_language_server_returns_an_id_that_removes_it() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--lang=rust", "rust-analyzer"])), 0);
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--lang=python", "pylsp"])), 0);
        assert_eq!(lsp_ids(&shell), vec![1, 2], "ids come from a counter, in order");
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["rm", "1"])), 0);
        assert_eq!(lsp_ids(&shell), vec![2]);
        // Never reused, so `rm` can't hit the wrong one -- same
        // contract `hook` ids have.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "gopls"])), 0);
        assert_eq!(lsp_ids(&shell), vec![2, 3]);
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["rm", "99"])), 1);
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["rm", "nonsense"])), 2);
    }

    #[test]
    fn a_command_keeps_its_words_and_defaults_to_a_git_root() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--lang=c", "clangd", "--background-index"])), 0);
        let server = &shell.lsp_servers[0];
        assert_eq!(server.command, vec!["clangd".to_string(), "--background-index".to_string()]);
        assert_eq!(server.lang, "c");
        assert_eq!(server.root_markers, vec![".git".to_string()]);
        assert_eq!(server.command_line(), "clangd --background-index");
        // ...and a word that would stop being one word does get quoted.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "some server", "--flag=a b"])), 0);
        assert_eq!(shell.lsp_servers[1].command_line(), "'some server' '--flag=a b'");
    }

    #[test]
    fn a_root_command_is_recorded_and_shown_instead_of_the_markers() {
        let mut shell = Shell::new();
        assert_eq!(
            crate::builtins::bish::run_lsp(
                &mut shell,
                &strs(&["add", "--lang=rust", "--root-cmd", "cargo metadata | json .workspace_root", "rust-analyzer"])
            ),
            0
        );
        assert_eq!(shell.lsp_servers[0].root_cmd, "cargo metadata | json .workspace_root");
        // `--root` still has its default, since the command is what
        // gets asked first and the markers are the fallback.
        assert_eq!(shell.lsp_servers[0].root_markers, vec![".git".to_string()]);

        // Either order, and the `=` spelling.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--root=go.mod", "--root-cmd=go env GOMOD", "gopls"])), 0);
        assert_eq!(shell.lsp_servers[1].root_cmd, "go env GOMOD");
        assert_eq!(shell.lsp_servers[1].root_markers, vec!["go.mod".to_string()]);

        // A flag with nothing usable after it is a config error, not a
        // silently empty command that would never run.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--root-cmd"])), 2);
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--root-cmd", "   ", "x"])), 2);
        assert_eq!(shell.lsp_servers.len(), 2);
    }

    #[test]
    fn lsp_add_collects_repeated_settings_in_any_position() {
        let mut shell = Shell::new();
        let _out = capture_output(&mut shell);
        assert_eq!(
            crate::builtins::bish::run_lsp(
                &mut shell,
                &strs(&[
                    "add",
                    "--setting",
                    "rust-analyzer.check.command=clippy",
                    "--lang=rust",
                    "--setting=rust-analyzer.cargo.features=[\"all\"]",
                    "rust-analyzer",
                ])
            ),
            0
        );
        let server = &shell.lsp_servers[0];
        // The flags are round-robin, so a `--setting` before `--lang`
        // is not mistaken for the command to run.
        assert_eq!(server.command, vec!["rust-analyzer".to_string()]);
        assert_eq!(server.lang, "rust");
        assert_eq!(
            server.settings,
            vec![
                ("rust-analyzer.check.command".to_string(), "clippy".to_string()),
                ("rust-analyzer.cargo.features".to_string(), "[\"all\"]".to_string()),
            ]
        );

        // A repeated key is the later one, not both -- the same rule
        // `settings_tree` follows for a key that contradicts an earlier
        // one.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--setting", "a.b=1", "--setting", "a.b=2", "srv"])), 0);
        assert_eq!(shell.lsp_servers[1].settings, vec![("a.b".to_string(), "2".to_string())]);

        // A value is free to contain `=`; a key is not.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--setting", "a=b=c", "srv"])), 0);
        assert_eq!(shell.lsp_servers[2].settings, vec![("a".to_string(), "b=c".to_string())]);

        // Malformed is a config error rather than a setting nobody
        // notices was dropped.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--setting", "nokey", "srv"])), 2);
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--setting", "=value", "srv"])), 2);
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--setting"])), 2);
        assert_eq!(shell.lsp_servers.len(), 3);
    }

    #[test]
    fn lineno_is_the_line_of_the_statement_running() {
        let mut shell = Shell::new();
        let out = capture_output(&mut shell);
        shell.run_source_here("echo \"line $LINENO\"\necho \"line $LINENO\"\n\nf(){ echo \"in f: $LINENO\"; }\nf\n", "<test>");
        // Checked against real bash, which gives exactly these three.
        assert_eq!(out.borrow().as_str(), "line 1\nline 2\nin f: 4\n");
    }

    #[test]
    fn the_epoch_variables_are_a_real_clock() {
        let mut shell = Shell::new();
        let out = capture_output(&mut shell);
        shell.run_source_here("echo $EPOCHSECONDS\necho $EPOCHREALTIME", "<test>");
        let seen = out.borrow().clone();
        let mut lines = seen.lines();
        let secs: u64 = lines.next().unwrap().parse().expect("a whole number of seconds");
        // Somewhere after this was written and before the far future --
        // enough to catch a zero or a garbled value without pinning a
        // date.
        assert!(secs > 1_700_000_000, "{secs}");

        let real = lines.next().unwrap();
        let (whole, frac) = real.split_once('.').expect("a decimal point, not a locale separator");
        assert!(whole.parse::<u64>().unwrap().abs_diff(secs) <= 1);
        assert_eq!(frac.len(), 6, "microseconds, zero-padded: {real}");
    }

    // Every expectation was run against real bash first -- the
    // announce rule in particular is not guessable (a literal `.`
    // component prints; an *empty* one does not).
    #[test]
    fn cdpath_searches_and_says_where_it_landed() {
        // Moves the real process cwd (`cd` is a real chdir -- see
        // change_directory), so it has to take the same lock every
        // other cwd-moving test does.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // `cd` moves the *real* process directory (see
        // `change_directory`), so this has to be put back -- otherwise
        // every later test resolving a relative path runs from a
        // directory this one deleted.
        let restore = std::env::current_dir().expect("a current directory");
        let root = std::env::temp_dir().join(format!("bish-cdpath-{}", std::process::id()));
        let (here, away) = (root.join("here"), root.join("away"));
        std::fs::create_dir_all(here.join("alpha")).unwrap();
        std::fs::create_dir_all(away.join("beta")).unwrap();
        std::fs::create_dir_all(away.join("alpha")).unwrap();

        // `unset CDPATH` up front on every one: a plain assignment is
        // real process-wide environment (see `sync_real_state_in`'s own
        // doc comment), so a value one of these scripts sets is still
        // there for the next `Shell::new()`.
        let run = |script: &str| {
            let mut shell = Shell::new();
            let out = capture_output(&mut shell);
            shell.run_source_here(&format!("unset CDPATH\n{script}"), "<test>");
            out.borrow().clone()
        };
        let prelude = format!("cd {}; CDPATH={}", here.display(), away.display());

        // The announcement *is* the observable here, and a better one
        // than `pwd`: it says both that CDPATH was used and which
        // component won. (`pwd` reports the real process directory,
        // which a test shell's own `cd` deliberately does not move.)

        // Found only via CDPATH: goes there, and says so.
        assert_eq!(run(&format!("{prelude}; cd beta")), format!("{}\n", away.join("beta").display()));

        // A name that exists in *both* -- CDPATH wins, because that is
        // what a search path means.
        assert_eq!(run(&format!("{prelude}; cd alpha")), format!("{}\n", away.join("alpha").display()));

        // An empty component means "here", and is the one hit that is
        // not announced. A literal `.` is announced, which is the part
        // that is not guessable.
        assert_eq!(run(&format!("cd {}; CDPATH=:{}; cd alpha", here.display(), away.display())), "");
        assert_eq!(run(&format!("cd {}; CDPATH=.:{}; cd alpha", here.display(), away.display())), format!("{}\n", here.join("alpha").display()));

        // Never searched: an explicit `./`, an absolute path, or no
        // CDPATH at all.
        assert_eq!(run(&format!("{prelude}; cd ./alpha")), "");
        assert_eq!(run(&format!("{prelude}; cd {}", away.join("beta").display())), "");
        assert_eq!(run(&format!("cd {}; cd alpha", here.display())), "");

        // A name no component holds is the ordinary failure, reported
        // against the name as typed rather than against some candidate
        // the search happened to try last.
        let seen = run(&format!("{prelude}; cd nope"));
        assert!(seen.contains("cd: nope:"), "{seen}");

        std::env::set_current_dir(&restore).expect("restoring the test process's own directory");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn command_not_found_handle_is_called_with_the_whole_command() {
        let mut shell = Shell::new();
        let out = capture_output(&mut shell);
        shell.run_source_here(
            r#"command_not_found_handle(){ echo "no $1 (args: $*)"; return 42; }
bish_no_such_command a b
echo "status=$?""#,
            "<test>",
        );
        assert_eq!(out.borrow().as_str(), "no bish_no_such_command (args: bish_no_such_command a b)\nstatus=42\n");
    }

    #[test]
    fn without_a_handler_a_missing_command_is_still_127() {
        let mut shell = Shell::new();
        let out = capture_output(&mut shell);
        shell.run_source_here("bish_no_such_command\necho \"status=$?\"", "<test>");
        assert!(out.borrow().contains("status=127"), "{}", out.borrow());
    }

    // Real bash loops forever here. The guard is a deliberate
    // divergence, not an accident: a handler that itself mistypes
    // something would otherwise call itself until the machine gives up.
    #[test]
    fn a_handler_that_mistypes_something_does_not_call_itself_forever() {
        let mut shell = Shell::new();
        let out = capture_output(&mut shell);
        shell.run_source_here("command_not_found_handle(){ echo H; bish_also_missing; }\nbish_no_such_command\necho \"status=$?\"", "<test>");
        let seen = out.borrow().clone();
        assert_eq!(seen.matches('H').count(), 1, "the handler ran exactly once: {seen}");
        assert!(seen.contains("status=127"));
    }

    // The one thing a `PROMPT_COMMAND` must not do is change the answer
    // to `$?` for the command the user actually ran -- a prompt that
    // colours itself by the last status would be wrong on every line
    // after the first.
    #[test]
    fn prompt_command_runs_and_gives_the_status_back() {
        let mut shell = Shell::new();
        let out = capture_output(&mut shell);
        shell.run_source_here("false", "<test>");
        assert_eq!(shell.last_status(), 1);
        shell.run_source_here("PROMPT_COMMAND='true; echo PC'", "<test>");
        shell.set_last_status(1);
        shell.run_prompt_command();
        assert_eq!(out.borrow().as_str(), "PC\n");
        assert_eq!(shell.last_status(), 1, "the prompt hook's own `true` must not become the user's `$?`");
    }

    #[test]
    fn prompt_command_takes_the_array_form_too() {
        let mut shell = Shell::new();
        let out = capture_output(&mut shell);
        // bash 5.1's spelling: each element run in turn.
        shell.run_source_here("PROMPT_COMMAND=('echo A' 'echo B')", "<test>");
        shell.run_prompt_command();
        assert_eq!(out.borrow().as_str(), "A\nB\n");
    }

    #[test]
    fn an_unset_or_empty_prompt_command_runs_nothing() {
        let mut shell = Shell::new();
        let out = capture_output(&mut shell);
        shell.run_prompt_command();
        shell.run_source_here("PROMPT_COMMAND='   '", "<test>");
        shell.run_prompt_command();
        assert_eq!(out.borrow().as_str(), "");
    }

    // Every expectation in this test was run against real bash first.
    // The inheritance rules in particular are not guessable: DEBUG and
    // RETURN ride on `functrace`, ERR on `errtrace`, and they are two
    // different options for a reason.
    #[test]
    fn the_pseudo_signal_traps_fire_where_bash_fires_them() {
        let run = |script: &str| {
            let mut shell = Shell::new();
            let out = capture_output(&mut shell);
            shell.run_source_here(script, "<test>");
            out.borrow().clone()
        };
        assert_eq!(run("trap 'echo DBG' DEBUG; true; true"), "DBG\nDBG\n", "before each command, not after");
        assert_eq!(run("trap 'echo ERR' ERR; false; true"), "ERR\n");
        assert_eq!(run("trap 'echo ERR' ERR; true"), "");

        // ERR follows `errexit`'s own exemptions, because it is decided
        // in the same place.
        assert_eq!(run("trap 'echo E' ERR; if false; then :; fi; echo done"), "done\n");
        assert_eq!(run("trap 'echo E' ERR; false || true; echo done"), "done\n");
        assert_eq!(run("trap 'echo E' ERR; ! false; echo done"), "done\n");

        // ...and it fires whether or not `errexit` is on, which is the
        // point: a script traps ERR so it does not have to exit.
        assert_eq!(run("trap 'echo E' ERR; false; echo after"), "E\nafter\n");

        // Not inherited into a function without the option that says so.
        assert_eq!(run("f(){ false; }; trap 'echo E' ERR; f; echo done"), "E\ndone\n");
        assert_eq!(run("set -E; f(){ false; }; trap 'echo E' ERR; f; echo done"), "E\nE\ndone\n");
        assert_eq!(run("f(){ echo in; }; trap 'echo RET' RETURN; f"), "in\n");
        assert_eq!(run("set -T; f(){ echo in; }; trap 'echo RET' RETURN; f"), "in\nRET\n");

        // `-` clears one, same as for a real signal.
        assert_eq!(run("trap 'echo D' DEBUG; trap - DEBUG; true; true"), "D\n");

        // A trap does not get to change `$?` of the command it fired
        // around.
        assert_eq!(run("trap 'false' DEBUG; echo hi; echo $?"), "hi\n0\n");
    }

    // Every expectation here was run against real bash before being
    // asserted -- `PIPESTATUS` is the kind of thing where "what I think
    // bash does" and "what bash does" are easy to confuse.
    #[test]
    fn pipestatus_reports_every_stage() {
        let run = |script: &str| {
            let mut shell = Shell::new();
            let out = capture_output(&mut shell);
            shell.run_source_here(script, "<test>");
            out.borrow().clone()
        };
        // `sh -c` rather than bare `true`/`false`: those are builtins
        // now, and a builtin in a pipeline stage self-execs -- under
        // `cargo test` that resolves to the test harness rather than to
        // bish (the `current_exe` trap this codebase documents
        // elsewhere), so it is untestable from here whatever it is.
        assert_eq!(run(r#"sh -c 'exit 0' | sh -c 'exit 1'; echo "${PIPESTATUS[0]} ${PIPESTATUS[1]}""#), "0 1\n");
        assert_eq!(run(r#"sh -c 'exit 1' | sh -c 'exit 0'; echo "${PIPESTATUS[0]} ${PIPESTATUS[1]}""#), "1 0\n");
        // Real external stages, not `exit N`: a builtin in a pipeline
        // stage self-execs, and under `cargo test` that resolves to the
        // test harness rather than to bish (the `current_exe` trap this
        // codebase documents elsewhere). Verified by hand against the
        // real binary, where `exit 3 | exit 4 | exit 5` also gives
        // `3 4 5`.
        assert_eq!(run(r#"sh -c 'exit 3' | sh -c 'exit 4' | sh -c 'exit 5'; echo "${PIPESTATUS[@]}""#), "3 4 5\n");
        // A plain command is a one-element pipeline, which is what a
        // script reading `${PIPESTATUS[0]}` after one expects.
        assert_eq!(run(r#"false; echo "${PIPESTATUS[0]}""#), "1\n");
        assert_eq!(run(r#"true; echo "${#PIPESTATUS[@]}""#), "1\n");
        // The whole reason it exists: `$?` is the *last* stage, so a
        // failure anywhere earlier is invisible without this.
        assert_eq!(run(r#"sh -c 'exit 1' | sh -c 'exit 0'; echo "$? ${PIPESTATUS[0]}""#), "0 1\n");
    }

    // `nocasematch` has been in the shopt registry since that registry
    // existed, with nothing at all to act on -- regex.rs could not fold
    // case. Checked against real bash before being asserted here.
    #[test]
    fn nocasematch_makes_the_regex_operator_fold_case() {
        let run = |script: &str| {
            let mut shell = Shell::new();
            let out = capture_output(&mut shell);
            shell.run_source_here(script, "<test>");
            out.borrow().clone()
        };
        assert_eq!(run(r#"[[ HELLO =~ ^hel ]] && echo yes || echo no"#), "no\n");
        assert_eq!(run(r#"shopt -s nocasematch; [[ HELLO =~ ^hel ]] && echo yes || echo no"#), "yes\n");
        assert_eq!(run(r#"shopt -s nocasematch; [[ hello =~ ^[A-Z] ]] && echo yes || echo no"#), "yes\n");
        // Turned back off again really is off again.
        assert_eq!(run(r#"shopt -s nocasematch; shopt -u nocasematch; [[ HELLO =~ ^hel ]] && echo yes || echo no"#), "no\n");
        // Captures still land in BASH_REMATCH.
        assert_eq!(run(r#"shopt -s nocasematch; [[ FooBar =~ (foo)(bar) ]] && echo "${BASH_REMATCH[1]}-${BASH_REMATCH[2]}""#), "Foo-Bar\n");
    }

    #[test]
    fn apply_edits_is_a_named_policy_defaulting_to_scoped() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--lang=rust", "rust-analyzer"])), 0);
        assert_eq!(shell.lsp_servers[0].apply_edits, "scoped", "the default is the one that needs no thought");

        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--apply-edits=always", "gopls"])), 0);
        assert_eq!(shell.lsp_servers[1].apply_edits, "always");
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--apply-edits", "never", "clangd"])), 0);
        assert_eq!(shell.lsp_servers[2].apply_edits, "never");

        // A misspelling is a config error rather than a silent
        // downgrade: "--apply-edits=alwyas" quietly meaning `scoped`
        // is exactly the kind of thing nobody notices until a refactor
        // does nothing.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--apply-edits=sometimes", "x"])), 2);
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--apply-edits"])), 2);
        assert_eq!(shell.lsp_servers.len(), 3);
    }

    // Four flags is enough that insisting on one order means the wrong
    // one silently becomes part of the command to run.
    #[test]
    fn add_takes_its_flags_in_any_order() {
        let mut shell = Shell::new();
        assert_eq!(
            crate::builtins::bish::run_lsp(
                &mut shell,
                &strs(&["add", "--apply-edits=always", "--root=Cargo.toml", "--lang=rust", "--root-cmd=cargo x", "ra", "--stdio"])
            ),
            0
        );
        let server = &shell.lsp_servers[0];
        assert_eq!(server.lang, "rust");
        assert_eq!(server.root_markers, vec!["Cargo.toml".to_string()]);
        assert_eq!(server.root_cmd, "cargo x");
        assert_eq!(server.apply_edits, "always");
        assert_eq!(server.command, vec!["ra".to_string(), "--stdio".to_string()]);
    }

    #[test]
    fn root_markers_are_a_comma_separated_list_in_the_order_given() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--root=Cargo.toml,.git", "rust-analyzer"])), 0);
        assert_eq!(shell.lsp_servers[0].root_markers, vec!["Cargo.toml".to_string(), ".git".to_string()]);
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--root", "go.mod", "gopls"])), 0);
        assert_eq!(shell.lsp_servers[1].root_markers, vec!["go.mod".to_string()]);
        // A flag with nothing usable after it is a config error, not a
        // silently empty marker list that would never match anything.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--root=", "x"])), 2);
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--root"])), 2);
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add"])), 2, "a registration with no command at all");
        assert_eq!(shell.lsp_servers.len(), 2);
    }

    #[test]
    fn an_unknown_lsp_subcommand_is_refused() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["restart"])), 2);
    }

    #[test]
    fn a_child_shell_inherits_the_declared_language_servers() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--lang=rust", "rust-analyzer"]));
        let child = shell.new_virtual_child();
        assert_eq!(child.lsp_servers, shell.lsp_servers);
        assert_eq!(child.next_lsp_id, shell.next_lsp_id, "or the child would hand out an id the parent already used");
    }

    #[test]
    fn removing_a_hook_that_is_not_there_fails() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_hook(&mut shell, &strs(&["rm", "99"])), 1);
        assert_eq!(crate::builtins::bish::run_hook(&mut shell, &strs(&["rm", "nonsense"])), 2);
    }

    // A typo'd event is the mistake this can actually catch, and a hook
    // that never fires is the worst way to find out.
    #[test]
    fn an_unknown_event_is_refused() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "editor:file:prewrite", "__x"])), 2);
        assert!(shell.hooks.is_empty());
        assert_eq!(crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "editor:file:write:pre", "__x"])), 0);
    }

    #[test]
    fn a_hook_fires_only_for_its_event_and_language() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "--lang=rust", "editor:file:open", "__rust"]));
        crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "--lang", "!(rust)", "editor:file:open", "__other"]));
        crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "editor:file:open", "__any"]));
        crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "--lang=rust", "editor:file:close", "__bye"]));
        assert_eq!(shell.hooks_for("editor:file:open", "rust"), vec!["__rust", "__any"]);
        assert_eq!(shell.hooks_for("editor:file:open", "bash"), vec!["__other", "__any"]);
        assert_eq!(shell.hooks_for("editor:file:close", "rust"), vec!["__bye"]);
        assert!(shell.hooks_for("editor:file:write:pre", "rust").is_empty());
    }

    // Order is registration order, because a config that adds two hooks
    // for one event means them to run in the order it wrote them.
    #[test]
    fn hooks_fire_in_the_order_they_were_added() {
        let mut shell = Shell::new();
        for name in ["__first", "__second", "__third"] {
            crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "editor:file:open", name]));
        }
        assert_eq!(shell.hooks_for("editor:file:open", "bash"), vec!["__first", "__second", "__third"]);
    }

    // A split window should behave like the one it was split from.
    #[test]
    fn a_child_shell_inherits_the_hooks() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "editor:file:open", "__setup"]));
        let child = shell.new_virtual_child();
        assert_eq!(child.hooks_for("editor:file:open", "bash"), vec!["__setup"]);
    }

    #[test]
    fn a_command_with_arguments_is_kept_whole() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "editor:file:open", "lsp", "start", "--quiet"]));
        assert_eq!(shell.hooks[0].command, "lsp start --quiet");
    }

    // The one thing a parallel table gets wrong -- see the identical
    // guard on BISHOPT_HELP.
    #[test]
    fn every_hook_event_is_described() {
        let mut events: Vec<&str> = HOOK_EVENTS.to_vec();
        let mut described: Vec<&str> = HOOK_EVENT_HELP.iter().map(|(e, _)| *e).collect();
        events.sort_unstable();
        described.sort_unstable();
        assert_eq!(events, described, "HOOK_EVENTS and HOOK_EVENT_HELP disagree");
    }

    // The naming rule the whole scheme rests on: every internal node is
    // a real prefix, so `editor:file:write` names both write moments and
    // `shell:exec` will name both exec moments the day there are two.
    #[test]
    fn the_event_hierarchy_is_prefix_matchable() {
        let with_prefix = |prefix: &str| HOOK_EVENTS.iter().filter(|e| e.starts_with(prefix)).count();
        assert_eq!(with_prefix("editor:file:write"), 2, "pre and post are one event's two moments");
        assert_eq!(with_prefix("editor:file"), 4);
        assert_eq!(with_prefix("editor:"), 4);
        assert_eq!(with_prefix("shell:exec"), 2, "and now shell:exec really does name two");
        assert_eq!(with_prefix("shell:"), 3);
        // ...which is exactly what a `prewrite`/`postwrite` spelling
        // would have cost: nothing would share a `write` prefix.
        assert!(HOOK_EVENTS.iter().all(|e| !e.contains("prewrite") && !e.contains("postwrite")));
    }

    // A hook is not a command the user ran, so it must not become the
    // answer to their next `$?`.
    #[test]
    fn last_status_can_be_put_back_after_a_hook_runs() {
        let mut shell = Shell::new();
        shell.set_last_status(3);
        let saved = shell.last_status();
        // What running a hook does to it.
        shell.set_last_status(0);
        shell.set_last_status(saved);
        assert_eq!(shell.last_status(), 3);
    }

    #[test]
    fn hook_help_lists_every_event() {
        let help = hook_help().join("\n");
        for event in HOOK_EVENTS {
            assert!(help.contains(event), "{event} is missing from the help");
        }
        assert!(help.contains("--lang"), "and it says what --lang is");
    }

    #[test]
    fn shell_events_are_registrable_and_scoped_like_the_rest() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "shell:exec:pre", "__timer"])), 0);
        assert_eq!(crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "shell:cwd:change", "__ls"])), 0);
        assert_eq!(shell.hooks_for("shell:exec:pre", "bash"), vec!["__timer"]);
        assert_eq!(shell.hooks_for("shell:cwd:change", "bash"), vec!["__ls"]);
    }

    // A hook that causes its own event has to fire once, not forever.
    #[test]
    fn a_hook_cannot_trigger_more_hooks_while_it_runs() {
        let mut shell = Shell::new();
        crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "shell:cwd:change", "__cd_somewhere"]));
        assert_eq!(shell.hooks_for("shell:cwd:change", "bash").len(), 1);
        shell.set_firing_hooks(true);
        assert!(shell.hooks_for("shell:cwd:change", "bash").is_empty(), "nothing fires while a hook runs");
        shell.set_firing_hooks(false);
        assert_eq!(shell.hooks_for("shell:cwd:change", "bash").len(), 1);
    }
}
