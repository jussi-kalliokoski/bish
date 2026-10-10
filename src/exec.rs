use std::cell::{Cell, RefCell};
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::rc::Rc;

// Shims over `std::process` plumbing that exists only where a child can
// be spawned. On WASI the spawn fails by itself; these keep the code that
// builds commands compiling, with a null stream or an "unsupported" error.
#[cfg(unix)]
fn stdio_from_owned(fd: std::os::fd::OwnedFd) -> Stdio {
    Stdio::from(fd)
}
#[cfg(not(unix))]
fn stdio_from_owned(_fd: std::os::fd::OwnedFd) -> Stdio {
    Stdio::null()
}

#[cfg(unix)]
fn child_stdout_as_owned(out: Option<std::process::ChildStdout>) -> Option<std::os::fd::OwnedFd> {
    out.map(std::os::fd::OwnedFd::from)
}
#[cfg(not(unix))]
fn child_stdout_as_owned(_out: Option<std::process::ChildStdout>) -> Option<std::os::fd::OwnedFd> {
    None
}

#[cfg(unix)]
fn set_command_arg0(command: &mut Command, argv0: &str) {
    command.arg0(argv0);
}
#[cfg(not(unix))]
fn set_command_arg0(_command: &mut Command, _argv0: &str) {}

#[cfg(unix)]
fn exec_replacing_process(command: &mut Command) -> std::io::Error {
    command.exec()
}
#[cfg(not(unix))]
fn exec_replacing_process(_command: &mut Command) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Unsupported, "exec is not available on this platform")
}

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

// Where a Shell's own output (builtins and diagnostics; an external
// process always writes to its inherited fds) goes. `Real` writes to the
// process's stdout and stderr, as before promotion. `Grid` feeds the
// session's `vt100::Screen` once the compositor drives the terminal;
// repl.rs switches a session to it (see `apply_window_action`) and renders
// the focused window from it.
#[derive(Clone)]
pub(crate) enum OutputSink {
    Real,
    Grid(Rc<RefCell<vt100::Screen>>),
    // Command mode swaps a session's sink to this for one command, to show
    // its combined output as an overlay (see `run_command_mode`). No ONLCR:
    // repl.rs splits it on `\n` itself.
    Capture(Rc<RefCell<String>>),
    // Installed by `push_builtin_output_sink` around one builtin, so its
    // output honours that command's own redirects (`>`, `>>`, `2>`, `&>`,
    // `2>&1`, `1>&2`), which guards like `declare -p foo >/dev/null 2>&1` in
    // activation scripts rely on. A stream without its own redirect names the
    // enclosing sink's matching side.
    Builtin { previous: Box<OutputSink>, stdout: SinkStream, stderr: SinkStream },
}

// Where one of a redirected builtin's streams ends up. A dup names the
// descriptor as the command found it, which is why there are three cases:
// in `echo e >&2 2>/dev/null`, `>&2` copies fd 2 before `2>` rebinds it,
// so the line still prints. Pointing at this sink's own stderr would find
// /dev/null; naming the enclosing sink keeps them apart.
#[derive(Clone)]
pub(crate) enum SinkStream {
    // The enclosing sink's stdout: no redirect of its own, or a `2>&1`
    // whose fd 1 this command didn't rebind.
    OuterOut,
    // The enclosing sink's stderr, likewise.
    OuterErr,
    // Where this command's redirects put the stream. Two streams on one
    // destination share the `Rc`, so they share a write position (`>file
    // 2>&1` mustn't overwrite itself) and, for an unmaterialised capture,
    // one buffer.
    File(Rc<Capture>),
}

// Translates LF to CRLF for text going into a Grid, as a terminal's ONLCR
// would. Every `sh_println!` assumes a cooked terminal that inserts `\r`;
// `vt100::Screen` doesn't, because curses programs writing through a pty
// manage `\r` themselves. So the translation happens only at the Grid
// sink.
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
            // Not `print!` (nor `sh_print!`, which bottoms out here and
            // would recurse): `print!` panics on a write error, which
            // happens whenever output is piped into something that closes
            // early (`ulimit -a | head -3`). Every builtin's output comes
            // through here, so a broken pipe is treated as "stop writing",
            // not fatal.
            OutputSink::Real => {
                // Straight to the descriptor, not `std::io::stdout()`,
                // whose buffer returns `Ok` for a write into a full
                // pipeline pipe and surfaces the `EAGAIN` later in `flush`,
                // where it was dropped with the bytes.
                note_write(write_fd_parking(1, s.as_bytes()));
                // Flushed at once: fd 1 isn't this shell's alone, and the
                // next writer is often a child, so a half-line left
                // buffered would come out after the child's output
                // (`printf A; printf B | cat; printf C` printed `BAC`).
                // This adds a syscall only where output stops mid-line.
            }
            OutputSink::Grid(screen) => {
                // A pipeline's pty and this shell write into the same grid,
                // and only this shell writes synchronously. Draining the
                // pty first keeps a stage's output ahead of what the shell
                // prints next.
                drain_pipeline_output();
                screen.borrow_mut().feed(onlcr(s).as_bytes());
            }
            OutputSink::Capture(buf) => buf.borrow_mut().push_str(s),
            OutputSink::Builtin { previous, stdout, .. } => match stdout {
                SinkStream::OuterOut => previous.write_out(s),
                SinkStream::OuterErr => previous.write_err(s),
                SinkStream::File(c) => note_write(c.write_all(s.as_bytes())),
            },
        }
    }

    fn write_err(&self, s: &str) {
        match self {
            OutputSink::Real => {
                use std::io::Write;
                let _ = std::io::stderr().write_all(s.as_bytes());
            }
            OutputSink::Grid(screen) => {
                // The same ordering rule as `write_out`.
                drain_pipeline_output();
                screen.borrow_mut().feed(onlcr(s).as_bytes());
            }
            OutputSink::Capture(buf) => buf.borrow_mut().push_str(s),
            OutputSink::Builtin { previous, stderr, .. } => match stderr {
                SinkStream::OuterOut => previous.write_out(s),
                SinkStream::OuterErr => previous.write_err(s),
                SinkStream::File(c) => {
                    let _ = c.write_all(s.as_bytes());
                }
            },
        }
    }
}

/// Where a construct's standard output goes. A `$( )` starts in memory and
/// gets a descriptor only if the body spawns a process, so `$(pwd)` and
/// `$(echo x)` cost no syscalls, where a memfd, dup, lseek, read and close
/// cost about as much as the body. A real redirect starts as a file. Once
/// materialised a capture stays so, keeping builtin and process output in
/// order.
pub(crate) struct Capture {
    inner: RefCell<CaptureInner>,
    // Set when no anonymous file was available and a named temporary stands
    // in; removed on drop.
    temp: RefCell<Option<std::path::PathBuf>>,
}

enum CaptureInner {
    Memory(Vec<u8>),
    Fd(std::fs::File),
}

impl Capture {
    /// A capture with no descriptor yet, which may never need one.
    fn memory() -> Rc<Capture> {
        Rc::new(Capture { inner: RefCell::new(CaptureInner::Memory(Vec::new())), temp: RefCell::new(None) })
    }

    /// A destination that's already a real file: a redirect, or a pipe.
    fn file(f: std::fs::File) -> Rc<Capture> {
        Rc::new(Capture { inner: RefCell::new(CaptureInner::Fd(f)), temp: RefCell::new(None) })
    }

    /// This shell's own writes.
    fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
        match &mut *self.inner.borrow_mut() {
            CaptureInner::Memory(buf) => {
                buf.extend_from_slice(bytes);
                Ok(())
            }
            CaptureInner::Fd(f) => write_all_parking(f, bytes),
        }
    }

    /// A descriptor for what needs one: a spawned process, or a `2>&1` naming
    /// fd 1. The buffer goes into a real file first, so nothing is lost or
    /// reordered.
    fn descriptor(&self) -> Option<std::fs::File> {
        let mut inner = self.inner.borrow_mut();
        if let CaptureInner::Memory(buf) = &*inner {
            let (file, path) = match capture_file() {
                Some(f) => (f, None),
                // No anonymous file: a named temporary, which this Capture
                // removes on drop.
                None => {
                    // Read and write, since `take_text` reads back through
                    // this descriptor; O_EXCL, so a name planted in advance
                    // is never opened (see `exclusive_temp_file`).
                    let (f, p) = crate::platform::exclusive_temp_file("procsub")?;
                    (f, Some(p))
                }
            };
            let mut file = file;
            if write_all_parking(&mut file, buf).is_err() {
                return None;
            }
            *self.temp.borrow_mut() = path;
            *inner = CaptureInner::Fd(file);
        }
        match &*inner {
            CaptureInner::Fd(f) => f.try_clone().ok(),
            CaptureInner::Memory(_) => None,
        }
    }

    /// Everything written, for whoever asked for the capture.
    fn take_text(&self) -> String {
        let mut inner = self.inner.borrow_mut();
        match &mut *inner {
            CaptureInner::Memory(buf) => String::from_utf8_lossy(&std::mem::take(buf)).into_owned(),
            CaptureInner::Fd(f) => match f.try_clone() {
                Ok(f) => read_capture(f),
                Err(_) => String::new(),
            },
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(p) = self.temp.borrow().as_ref() {
            let _ = std::fs::remove_file(p);
        }
    }
}

// What an external command's stdin and stdout default to when nothing
// redirects them, used at every `Stdio::inherit()` fallback
// (`spawn_stdin_stdio`, `spawn_stdout_stdio`). Only set for a Shell made by
// `run_in_child_shell` (a subshell or substitution running in-process),
// whose commands would otherwise inherit the real process's fds instead
// of the construct's. `None` means the real fds, as `Stdio::inherit()`.

// `run_in_child_shell`'s redirect and capture argument. `Default` means
// no redirect of its own: inherit the parent's destinations.
#[derive(Default)]
struct ChildStdio {
    stdout: Option<Rc<Capture>>,
    stdin: Option<std::fs::File>,
    stderr: Option<std::fs::File>,
    // `2>&1`/`1>&2`: which stream follows the other, and what it followed
    // when the dup appeared. A descriptor names what it names then: `{ echo
    // e; } >&2 2>/dev/null` prints, since `>&2` copied fd 2 before `2>`
    // rebound it, while `{ echo e; } 2>/dev/null >&2` prints nothing.
    out_follows_err: Option<Follows>,
    err_follows_out: Option<Follows>,
}

// What a `2>&1`/`1>&2` pointed at when it appeared.
#[derive(Clone, Copy)]
enum Follows {
    // The file this command had already opened for the other stream.
    OwnFile,
    // The enclosing shell's descriptor: the dup came first.
    Outer,
}

impl ChildStdio {
    // Whether either output stream is redirected. When not, the child's sink
    // is exactly the parent's rather than a wrapper.
    fn redirects_output(&self) -> bool {
        self.stdout.is_some() || self.stderr.is_some() || self.out_follows_err.is_some() || self.err_follows_out.is_some()
    }

    fn sink_stdout(&self) -> SinkStream {
        match self.out_follows_err {
            Some(Follows::Outer) => SinkStream::OuterErr,
            Some(Follows::OwnFile) => file_stream(&self.stderr).unwrap_or(SinkStream::OuterErr),
            None => capture_stream(&self.stdout).unwrap_or(SinkStream::OuterOut),
        }
    }

    fn sink_stderr(&self) -> SinkStream {
        match self.err_follows_out {
            Some(Follows::Outer) => SinkStream::OuterOut,
            Some(Follows::OwnFile) => capture_stream(&self.stdout).unwrap_or(SinkStream::OuterOut),
            None => file_stream(&self.stderr).unwrap_or(SinkStream::OuterErr),
        }
    }
}

// A second handle on the same open file: `try_clone` dups, so the two
// share a write position.
fn file_stream(f: &Option<std::fs::File>) -> Option<SinkStream> {
    f.as_ref()?.try_clone().ok().map(|f| SinkStream::File(Capture::file(f)))
}

// The same for a stream with a capture: sharing the `Rc` keeps one
// write position and one buffer.
fn capture_stream(c: &Option<Rc<Capture>>) -> Option<SinkStream> {
    Some(SinkStream::File(Rc::clone(c.as_ref()?)))
}

/// What a virtual child is being asked to run: text that still has to
/// be lexed and parsed, or a command that already has been.
enum ChildBody<'a> {
    // Text that is its own parse unit: a `$( )` or `<( )` body, which
    // bash reads separately from the source around it.
    Source(&'a str),
    // Text from a tree the enclosing source already read: a `( )` body.
    // bash read it with its surroundings, so an alias defined inside doesn't
    // apply part-way through (`( alias hi=x<newline>hi )` doesn't expand,
    // where in a `$( )` it does). See `run_program_rereadable`.
    Enclosed(&'a str),
    Parsed(&'a parser::Command),
}

/// Process-global state a construct disturbed, and its value before.
/// The cwd, umask and fds 0/1/2 belong to the process, and a `( )` or
/// `$( )` isn't one, so it puts back what was moved. `cd`, `umask` and a
/// bare `exec` record the old value the first time they run (the value
/// from before the construct began), so a body that changes nothing pays
/// nothing.
#[derive(Default)]
struct ProcessRestore {
    cwd: Option<std::path::PathBuf>,
    umask: Option<u32>,
    fd012: Option<[i32; 3]>,
}

struct StdioOverride {
    // `Some`: read from this shared reader instead of the real stdin (see
    // `SharedReaderState`).
    stdin: Option<Rc<RefCell<SharedReaderState>>>,
    // `Some`: write here instead of the real stdout.
    stdout: Option<Rc<Capture>>,
    // And stderr, so a construct's `2>` reaches the externals inside it, not
    // only its builtins: `{ /bin/ls /nosuch; } 2>/dev/null` is silent.
    stderr: Option<std::fs::File>,
}

// Backs a converted construct's stdin override: the file plus bytes read
// from it but not yet consumed. As with `read_input_source`, a fresh
// BufReader per call would drop read-ahead between separate `read` calls
// in one `while read` loop; one persistent buffer avoids that.
struct SharedReaderState {
    file: std::fs::File,
    pending: Vec<u8>,
    /// So a read can give a live coroutine a turn instead of blocking on
    /// bytes only this thread produces: `while read ... done < <(cmd)`
    /// reads a pipe a coroutine writes.
    coroutines: Rc<RefCell<crate::scheduler::Scheduler>>,
}

// A per-call BufRead over a SharedReaderState; successive ones share the
// pending bytes, so separate calls act as one reader. Needed because
// `fill_buf` returns a slice borrowed from `&mut self`, which a RefCell
// borrow can't provide, so this keeps a local copy of the available slice
// and reconciles it with the shared state.
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

/// Runs `op` with `fd` non-blocking, then restores the flag. O_NONBLOCK is
/// on the open file description, shared by everything that inherits the
/// descriptor, so leaving it on a stage's pipe made every external that
/// stage spawned fail with `Resource temporarily unavailable`. Set only
/// around this shell's own read or write, it's never set when a child
/// forks; a stage yields only at a park, never mid-syscall.
///
/// Only inside a coroutine, where blocking would take the whole thread;
/// elsewhere it's just `op`.
fn briefly_nonblocking<T>(fd: i32, op: impl FnOnce() -> T) -> T {
    // A coroutine must never block, taking every other stage with it; nor
    // must an in-process stage whose pane output only this thread drains (see
    // PIPELINE_DRAIN).
    if !crate::coroutine::in_coroutine() && !pipeline_drain_armed() {
        return op();
    }
    let Some(was) = crate::platform::make_nonblocking(fd) else { return op() };
    let out = op();
    crate::platform::restore_flags(fd, was);
    out
}

/// The pty a pipeline in a pane writes into, and its grid. A pipeline's
/// processes inherit the streams nobody redirected, which in a pane is the
/// real terminal underneath the compositor, so `ls | grep x` printed
/// nothing in a split window. Here the last stage's stdout and every
/// stage's stderr point at the slave, and the master is drained into the
/// grid.
struct PaneOutput {
    master: std::fs::File,
    screen: Rc<RefCell<crate::vt100::Screen>>,
    /// One slave descriptor held open and never written. Without it the
    /// master returns EIO once the last stage closes its copy, before its
    /// output has been read; with it an empty pty reads as "nothing yet".
    _keepalive: std::fs::File,
}

impl PaneOutput {
    /// Everything waiting, into the grid, without blocking: the master is
    /// non-blocking, and `Err` covers both "nothing yet" and Linux's EIO once
    /// every slave is closed.
    fn drain(&mut self) {
        use std::io::Read;
        // `try_borrow_mut`, since this runs inside the grid's own write
        // path: if the screen is already borrowed up the stack, leave the
        // bytes for next time.
        let Ok(mut screen) = self.screen.try_borrow_mut() else { return };
        let mut buf = [0u8; 4096];
        loop {
            match self.master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => screen.feed(&buf[..n]),
                Err(_) => break,
            }
        }
    }
}

thread_local! {
    /// Armed while this thread runs something writing into a pipeline it
    /// also drains. The in-process stage runs inline, so nothing empties
    /// the pane's pty meanwhile; if the upstream stage overfills its pipe
    /// and the downstream one the pty, both stop. Draining from inside the
    /// blocking write prevents that hang.
    static PIPELINE_DRAIN: RefCell<Option<PaneOutput>> = const { RefCell::new(None) };
}

/// Whether a write that would block should drain the pty and retry
/// instead of waiting in the kernel.
fn pipeline_drain_armed() -> bool {
    PIPELINE_DRAIN.with(|d| d.borrow().is_some())
}

/// One pass of the armed drain; `false` if none is armed, and the caller
/// has to block.
pub(crate) fn drain_pipeline_output() -> bool {
    PIPELINE_DRAIN.with(|d| match d.borrow_mut().as_mut() {
        Some(output) => {
            output.drain();
            true
        }
        None => false,
    })
}

/// A failed write to a redirect target. A broken pipe is remembered: it's
/// how an in-process stage learns its reader has gone, since SIGPIPE can't
/// tell it. Other errors are dropped.
fn note_write(result: std::io::Result<()>) {
    if result.is_err_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) {
        note_broken_pipe();
    }
}

/// `write_all`, except that a full descriptor hands the thread to another
/// pipeline stage instead of failing. Only a pipe between two in-process
/// stages is non-blocking, so elsewhere this is `write_all`; there,
/// `WouldBlock` means the reader, a coroutine, is behind, so let it run.
pub(crate) fn write_all_parking(file: &mut std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    write_fd_parking(file.as_raw_fd(), bytes)
}

/// How many turns a `<( )` producer gets to notice nobody reads it any
/// more, after its command finishes.
const PROC_SUB_WINDDOWN_TURNS: usize = 8;

/// A file this shell reads that gives a live coroutine a turn instead of
/// blocking: in `read v < <(echo hello)` the name is a pipe a coroutine
/// writes, and a blocking read would wait for bytes only this thread can
/// produce. With nothing live it's a plain read with one `poll` first.
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
                // Nothing can produce anything, so waiting in the kernel is
                // right.
                drop(scheduler);
                return self.file.read(buf);
            }
            scheduler.step();
        }
    }
}

/// `write_all_parking` onto a descriptor.
fn write_fd_parking(fd: i32, bytes: &[u8]) -> std::io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        let wrote = briefly_nonblocking(fd, || crate::platform::write_bytes(fd, &bytes[written..]));
        let e = match wrote {
            Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero)),
            Ok(n) => {
                written += n;
                continue;
            }
            Err(e) => e,
        };
        match e.kind() {
            // Nothing downstream can take more yet. A coroutine parks; an
            // in-process stage drains the pane's pty, which the downstream
            // process is waiting on, and retries.
            std::io::ErrorKind::WouldBlock => {
                if !drain_pipeline_output() {
                    crate::scheduler::park_writable(fd);
                }
            }
            std::io::ErrorKind::Interrupted => {}
            _ => return Err(e),
        }
    }
    // The write succeeded, so downstream has something to read: hand it the
    // thread. A stage otherwise yields only when it blocks, and a producer
    // whose reader keeps up never blocks, so a `while true; do echo x; done`
    // producer ran until the pipe filled before its reader's first turn. The
    // kernel preempts processes; a cooperative scheduler is told here.
    if crate::coroutine::in_coroutine() {
        crate::scheduler::park_ready();
    }
    Ok(())
}

/// `read`, likewise: nothing to read means a stage, a coroutine, hasn't
/// written it yet.
fn read_parking(file: &mut std::fs::File, buf: &mut [u8], coroutines: &Rc<RefCell<crate::scheduler::Scheduler>>) -> std::io::Result<usize> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    loop {
        // Inside a stage: hand the thread back to the scheduler above us.
        if crate::coroutine::in_coroutine() {
            match briefly_nonblocking(fd, || file.read(buf)) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => crate::scheduler::park_readable(fd),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                other => return other,
            }
            continue;
        }
        // The shell itself reading what a coroutine feeds; blocking would
        // wait for bytes only this thread produces.
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
        // Hand back what this call fetched but didn't consume, so the next
        // reader, the next `read` in the loop, continues from there.
        if !self.local.is_empty() {
            let mut state = self.state.borrow_mut();
            let mut rest = std::mem::take(&mut self.local);
            rest.append(&mut state.pending);
            state.pending = rest;
        }
    }
}

// `print!` and friends through a Shell's sink instead of the real
// stdout/stderr. `$self` needs `sink_out`/`sink_err`; the arguments go to
// `format!` unchanged.
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
    // A `window` command's result, bubbling up like Break and Return to
    // whoever called `run_program`: repl.rs, which owns the sessions and
    // windows. A session's Shell can't hold all sessions, itself included,
    // without a RefCell cycle that panics when borrowed twice, so the action
    // travels as a signal and `run_window` never touches window state.
    Window(WindowAction),
    // `fg` on a pty-attached job (see `Job::pty_master`) while promoted.
    // Bubbles up like Window, since rendering the job needs repl.rs's state.
    // `run_fg` moves the job to `pending_fg`; repl.rs takes it with
    // `take_pending_fg`, pushes it as a `Frame::Job` and drives it
    // (`drive_fg_job`), owning it across loop iterations.
    Fg,
    // `e [ARG...]`, bubbling up like Fg: a builtin has no raw mode, keys or
    // rendering, and `Registers` lives in `repl::run`. `ExecResult` stays
    // `Copy`, so the arguments go to `pending_edit` and repl.rs takes them
    // with `take_pending_edit`.
    Edit,
    // `exit`, a failure under `set -e`, a `set -u` violation, or a failed
    // `exec`: ending this Shell's top-level run, not necessarily the
    // process. Bubbles up like the other signals, and the outermost caller
    // decides: the real top level (main.rs's `run_source`, repl.rs) exits
    // the process, while `run_in_child_shell` takes it as that child's
    // status, so `(exit 3)` doesn't end the shell, as with bash's fork. The
    // EXIT trap has already run.
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

// What a paused DebugHook does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugAction {
    // Run to the next breakpoint or step condition.
    Continue,
    // Stop at the next statement at this depth or shallower, not inside a
    // function or subshell entered meanwhile.
    StepOver,
    // Stop at the next statement, at any depth.
    StepInto,
    // Unwind the whole program, like `exit` (see `ExecResult::Exit`), to
    // abandon a debug run without ending the process.
    Quit,
}

// Where the interpreter is, for stepping. Compares `subshell_depth`, then
// `call_depth`: without the first, a subshell starting its own
// `run_program` at call depth 0 would alias the parent's top level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DebugDepth {
    pub subshell_depth: u32,
    pub call_depth: usize,
}

// Called before each top-level `ListItem` that `run_program` runs, at
// every depth (compound bodies recurse through `run_program`). Defined
// with exec.rs types so the debugger, which owns breakpoints, rendering
// and key reads, depends on exec.rs and not the reverse.
pub trait DebugHook {
    // `&mut Shell`: when a statement should run uninterrupted, the hook
    // hands the terminal to the script (`PauseState::hand_off_to_script`,
    // via `set_sink_real`), so `read -p`'s prompt shows at once, and takes
    // it back when it's the hook's turn again.
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

/// One declared theme: the bishopts and highlight colours it sets. Two
/// maps, since `::bish hl` names are open and bishopt names a fixed
/// registry, but one type, since a theme is one thing you switch to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Theme {
    pub(crate) opts: std::collections::HashMap<String, BishOptValue>,
    pub(crate) hl: std::collections::HashMap<String, String>,
}

/// Whatever a shell's output sink was before something borrowed it,
/// opaque so `OutputSink` stays private.
pub(crate) struct SavedSink(OutputSink);

/// One registered hook: what to run, when, and for which languages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    /// Assigned from a per-shell counter and never reused, so `rm`
    /// always names the thing `ls` showed.
    pub id: u64,
    pub event: String,
    /// `--lang=`: a glob matched against the language of the event's file
    /// (`fileeditor::language_of`), as `abbr --lang` matches:
    /// `--lang='*script'` covers a family, `--lang='!(bash)'` all but one.
    /// `*` when unscoped.
    pub lang: String,
    /// The command line to run, with the file's path appended as its
    /// first argument.
    pub command: String,
}

/// Long-running helper processes the shell can report on and manage
/// without knowing what they are: entries have an id, fields to print and
/// a recent log, and can be dropped. `::bish lsp` is a builtin, readable
/// in `$(...)`, but running a language server isn't the shell's
/// business. `lspclient::Table` implements it. The row fields are plain
/// strings so the shell needn't know their shape.
pub trait ServiceTable {
    /// One row per entry, in the order to print, each already split
    /// into the fields `status` will join with tabs.
    fn rows(&self) -> Vec<Vec<String>>;

    /// Recent output from the entries started from declaration `id`, oldest
    /// first: what a helper that failed to start said. Empty when there's
    /// nothing.
    fn logs(&self, id: u64) -> Vec<String>;

    /// Forgets every entry started from declaration `id`, including a
    /// remembered failure to start, so the next thing that needs one
    /// gets a fresh attempt. Returns how many were dropped.
    fn forget(&mut self, id: u64) -> usize;
}

/// The interactive history, as much as a builtin needs. It lives in
/// `repl::SessionState`, since it belongs to a session at a prompt and a
/// script has none; like `ServiceTable`, exec.rs owns the trait and
/// repl.rs implements it.
pub trait HistoryAccess {
    /// Every entry, oldest first, with when it was recorded if known.
    /// Callers print the position plus one, as bash numbers from 1. The
    /// time is `None` for entries written before the file recorded times,
    /// and for ones rebuilt by `delete` (see history.rs); `HISTTIMEFORMAT`
    /// then shows nothing rather than a wrong time.
    fn entries(&self) -> Vec<(String, Option<i64>)>;

    /// Drops everything: `history -c`.
    fn clear(&mut self);

    /// Drops entry `n` (1-based); `false` if there's none.
    fn delete(&mut self, n: usize) -> bool;
}

/// Before a real history is installed, and for every non-interactive
/// shell, which has none.
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

/// Before a real table is installed, and for every non-interactive shell,
/// which never starts a language server.
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
/// how to find the project root. A declaration only: `repl::run` owns the
/// running servers (keyed by command and root, so one serves every pane on
/// a project); this is what a bishrc declares. `lspclient::Server` is the
/// other half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspServer {
    /// From a per-shell counter, never reused, as `Hook::id`.
    pub id: u64,
    /// `--lang=`: a glob over the edited file's language
    /// (`fileeditor::language_of`), as for `hook` and `abbr`. `*` when
    /// unscoped.
    pub lang: String,
    /// The command line to run, split into words at registration time.
    pub command: Vec<String>,
    /// `--root=`: file names marking a project's top, tried in order
    /// walking up from the file. `.git` by default, as
    /// `gitignore::Stack::for_directory` uses.
    pub root_markers: Vec<String>,
    /// `--root-cmd=`: a command printing the project root on its first line,
    /// run in the file's directory; usually empty. Some roots only the build
    /// tool knows (a Cargo workspace member belongs at the workspace, which
    /// `cargo metadata` knows), and a generic command lets each language
    /// answer without per-language code here.
    pub root_cmd: String,
    /// `--apply-edits=`: how much a server may change files on its own. A
    /// server can send `workspace/applyEdit` at any time, which is how
    /// command code actions work, and also how one could rewrite a buffer
    /// uninvited. `scoped` (default) accepts only while a user-chosen
    /// command runs; `never` always refuses; `always` always accepts, as VS
    /// Code does.
    pub apply_edits: String,
    /// `--setting KEY=VALUE`, repeatable: the server's configuration. Kept
    /// as flat `dotted.key=value` pairs, as `ls` prints them; the nesting a
    /// server expects is built when sending. Values are JSON if they parse,
    /// else strings, so `x.enable=true` is a boolean and
    /// `x.path=/usr/bin/foo` a string without quoting through two levels of
    /// shell.
    pub settings: Vec<(String, String)>,
}

/// One `--setting` pass: the pair, if the flag was first, and the rest of
/// the arguments.
type ParsedSetting<'a> = (Option<(String, String)>, &'a [String]);

impl LspServer {
    /// The command as one line for `ls` and `status`, quoting only words that
    /// would otherwise split.
    pub fn command_line(&self) -> String {
        let plain = |w: &String| !w.is_empty() && w.chars().all(|c| c.is_alphanumeric() || "-_./=:+@,".contains(c));
        self.command.iter().map(|w| if plain(w) { w.clone() } else { shell_quote(w) }).collect::<Vec<_>>().join(" ")
    }
}

/// Every event a hook can attach to.
///
/// `pre`/`post` is a segment, not a prefix (`editor:file:write:pre`), so
/// the hierarchy stays prefix-matchable: `editor:file:write` and
/// `editor:file` can be globbed. `open` and `close` have one useful moment
/// each: `open` once the buffer exists, `close` before it goes. The
/// `editor:` namespace leaves room for `shell:` events.
pub const HOOK_EVENTS: &[&str] = &[
    "editor:file:open",
    "editor:file:write:pre",
    "editor:file:write:post",
    "editor:file:close",
    "shell:exec:pre",
    "shell:exec:post",
    "shell:cwd:change",
];

/// `::bish hook help`: usage and every event, shared with `:help hooks`
/// (repl.rs).
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

/// One line per event: when it fires and its argument. A parallel table
/// guarded by `every_hook_event_is_described`, like `BISHOPT_HELP`.
pub const HOOK_EVENT_HELP: &[(&str, &str)] = &[
    ("editor:file:open", "A file has been opened in the editor and its options are resolved. Argument: the path."),
    ("editor:file:write:pre", "A file is about to be written. Argument: the path."),
    ("editor:file:write:post", "A file has been written to disk. Argument: the path."),
    ("editor:file:close", "A file is about to be closed, while its buffer still exists. Argument: the path."),
    ("shell:exec:pre", "A command line is about to run at the prompt. Argument: the command line."),
    ("shell:exec:post", "A command line has finished. Argument: the command line; `$?` is its exit status."),
    ("shell:cwd:change", "The working directory has changed, however it happened. Argument: the new directory."),
];

/// Where `::bish window move` puts the current window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowMove {
    /// One place towards the front of the tab bar, if not already first.
    Left,
    /// One place towards the back, likewise.
    Right,
    /// A 1-based position, clamped: `move 1` makes this the first tab.
    To(usize),
}

// What repl.rs should do for a `window` command, returned rather than
// done here; see `ExecResult::Window`.
#[derive(Debug, Clone)]
pub enum WindowAction {
    /// `window create [--name NAME] [--] [COMMAND...]`. The name shows in the
    /// tab bar instead of the cwd and is what `window select` finds; the
    /// command runs in the new window's shell as if typed there.
    New {
        name: Option<String>,
        command: Option<String>,
    },
    /// `window rename [NAME]`: the current window; no name clears it.
    Rename(Option<String>),
    /// `::bish window select NAME|ID`, produced only once the target is found
    /// in `Shell::windows`: a miss is an ordinary status 1, so
    /// `select || create` works anywhere.
    Select(usize),
    Next,
    Previous,
    Close,
    /// `::bish window minimize`/`_` and `<C-w>_`: shrinks the focused pane to
    /// its divider and focuses a neighbour; focusing it restores it.
    Minimize,
    // `window fg <window-id>`: puts the target window's top frame on this
    // window's stack too, one session shown in two windows (see
    // `WindowEntry::stack`). An id, since indices shift as windows close.
    FgSession(u32),
    // `window split`/`s` and `window vsplit`/`v`: divides the focused pane,
    // the new half holding a cloned session and taking focus (see repl.rs's
    // `PaneLayout`).
    //
    // `horizontal` is the divider line's orientation, so `true` stacks the
    // panes. The commands are named by how the panes end up, so `vsplit`
    // (stacked) is `horizontal: true` and `split` (side by side) is
    // `horizontal: false`; see builtins/bish.rs.
    Split {
        horizontal: bool,
        // A command to run in the new pane's shell as if typed there.
        command: Option<String>,
    },
    // `window h/left`, `j/below`, `k/above`, `l/right`: focus the nearest
    // pane that way, as vim's `<C-w>hjkl`. A no-op without one.
    FocusPane(PaneDirection),
    /// `::bish window focus <n>`: the nth pane in layout order, from 1, which
    /// a saved layout uses to return to a pane.
    FocusPosition(usize),
    /// `::bish window save [file]`: writes the `::bish window` script that
    /// rebuilds every window, pane and directory; without a file, prints it.
    Save(Option<String>),
    /// `::bish window restore <file>`: runs such a script line by line, as
    /// typed.
    Restore(String),
    /// `::bish window move <left|right|N>`: moves the current window in the
    /// tab bar. No `<C-w>` key: `<`/`>` mean narrower and wider in vim.
    MoveWindow(WindowMove),
    /// `::bish window layout <name>` or `layout next`, and `<C-w><Space>`:
    /// rearranges the window's panes into a named layout; `None` is the next
    /// one (see `window::NamedLayout`).
    SetLayout(Option<crate::window::NamedLayout>),
    /// `::bish window swap`/`x` and `<C-w>x`: exchanges the focused pane with
    /// the next, wrapping. Focus follows the pane, as in vim.
    SwapPane,
    /// `::bish window break` and `<C-w>T`: moves the focused pane into a new
    /// window of its own.
    BreakPane,
    /// `::bish window join <window-id>`/`vjoin`: the reverse, bringing that
    /// window's focused pane here beside the focused one; an emptied window
    /// goes away. `horizontal` reads as for `Split`.
    JoinPane {
        from: u32,
        horizontal: bool,
    },
    /// `::bish window zoom`/`z` and `<C-w>z`/`<C-w>o`: the focused pane fills
    /// the window until toggled back. The split tree is untouched, so
    /// unzooming restores it (see `WindowEntry::zoomed`).
    Zoom,
    // `window =`/`balance`: every pane's size weight back to even.
    Balance,
    // `window +`/`sizeup` and `-`/`sizedown`: grow or shrink the focused
    // pane's share of its parent split by a step. A no-op unsplit.
    SizeUp,
    SizeDown,
    // `window size <N>`, `<N>%`, `<N>/<M>`: sets the focused pane's size
    // along its parent split's axis (see `SizeSpec`). A no-op unsplit.
    SetSize(SizeSpec),
}

// `window size`'s argument: a bare integer is a count of characters
// along the parent split's axis, `N%` a percentage of it, `N/M` a
// fraction. All become a target fraction converted to a size weight
// relative to the siblings (see repl.rs's `set_focused_pane_size`), since
// panes have to keep resizing with the terminal.
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

// Parses `window size`'s argument: `N%`, `N/M` (two integers, not
// arithmetic) or `N`. Invalid input, `/0` included, is `None`, for one
// consistent usage error.
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

// Per name a call frame shadowed: the name, the value it displaced
// (`None` if none), and whether it had the array attribute, all restored
// on return so `local -a x` doesn't leave `x` an array.
type ArrayShadowStack<V> = Vec<Vec<(String, Option<V>, bool)>>;

pub struct Shell {
    pub last_status: i32,
    pub(crate) functions: HashMap<String, parser::Command>,
    /// How far into the current word `getopts` has read, 0 before starting
    /// one. `-ab` is two options and OPTIND names the word, so the position
    /// lives here, hidden as in bash; assigning OPTIND resets it (see
    /// `assign_var_impl`).
    pub(crate) getopts_offset: usize,
    /// How many `.`/`source` calls are running. `return` in a sourced file
    /// ends the file, not the shell, the usual guard clause at a library's
    /// top.
    pub(crate) source_depth: usize,
    /// What `$_` becomes when the running command finishes. A stack, since
    /// commands nest: `f arg` records `arg` and the body's commands push
    /// their own, so a body never decides its caller's `$_`.
    pub(crate) underscore_pending: Vec<String>,
    // Positional-parameter frames; `last()` is the current one. `$0` is
    // tracked separately.
    pub(crate) arg_frames: Vec<Vec<String>>,
    // `local` overlays, empty outside functions. Only names `local` declared
    // live here; plain assignment targets the global unless a local of that
    // name exists, as in bash. `None` is a name declared local and unset:
    // `local x` shadows the caller's, and `${x-default}` and `set -u` must
    // tell that from an empty string.
    pub(crate) var_scopes: Vec<HashMap<String, Option<String>>>,
    script_name: String,
    // Whether this shell runs a named script rather than `-c` text, which
    // decides whether the call stack has an outer `main` frame (see
    // `refresh_call_arrays`).
    pub running_a_script: bool,
    // Indexed arrays. A BTreeMap so arrays are sparse as in bash:
    // `arr[10]=x` doesn't create 0..9, and `${#arr[@]}` counts what's set.
    // One flat map rather than a scope stack, since only `local -a`/`-A`
    // need scoping (see `array_local_stack`).
    pub(crate) arrays: HashMap<String, std::collections::BTreeMap<usize, String>>,
    // Associative arrays (`declare -A`), apart from `arrays` since their keys
    // are strings. Names in `assoc_names` are looked up here.
    pub(crate) assoc_arrays: HashMap<String, OrderedMap>,
    // `c`, `s` or `i`: how this shell was invoked, the last character of
    // `$-`. `None` for a named script.
    pub invocation_flag: Option<char>,
    // Names this shell explicitly unset, so the real-environment fallback
    // the lookups end in doesn't hand the value back. That fallback is for
    // names set behind the shell's back after startup (the session
    // bridge's `XDG_RUNTIME_DIR`, `TZ` in a test); everything inherited
    // is seeded into `globals` at startup.
    unset_names: std::collections::HashSet<String>,
    // The array attribute, apart from the values: `declare -a A` leaves A
    // unset (bash prints `declare -a A`, where `A=()` prints
    // `declare -a A=()`). A name is an array if it's in either half of its
    // pair. The attribute makes a later `A=x` mean `A[0]=x`.
    pub(crate) array_names: std::collections::HashSet<String>,
    pub(crate) assoc_names: std::collections::HashSet<String>,
    // `alias name=value`, expanded by `lexer::expand_aliases`: a
    // command-position word is replaced by its value's tokens before the
    // parser sees them, so an alias whose value is a pipeline is a
    // pipeline. Gated on `shopt -s expand_aliases`, as in bash, which is
    // off for scripts; repl.rs turns it on interactively. A Vec, since bash
    // lists aliases in definition order.
    pub(crate) aliases: Vec<(String, String)>,
    // `abbr -a NAME EXPANSION`: abbreviations, expanded by editor.rs's
    // `read_line` as you type (Space or Enter after NAME in command
    // position), never here; one with `%s` placeholders becomes a snippet
    // (bishedit::snippet). repl.rs snapshots this table for `read_line` at
    // each prompt.
    pub abbrs: Vec<Abbr>,
    /// `::bish map` key remappings, scoped to modes by a glob. Beside the
    /// abbreviations because it's the same kind of table: user-defined,
    /// consulted by the editor, set from config.bash or at runtime.
    pub mappings: Vec<crate::keymap::Mapping>,
    /// `enable -n NAME`: builtins taken out of service, so the external
    /// of the same name runs instead.
    pub(crate) disabled_builtins: std::collections::HashSet<String>,
    /// One entry per function call in progress, oldest first, for `caller`.
    /// Pushed by `call_function`, the only way into a function body.
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
    /// Declared language servers: config, not processes. See `LspServer`.
    pub lsp_servers: Vec<LspServer>,
    pub(crate) next_lsp_id: u64,
    /// The helper processes `::bish lsp` reports on, behind a trait so this
    /// module never names a language-server type (see `ServiceTable`).
    /// Shared by every shell in the process, as `jobs` is, so
    /// `::bish lsp status` works in `$(...)`.
    pub lsp: Rc<RefCell<dyn ServiceTable>>,
    /// The interactive history, when there is one (see `HistoryAccess`),
    /// shared with the session that owns it as `lsp` is.
    pub history: Rc<RefCell<dyn HistoryAccess>>,
    /// Set while a hook runs, so a hook that causes its own event (a
    /// `shell:cwd:change` hook that `cd`s) fires once. Not shared with a
    /// virtual child: a subshell a hook starts keeps its own hooks.
    firing_hooks: bool,
    // One frame per function call. `local -a`/`-A name` saves the array's
    // previous value here (`None` if it didn't exist), and returning
    // restores it, so `arrays`/`assoc_arrays` stay flat.
    array_local_stack: ArrayShadowStack<std::collections::BTreeMap<usize, String>>,
    assoc_local_stack: ArrayShadowStack<OrderedMap>,
    // `declare -n ref=target`: `ref` holds the target's name, and lookups,
    // assignments and `var_is_set` follow it (`resolve_nameref`). Scalars
    // only; `declare -n ref=arr[0]` isn't supported.
    pub(crate) nameref_names: std::collections::HashSet<String>,
    // `local -n` scoping, like `array_local_stack`: per frame, each name
    // made a nameref and whether it already was one, so returning undoes
    // only this function's change. A top-level `declare -n` has no frame
    // and stays, as in bash.
    nameref_local_stack: Vec<Vec<(String, bool)>>,
    // pushd/popd/dirs, without the current directory, which `dirs` prints
    // first. The `+N`/`-N` rotation forms aren't implemented.
    pub(crate) dir_stack: Vec<String>,
    // `shopt -s`/`-u` overrides by name; absent means the default in
    // `KNOWN_SHOPT_OPTIONS` (some are on). Most only need to be accepted,
    // so `shopt -s nullglob` in a script under `set -e` doesn't abort it.
    pub(crate) shopt_options: std::collections::HashMap<String, bool>,
    /// What `$$` reports: the PID of the shell that started, not of the
    /// process asking. POSIX keeps `$$` the same in a subshell, and scripts
    /// rely on it (`kill -HUP -$$` from a background helper); `BASHPID` asks
    /// the kernel. A re-exec'd shell is told it through `REEXEC_SHELL_PID`; a
    /// new shell process gets its own in `Shell::new`.
    pub(crate) shell_pid: u32,
    // `bishopt --set`/`--unset`, bish's own settings, kept apart from
    // `shopt_options` (see `KNOWN_BISHOPTS`). Overrides only: `--unset`
    // removes the entry, so unset and never set are the same state.
    pub(crate) bishopts: std::collections::HashMap<String, BishOptValue>,
    // `::bish theme` declarations: theme name to the bishopt overrides
    // captured while declaring it (see `pending_theme`). When the `theme`
    // bishopt names one, `bishopt_value` uses it below explicit `bishopts`
    // and above `KNOWN_BISHOPTS`' defaults. Cloned into a child like
    // `bishopts`.
    pub(crate) themes: std::collections::HashMap<String, Theme>,
    /// Syntax-highlighting colours by name (see `::bish hl`). A plain map,
    /// not a registry, because the names are open: a language server's
    /// semantic token types needn't be declared first.
    pub(crate) hl: std::collections::HashMap<String, String>,
    // `Some` between `::bish theme begin` and `end`: every `bishopt --set`
    // between is captured here instead of applying, becoming a `themes`
    // entry on `end` (see `run_bish_theme_end`). Reset to `None` in a
    // child, since a declaration in progress belongs to the top level.
    pub(crate) pending_theme: Option<Theme>,
    // `complete NAME` specs by command (see `run_complete`), for `complete
    // -p`, `-r`, `compopt`, and through a per-prompt snapshot for Tab
    // completion (`ShellCompletionProvider`).
    pub(crate) completions: std::collections::HashMap<String, compgen::CompgenSpec>,
    // `complete -D`: the fallback spec used when no exact name matches.
    pub(crate) default_completion: Option<compgen::CompgenSpec>,
    // `readonly NAME`, checked by `assign_var`, which every kind of
    // assignment goes through.
    pub(crate) readonly_names: std::collections::HashSet<String>,
    // `declare -i`/`local -i`: assignments are evaluated as arithmetic (in
    // `assign_var`).
    pub(crate) integer_names: std::collections::HashSet<String>,
    // `declare -u`/`-l`: assignments are case-folded (in `assign_var`).
    pub(crate) upper_names: std::collections::HashSet<String>,
    pub(crate) lower_names: std::collections::HashSet<String>,
    // What this shell changed about the process it shares, and what it was
    // before; see ProcessRestore.
    process_restore: ProcessRestore,
    // Left by a bare `exec` naming the descriptors it moved, taken by
    // `dispatch_builtin_or_external` after the command's redirect sink is
    // popped. The follow belongs on the sink that survives the command; the
    // one installed for the command is removed with it. See
    // `follow_moved_descriptors`.
    descriptors_to_follow: Option<Vec<i32>>,
    // Bumped when the alias table changes, so `run_program` knows to reread
    // the statements after one that defined or removed an alias. See
    // `aliases_mut`.
    alias_epoch: u64,
    // Exported names. Copy-on-write, like `globals`.
    pub(crate) exported_names: Rc<std::collections::HashSet<String>>,
    // `_` lives apart from `globals` because it's written after every simple
    // command, and `globals` is copy-on-write: inside a `$( )`, where it's
    // shared with the parent, each write copied the whole environment, 39% of
    // a substitution's cost. `global_get`, `global_set`, `global_remove` and
    // `global_names` are the only places aware of it.
    underscore: Option<String>,
    // Parsed bodies of `$( )`, `( )` and `<( )`, shared with virtual
    // children (which do the parsing), so a substitution in a loop is
    // parsed once. Keyed by text, sound because lexing and parsing read no
    // shell state except alias expansion; so nothing is cached while alias
    // expansion could apply (see `cached_program`).
    parse_cache: Rc<RefCell<std::collections::HashMap<String, Rc<parser::Program>>>>,
    // The proc-sub temp files for the command being built, deleted when it
    // finishes (`drain_proc_subs`).
    proc_sub_cleanup: Vec<String>,
    /// Coroutines outliving the command that started them, such as a `<( )`
    /// producer. Shared with virtual children like the job table, and run
    /// wherever the shell would otherwise wait in the kernel; see
    /// `pump_coroutines`.
    background_coroutines: Rc<RefCell<crate::scheduler::Scheduler>>,
    /// This shell's ends of the pipes a substitution is on the far side of,
    /// held until the command using them is done.
    proc_sub_pipes: Vec<std::os::fd::OwnedFd>,
    /// Substitutions that are real processes, and whether one still running
    /// may be killed: a `<( )` producer may, a `>( )` consumer may not
    /// (killing `tee` lost its unflushed output).
    proc_sub_children: Vec<(std::process::Child, bool)>,
    // `$RANDOM`: xorshift64* state seeded from the time, advanced per read.
    rng_state: u64,
    // `$SECONDS`: when the shell started. `SECONDS=n` records an offset in
    // `assign_var` rather than rewinding this.
    shell_start: std::time::Instant,
    seconds_offset: i64,
    // Background jobs, enough for `jobs`, `fg`, `bg`, `wait` and `kill %N`.
    // Under `set -m` (opt_monitor), a foreground external command gets its
    // own process group, the terminal (`tcsetpgrp`) and Ctrl-Z stops (see
    // `run_single`'s setpgid hook and `Job::pgid`). A backgrounded pipeline
    // or redirected compound gets a process group, so `kill %N` and `bg`
    // reach every process, but not foreground handling: Ctrl-Z on a
    // foreground pipeline does nothing useful. Self-exec'd subshells and
    // coprocs get no isolation.
    //
    // An `Rc<RefCell<_>>`, since `Child` isn't Clone and sessions sharing
    // job control must point at one table.
    pub(crate) jobs: Rc<RefCell<JobTable>>,
    // `trap CMD SIGNAL` for real signals, installed with sigaction (see
    // `install_trap_handler`), keyed by number as `PENDING_SIGNALS` records
    // them. EXIT is `exit_trap`.
    traps: std::collections::HashMap<i32, TrapAction>,
    // `trap CMD EXIT`. Not a signal, so it runs wherever the shell is about
    // to end.
    exit_trap: Option<String>,
    // `$BASH_COMMAND`, for a DEBUG or ERR trap. bash holds the source text,
    // which needs spans this parser doesn't carry (see the
    // `function-body-formatting` divergence), so this is the command with
    // its words expanded, differing only where it had an expansion. An
    // `Rc<str>` pointing at the command node's text (see
    // `serialize::rendered_command`).
    bash_command: Rc<str>,
    // The subshell depth the EXIT trap was set at: it fires only for that
    // shell's exit (see `run_exit_trap`).
    exit_trap_depth: u32,
    // The other pseudo-signals: DEBUG before each simple command, ERR after a
    // failure under errexit's rules, RETURN on a function's return. Separate
    // fields like `exit_trap`, since `traps` is keyed by real signal numbers.
    debug_trap: Option<String>,
    err_trap: Option<String>,
    return_trap: Option<String>,
    // The call depth each pseudo-trap was set at. It fires only there unless
    // its inheriting option is on, so a RETURN trap set in a function runs
    // when that function returns, without `set -T`.
    pseudo_trap_depth: [usize; 3],
    // Whether a trap body is running: a DEBUG trap's own commands would fire
    // it forever. bash guards the same way.
    in_trap: bool,
    // Whether `PROMPT_COMMAND` is running, so a second call site could never
    // re-enter it.
    in_prompt_command: bool,
    // Whether `command_not_found_handle` is running: a handler mistyping a
    // command would call itself forever.
    in_command_not_found: bool,
    // `set -T` (functrace) makes DEBUG and RETURN inherited by functions;
    // `set -E` (errtrace) does so for ERR. Two options, as in bash. Without
    // them, none of the three is inherited.
    opt_functrace: bool,
    opt_errtrace: bool,
    // How many functions deep we are, which `functrace` is about.
    function_depth: usize,
    // Set when a call was refused for nesting too deeply, cleared once that
    // stack has unwound. Refusing the one call isn't enough: `f() { f; f; }`
    // would call `f` again at every level on the way out, taking exponential
    // time. bash longjmps to the top level; this flag stands in for that.
    nesting_unwind: bool,
    // The shell's ends of `coproc` pipes, by fd, kept open while the
    // coprocess may be used (the coprocess's ends go to the child). A
    // redirect like `cmd <&"${NAME[0]}"` only needs the fd open; `read -u`
    // reads through this table.
    coproc_fds: std::collections::HashMap<i32, KeptFd>,
    // `set -n`, `-o noexec`, `bish -n`: parse, run nothing. Only when not
    // interactive, as in bash: a prompt that ran nothing could never take
    // `set +n`. Once on, nothing turns it off, because nothing runs.
    opt_noexec: bool,
    // `set -e`/`-u`/`-x`/`-o pipefail`/`-f`.
    opt_errexit: bool,
    opt_nounset: bool,
    opt_xtrace: bool,
    opt_pipefail: bool,
    opt_noglob: bool,
    // `set -m`: `fg` and `bg` refuse without it ("no job control"), as in
    // bash, which has it off in scripts. `jobs`, `wait` and `kill` don't
    // care.
    pub(crate) opt_monitor: bool,
    // `set -r`, bash's restricted mode (unrelated to
    // `restrict_to_builtins`). No `-o` name, as in bash. One-way: `set +r`
    // can't turn it off. Enforced in `run_cd`, the `exec`/`.`/`source` arms,
    // `check_restricted_command_name` and `open_out`.
    opt_restricted: bool,
    // `set -C`: a plain `>` refuses to truncate an existing regular file.
    // Enforced in `open_out`; `>|` carries a `clobber` flag past it.
    opt_noclobber: bool,
    // `set -o posix`: accepted and toggleable, so scripts setting it work,
    // but POSIX mode's many small differences aren't implemented.
    opt_posix: bool,
    // Suppresses errexit while above zero: around if/while/until conditions
    // and `!` pipelines, which POSIX exempts from `-e`.
    suppress_errexit: u32,
    // Set by `run_and_or` when its status came from a command `set -e`
    // doesn't apply to: bash exempts all but the command after the final
    // `&&` or `||`, so `false && echo no` doesn't abort. Read only by the
    // ListItem check.
    errexit_exempt: bool,
    // The status of the latest `$(...)`, for a bare assignment, which takes
    // it as its own: `x=$(false)` fails in bash, which is how `set -e`
    // catches a failed capture.
    last_subst_status: Option<i32>,
    // The current command's `2>` target while its words expand, so an
    // error during expansion (`set -u`'s "unbound variable") follows the
    // command's redirect, as in bash.
    current_stderr_target: Option<String>,
    // Set while running command-mode (`:`) input: only builtins run directly
    // (see the gate in `run_single` before function lookup); `command NAME`
    // reaches externals.
    pub restrict_to_builtins: bool,
    // Whether promotion to full-screen mode has happened (see
    // `promote_if_needed`); it flips once. Shared by every session on this
    // root, since the terminal is one, so a second session's `window` doesn't
    // promote again.
    promoted: Rc<Cell<bool>>,
    /// Whether something manages windows for this shell: set by repl.rs on
    /// its sessions. It lets the `window` builtin run from a function
    /// without letting `ExecResult::Window` escape from `bish script.sh`,
    /// where `run_program` would take it for a signal and stop.
    pub windows_available: bool,
    /// The windows that exist, refreshed by the repl loop before each
    /// command, so `window ls` is an ordinary builtin writing to its sink
    /// and `$(window ls)` captures it. Cloned into virtual children.
    pub windows: Vec<WindowInfo>,
    // This session's working directory. Every spawn passes it explicitly
    // (`current_dir`), so sessions sharing the process can differ; see
    // `sync_real_state_in` for relative-path file I/O.
    pub cwd: std::path::PathBuf,
    // See OutputSink: Real until repl.rs switches it to Grid on promotion.
    pub(crate) sink: OutputSink,
    // Whether the last byte written through the sink wasn't a newline. Only
    // the Real sink needs it (a grid tracks its own cursor): output ending
    // mid-row (`printf foo`) would otherwise be erased by the next prompt's
    // redraw, which assumes it owns the row. repl.rs reads it before drawing
    // the prompt. External output is `ran_external_since_prompt`'s problem.
    // A Cell, since the sink methods take `&self`.
    real_output_needs_newline: std::cell::Cell<bool>,
    // Whether an external process writing to the real terminal ran since
    // repl.rs last checked. bish never sees its output, so repl.rs has to ask
    // the terminal where the cursor is (`term::query_cursor_column`, a DSR
    // round trip); this lets it skip that after builtin-only commands.
    ran_external_since_prompt: std::cell::Cell<bool>,
    // Set by `run_fg` before returning `ExecResult::Fg` and taken by repl.rs
    // with `take_pending_fg`. Not shared: it belongs to the session that ran
    // `fg`.
    pub(crate) pending_fg: Option<Job>,
    // The same for `e` (see `ExecResult::Edit`): its arguments verbatim,
    // which the editor interprets (`fileeditor::parse_edit_args`).
    pending_edit: Option<Vec<String>>,
    // Set by `check_nounset` on a `set -u` violation, deep in word expansion
    // where there's no `ExecResult` to return. Checked in `run_single` after
    // building a command's argv, so the command never runs, and in
    // `run_program` per statement for expansions elsewhere (`for`, `case`,
    // arithmetic). Not shared with children.
    pending_exit: Option<i32>,
    // How many of the redirect scopes bash undoes before its EXIT trap are
    // open (a redirected group, loop or `if`, a builtin's `> f`, but not a
    // function call's). See `run_exit_trap`.
    redirect_scopes: u32,
    // The EXIT trap came due inside such a scope and waits for the last to
    // close.
    exit_trap_deferred: bool,
    // Whether this is a subshell bash leaves without putting anything back
    // (a `( )`, a pipeline stage, a background job), as opposed to the shell,
    // a `$( )` or a `<( )`. See `run_exit_trap`.
    forked_subshell: bool,
    // Whether a person is typing at this shell, set by repl.rs at interactive
    // startup and inherited. It decides which errors are fatal: bash exits a
    // script on assigning a readonly variable, which would be intolerable in
    // someone's session.
    interactive: bool,
    // Started as a login shell (`-l`, `--login`, or `argv[0]` starting with
    // `-`), as `shopt login_shell` reports in every child.
    pub(crate) started_as_login: bool,
    // Whether this shell is the login shell, as `logout` asks. Inherited,
    // except by a `( )` and a pipeline stage, as in bash (see
    // `run_body_in_child_shell`).
    pub(crate) login_shell: bool,
    // Set when expanding a word of the command being built failed: `${x!y}`,
    // `${a[}`, `${x:?}`, or arithmetic that doesn't parse. bash makes these
    // fatal in a script, but only in a word: `(( 1+ ))` and `let '1+'` just
    // fail. Cleared where `run_single` checks it.
    expansion_failed: bool,
    // See StdioOverride. Set by `run_in_child_shell` on the Shell it builds;
    // `None` otherwise. Not shared: a nested construct sets its own.
    stdio_override: Option<Rc<RefCell<StdioOverride>>>,
    // The active debugger, if this Shell or an ancestor runs under one (see
    // DebugHook). Shared, like `jobs`, so breakpoints fire inside a
    // subshell or substitution's separate Shell.
    debug_hook: Option<Rc<RefCell<dyn DebugHook>>>,
    // `$LINENO`: the line of the running statement, recorded in
    // `run_program`.
    current_line: usize,
    // How many in-process subshells and substitutions deep this Shell is,
    // incremented by `new_virtual_child`. Half of DebugDepth.
    subshell_depth: u32,
    // This session's view of the process environment and umask; see
    // `sync_real_state_in`. Sessions share one process, and exports and
    // `umask` change process-wide state, so each session's is restored when
    // it runs. `Rc`, because `new_virtual_child` runs on every `$( )` and
    // `( )`, and collecting `std::env::vars()` there cost ~18us for a copy
    // only session switching reads; shared until written (`Rc::make_mut`).
    env_snapshot: Rc<std::collections::HashMap<String, String>>,
    // Every global variable, seeded from the environment at startup
    // (inherited variables are exported, as in bash). The real environment
    // is derived: a name is written there only while in `exported_names`
    // (see `raw_var_write`), which is what children inherit. A BTreeMap so
    // `declare -p` and `compgen -v` list in a stable order. Behind an `Rc`
    // so a virtual child shares it until it assigns (`Rc::make_mut`).
    globals: Rc<std::collections::BTreeMap<String, String>>,
    // A count of effects: every command run, assignment made, and read of a
    // volatile variable (`$RANDOM`, `$SECONDS`, `$EPOCH*`). A function call
    // isn't one, its body's commands are, so `call_function` can tell a
    // function re-entered itself having done nothing, which with the same
    // arguments proves it can't terminate. See
    // `check_nonproductive_recursion`.
    effects: u64,
    pub(crate) umask_snapshot: u32,
}

// `_` as an owned String, so `global_names` and the pipeline environment
// can hand out a `&String` for the value kept apart from the map.
static UNDERSCORE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| "_".to_string());

/// A fresh seed for a new Shell or a virtual child, which doesn't inherit
/// the parent's state, so siblings' `$RANDOM` aren't correlated. `pid` is
/// passed in: `std::process::id()` is a real `getpid`, a sixth of the cost
/// of entering a `$( )`.
fn fresh_rng_seed(pid: u32) -> u64 {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0x2545F4914F6CDD1D)
        ^ (pid as u64).wrapping_mul(0x9E3779B97F4A7C15);
    if seed == 0 { 0x2545F4914F6CDD1D } else { seed }
}

/// One function call in progress, for `caller`: `called` is the function
/// running, `call_line` the line of the call, `source` that line's file.
/// The caller isn't stored: it's the frame below's `called`, or the
/// script.
#[derive(Clone)]
pub(crate) struct CallFrame {
    pub(crate) called: String,
    pub(crate) call_line: usize,
    pub(crate) source: String,
    // `Shell::effects` on entry and a hash of the arguments: the two halves
    // of `call_function`'s non-productive-recursion check.
    pub(crate) effects_at_entry: u64,
    pub(crate) args_hash: u64,
}

impl Shell {
    pub fn new() -> Self {
        // `std::env::vars()` walked once for all three collections below
        // (what's exported, the variables, the startup environment), 169us
        // of this function's 253us when done three times. The shell pid
        // comes first and is filtered out: it's a private channel to a
        // re-exec'd continuation, and left in, a new `bish -c` further down
        // would report its parent's `$$`.
        let shell_pid = inherited_shell_pid().unwrap_or_else(crate::platform::process_id);
        let mut inherited: Vec<(String, String)> = std::env::vars().filter(|(k, _)| k != REEXEC_SHELL_PID).collect();
        // `PS4` is a real variable with a default, so `unset PS4` traces
        // with no prefix, as in bash.
        if !inherited.iter().any(|(k, _)| k == "PS4") {
            inherited.push(("PS4".to_string(), "+ ".to_string()));
        }
        let mut shell = Shell {
            last_status: 0,
            functions: HashMap::new(),
            getopts_offset: 0,
            source_depth: 0,
            underscore_pending: Vec::new(),
            arg_frames: vec![Vec::new()],
            var_scopes: Vec::new(),
            script_name: "bish".to_string(),
            running_a_script: false,
            // Scripts read `${BASH_VERSINFO[0]}` more than they parse
            // `$BASH_VERSION`.
            arrays: HashMap::from([(
                "BASH_VERSINFO".to_string(),
                crate::sort::btree_map(BASH_VERSINFO.iter().enumerate().map(|(i, v)| (i, (*v).to_string()))),
            )]),
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
            shell_pid,
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
            // A variable inherited from the environment is exported, as in
            // bash, so `env` agrees with `declare -p`.
            exported_names: Rc::new(inherited.iter().map(|(k, _)| k.clone()).collect()),
            alias_epoch: 0,
            process_restore: ProcessRestore::default(),
            descriptors_to_follow: None,
            parse_cache: Rc::new(RefCell::new(std::collections::HashMap::new())),
            underscore: None,
            globals: Rc::new(crate::sort::btree_map(inherited.iter().cloned())),
            effects: 0,
            proc_sub_cleanup: Vec::new(),
            rng_state: fresh_rng_seed(shell_pid),
            shell_start: std::time::Instant::now(),
            seconds_offset: 0,
            jobs: Rc::new(RefCell::new(JobTable::new())),
            background_coroutines: Rc::new(RefCell::new(crate::scheduler::Scheduler::new())),
            proc_sub_pipes: Vec::new(),
            proc_sub_children: Vec::new(),
            traps: std::collections::HashMap::new(),
            bash_command: Rc::from(""),
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
            opt_noexec: false,
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
            redirect_scopes: 0,
            exit_trap_deferred: false,
            // A re-executed continuation of another shell (a background
            // job, a pipeline stage) is told that shell's `$$`, which is
            // how it knows.
            forked_subshell: shell_pid != crate::platform::process_id(),
            interactive: false,
            started_as_login: false,
            login_shell: false,
            expansion_failed: false,
            stdio_override: None,
            debug_hook: None,
            current_line: 0,
            subshell_depth: 0,
            env_snapshot: Rc::new(inherited.into_iter().collect()),
            umask_snapshot: current_umask(),
        };
        // IFS starts as space, tab and newline, as in bash: scripts save
        // and restore it (`old=$IFS; ...; IFS=$old`), and an unset IFS
        // would restore as empty, meaning "don't split".
        shell.assign_var("IFS", " \t\n".to_string());
        // `$_` starts as argv[0] as invoked, bash's rule (so `bash` by name,
        // `/usr/bin/bash` by path).
        if let Some(argv0) = std::env::args().next() {
            shell.assign_var("_", argv0);
        }
        // OPTIND and OPTERR start at 1, as in bash: functions save and
        // restore OPTIND around `getopts`.
        shell.assign_var("OPTIND", "1".to_string());
        shell.assign_var("OPTERR", "1".to_string());
        // `PWD` is set and exported at startup, as in bash. An inherited
        // one is kept if it names this directory another way, which is how
        // a path through a symlink survives; otherwise it's stale and
        // replaced.
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

    // Public for repl.rs's few direct sites (session error messages,
    // command-mode diagnostics); the `sh_print!` macros are private to this
    // module.
    pub fn sink_out(&self, s: &str) {
        self.sink.write_out(s);
        self.note_output(s);
    }

    pub fn sink_err(&self, s: &str) {
        self.sink.write_err(s);
        self.note_output(s);
    }

    // Updates `real_output_needs_newline`. An empty `s` leaves it, since the
    // cursor didn't move.
    fn note_output(&self, s: &str) {
        if let Some(c) = s.chars().last() {
            self.real_output_needs_newline.set(c != '\n');
        }
    }

    // Called by repl.rs before drawing the prompt (Real sink only): whether
    // the last output left the cursor mid-row, clearing the flag.
    pub fn take_needs_newline(&self) -> bool {
        self.real_output_needs_newline.replace(false)
    }

    // Notes that an external with the real terminal as stdout is about to be
    // spawned (see `ran_external_since_prompt`), from `run_single`, `command`
    // and `run_multi`'s stages.
    fn note_external_spawn(&self) {
        self.ran_external_since_prompt.set(true);
    }

    // Called with `take_needs_newline` before the prompt; see
    // `ran_external_since_prompt`.
    pub fn take_ran_external(&self) -> bool {
        self.ran_external_since_prompt.replace(false)
    }

    // This session's function names, without the bodies, for the
    // highlighter's command-validity check.
    pub fn function_names(&self) -> impl Iterator<Item = &str> {
        self.functions.keys().map(String::as_str)
    }

    // Called by repl.rs for each session on promotion, and for sessions made
    // later, to send output into its grid; see OutputSink.
    pub fn set_sink_grid(&mut self, screen: Rc<RefCell<vt100::Screen>>) {
        self.sink = OutputSink::Grid(screen);
    }

    // Command mode sets this around one command, then `set_sink_grid` again;
    // see `OutputSink::Capture`.
    pub(crate) fn set_sink_capture(&mut self, buf: Rc<RefCell<String>>) {
        self.sink = OutputSink::Capture(buf);
    }

    /// Redirects output into `buf` and returns what was there, for a caller
    /// that must restore it: an unpromoted session writes to the terminal,
    /// and "restoring" it to an unpainted grid would hide every later line.
    pub(crate) fn borrow_sink(&mut self, buf: Rc<RefCell<String>>) -> SavedSink {
        SavedSink(std::mem::replace(&mut self.sink, OutputSink::Capture(buf)))
    }

    pub(crate) fn return_sink(&mut self, saved: SavedSink) {
        self.sink = saved.0;
    }

    /// The last command's status, for a caller about to run something the
    /// user didn't type (a hook) and restore it after. Otherwise a hook would
    /// become the user's next `$?`, and `$?` inside a `shell:exec:post` hook
    /// couldn't mean the command's status.
    pub fn last_status(&self) -> i32 {
        self.last_status
    }

    pub fn set_last_status(&mut self, status: i32) {
        self.last_status = status;
    }

    // For the debugger handing the terminal to the script
    // (`PauseState::hand_off_to_script`): builtin output, such as `read -p`'s
    // prompt, has to reach the terminal at once.
    pub(crate) fn set_sink_real(&mut self) {
        self.sink = OutputSink::Real;
    }

    // Undoes any `stdio_override`, alongside `set_sink_real`, so a spawned
    // external also writes to the real terminal.
    pub(crate) fn clear_stdio_override(&mut self) {
        self.stdio_override = None;
    }

    // The size to give a fresh pty before a full-screen program asks;
    // otherwise it gets the kernel default and draws in a corner. A grid's
    // screen is kept at the session's on-screen size (the window, or its
    // pane; see `snapshot_window`), and ptys are only attached once promoted,
    // so that's the answer in practice. Otherwise TIOCGWINSZ on the
    // controlling terminal.
    fn pty_size(&self) -> (u16, u16) {
        // Through `sink_grid`: a command with an output redirect runs under
        // a `Builtin` sink, and a background job's pty should match its
        // pane, not the whole terminal.
        if let Some(screen) = self.sink_grid() {
            let (rows, cols) = screen.borrow().size();
            return (rows as u16, cols as u16);
        }
        // No grid (unpromoted, or command mode's capture): ask the terminal.
        match pty::get_size(0) {
            Ok(ws) if ws.rows > 0 && ws.cols > 0 => (ws.rows, ws.cols),
            _ => (24, 80),
        }
    }

    // Creates a related but independent Shell, as `window new` makes a
    // session, sharing the process and so the job table (a real child
    // couldn't). Variables, functions, aliases, options and cwd are copied,
    // so the two start identical and diverge; `jobs` and `promoted` are
    // shared. Non-cloneable state (`coproc_fds`) and what belongs to the
    // parent's current command (scope stacks, pending proc-subs) start
    // fresh. History sharing is repl.rs's (see history.rs).
    pub fn new_virtual_child(&self) -> Shell {
        Shell {
            last_status: 0,
            functions: self.functions.clone(),
            getopts_offset: 0,
            source_depth: 0,
            underscore_pending: Vec::new(),
            // A subshell sees its parent's positional parameters and the
            // `local`s in scope. One frame, not the stack: `shift` or `set`
            // inside mustn't reach a caller's frame.
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
            shell_pid: self.shell_pid,
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
            exported_names: Rc::clone(&self.exported_names),
            alias_epoch: self.alias_epoch,
            // Fresh: what the parent moved is the parent's to restore.
            process_restore: ProcessRestore::default(),
            descriptors_to_follow: None,
            parse_cache: Rc::clone(&self.parse_cache),
            underscore: self.underscore.clone(),
            globals: Rc::clone(&self.globals),
            effects: self.effects,
            proc_sub_cleanup: Vec::new(),
            rng_state: fresh_rng_seed(self.shell_pid),
            shell_start: std::time::Instant::now(),
            seconds_offset: 0,
            jobs: self.jobs.clone(),
            background_coroutines: Rc::clone(&self.background_coroutines),
            proc_sub_pipes: Vec::new(),
            proc_sub_children: Vec::new(),
            traps: self.traps.clone(),
            bash_command: Rc::clone(&self.bash_command),
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
            opt_noexec: self.opt_noexec,
            opt_errexit: self.opt_errexit,
            opt_nounset: self.opt_nounset,
            opt_xtrace: self.opt_xtrace,
            opt_pipefail: self.opt_pipefail,
            opt_noglob: self.opt_noglob,
            opt_monitor: self.opt_monitor,
            // Restricted mode stays in every descendant, as in bash.
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
            redirect_scopes: 0,
            exit_trap_deferred: false,
            forked_subshell: false,
            interactive: self.interactive,
            started_as_login: self.started_as_login,
            login_shell: self.login_shell,
            expansion_failed: false,
            stdio_override: None,
            debug_hook: self.debug_hook.clone(),
            current_line: self.current_line,
            subshell_depth: self.subshell_depth + 1,
            // Shared with the parent: it equals `self.env_snapshot` at this
            // point (a virtual child is only made in the synced-in
            // session), and only session switching reads it. Writers
            // (`run_cd`, `set_terminal_capability_env`) go through
            // `Rc::make_mut`.
            env_snapshot: Rc::clone(&self.env_snapshot),
            umask_snapshot: current_umask(),
        }
    }

    // True once promotion has switched the terminal to full screen; repl.rs
    // checks at exit whether to restore the normal screen.
    pub fn is_promoted(&self) -> bool {
        self.promoted.get()
    }

    /// Marks this shell promoted without printing the screen-switching
    /// escapes, for a caller capturing output rather than showing it:
    /// bashdiff's pane corpus.
    pub fn mark_promoted(&self) {
        self.promoted.set(true);
    }

    /// Runs a job the last command handed off, here, returning its status,
    /// instead of repl.rs driving it. See `drive_pending_fg_inline`; this is
    /// its one caller outside execution.
    pub fn settle_pending_fg(&mut self) -> i32 {
        match self.pending_fg.is_some() {
            true => self.drive_pending_fg_inline(),
            false => self.last_status,
        }
    }

    pub fn run_exit_trap(&mut self) {
        // Only for the exit of the shell that armed it. A subshell inherits
        // the trap (`trap -p EXIT` shows it there, as in bash), but its end
        // isn't that exit. The trap is nearly always a cleanup
        // (`trap 'rm -rf "$tmp"' EXIT`), which firing at every `$( )`,
        // `( )` and pipeline stage would run while the script still needs
        // the directory.
        if self.subshell_depth != self.exit_trap_depth {
            return;
        }
        // Inside a redirected group, loop, `if` or builtin, the trap waits
        // until those redirects are undone, as bash undoes them first:
        // `{ exit; } > f` says goodbye on the terminal. The last scope to
        // close runs it (see `leave_redirect_scope`). Not while a function
        // runs: bash then undoes nothing (`f(){ { exit; } > g; }; f` writes
        // into `g`), so the trap runs here. Nor in a `( )`, pipeline stage
        // or background job, which leave every redirect in place too.
        if self.redirect_scopes > 0 && self.function_depth == 0 && !self.forked_subshell {
            self.exit_trap_deferred |= self.exit_trap.is_some();
            return;
        }
        if let Some(cmd) = self.exit_trap.take() {
            self.run_source_here(&cmd, "trap");
        }
    }

    /// Whether a `command_not_found_handle` is defined and may run now; a
    /// handler mistyping a command would otherwise call itself forever.
    fn has_command_not_found_handler(&self) -> bool {
        !self.in_command_not_found && self.functions.contains_key(COMMAND_NOT_FOUND_HANDLER)
    }

    /// bash's `command_not_found_handle`, which distributions' "install
    /// package X" suggestions use. Called with the failed command line as its
    /// arguments; its status becomes the shell's.
    fn run_command_not_found_handler(&mut self, argv: &[String]) -> ExecResult {
        let Some(body) = self.functions.get(COMMAND_NOT_FOUND_HANDLER).cloned() else {
            return ExecResult::Status(127);
        };
        self.in_command_not_found = true;
        let result = self.call_function(&argv[0], &body, argv.to_vec());
        self.in_command_not_found = false;
        result
    }

    /// Runs `PROMPT_COMMAND` before a primary prompt, as a string or (bash
    /// 5.1) an array run element by element. Its output isn't captured,
    /// unlike a `::bish hook`'s: printing (a title escape, a blank line) is
    /// most of what it's for. `$?` is saved and restored, so the next prompt
    /// still sees the user's command's status.
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

    /// Runs a pseudo-signal trap if set, saving and restoring `$?` as
    /// `run_hooks_for` does: a DEBUG trap mustn't change the status of the
    /// command it fired before. Re-entry is refused, since a trap's body
    /// would fire it again.
    fn run_pseudo_trap(&mut self, which: PseudoTrap) {
        if self.in_trap {
            return;
        }
        // Not inherited by a function unless the matching option says so,
        // as in bash.
        let inherited = match which {
            PseudoTrap::Err => self.opt_errtrace,
            PseudoTrap::Debug | PseudoTrap::Return => self.opt_functrace,
        };
        // ERR keeps a coarser gate: bash fires it once per failure, at the
        // innermost level with the trap, not again for each function call the
        // failure propagates out of.
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

    // This shell is one somebody is typing at. Separate from
    // `enable_monitor_mode`: monitor mode is about the terminal and starts
    // when the prompt owns it, while this is about the rc file being read
    // interactively, so a `set -n` in it is discarded and a readonly
    // violation there doesn't kill the shell.
    pub fn mark_interactive(&mut self) {
        self.interactive = true;
    }

    // Job control (`set -m`) on, as bash does by default for an interactive
    // shell. Called by repl.rs for the root session; virtual children
    // inherit it.
    pub fn enable_monitor_mode(&mut self) {
        self.opt_monitor = true;
        self.interactive = true;
        crate::term::ignore_tty_signals();
    }

    // For repl.rs's EOF handler: warn "There are stopped jobs." instead of
    // exiting, as bash refuses the first Ctrl-D then. Only stopped jobs:
    // bash doesn't warn about running ones.
    pub fn has_stopped_jobs(&self) -> bool {
        self.jobs.borrow().jobs.iter().any(|j| j.stopped)
    }

    // The session's grid, looked up through whatever sink is installed on
    // top: a command with an output redirect runs under a `Builtin` sink
    // (which keeps the one it displaced), so `cmd >file &` would otherwise
    // find no grid and lose the stream it didn't redirect. `None` when never
    // promoted, and for command mode's `Capture` overlay.
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

    // A pty for a background job's stdio, sized to this session's area, or
    // `None` (not promoted, or no pty), leaving callers as they were. A pty
    // rather than a pipe, so `isatty()` answers as with the terminal (`ls &`
    // keeps its colours) and `fg` can use `drive_fg_job`.
    fn background_pty(&self) -> Option<pty::Pty> {
        if !self.is_promoted() {
            return None;
        }
        let p = pty::open().ok()?;
        let (rows, cols) = self.pty_size();
        let _ = p.resize(rows, cols);
        Some(p)
    }

    // Feeds what the pty-backed background jobs produced into the grid of
    // the session that started each (`Job::sink_screen`); `true` if anything
    // arrived. Without it, output filled the pty unseen until `fg`. The job
    // table is shared, so one call from any shell services all of them.
    pub(crate) fn drain_background_output(&self) -> bool {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        // Bounded per call, so a firehose job can't hold up the interactive
        // loop.
        const MAX_READS_PER_TICK: u32 = 16;
        let mut buf = [0u8; 4096];
        let mut fed = false;
        let mut table = self.jobs.borrow_mut();
        for job in &mut table.jobs {
            let (Some(master), Some(screen)) = (&mut job.pty_master, &job.sink_screen) else {
                continue;
            };
            // While an editor frame has the screen on its alternate buffer
            // (repl.rs's `enter_alt_screen`), output would scribble over
            // the file, so it waits in the pty until the frame closes.
            if screen.borrow().using_alternate {
                continue;
            }
            if !job.nonblocking {
                // Non-blocking, set here so spawn sites needn't: a blocking
                // read would park the shell on a quiet job.
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

    // Registers a spawned background job (one child, or several for a
    // pipeline) and sets `$!` to the last child's pid, as bash does for a
    // pipeline.
    fn push_job(&mut self, children: Vec<std::process::Child>, cmd_text: String) {
        self.push_job_with_pty(children, cmd_text, None);
    }

    // `push_job`, also recording the pty master the job's stdio is on (see
    // `Job::pty_master`); only `run_single`'s background spawn passes one.
    // For a process-group job, `push_job_with_pgid`.
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

    // A job spec (`%N`, `%%`/`%+` current, `%-` previous, `%name` prefix of
    // the command) to an index in `self.jobs`. Bare numbers too, as bash's
    // `fg`, `bg` and `wait` accept.
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

    /// Runs a job stashed for repl.rs to drive, here instead, returning its
    /// status. `ExecResult::Fg` is a control-flow escape (see `is_signal`),
    /// so bubbling it from a command with more after it would abandon the
    /// rest of the `&&` chain: `make && ./run` in a split window ran `make`
    /// and stopped. So such a command's pty is drained here, the output
    /// reaching the pane and the line carrying on, without repl.rs's key
    /// forwarding, Ctrl-C and resize, which matter for the command being
    /// sat in front of.
    fn drive_pending_fg_inline(&mut self) -> i32 {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let Some(mut job) = self.pending_fg.take() else { return self.last_status };
        let screen = self.sink_grid();
        if let Some(master) = job.pty_master.as_mut() {
            pty::set_nonblocking(master.as_raw_fd());
        }
        let mut buf = [0u8; 4096];
        // Everything waiting on the pty right now, into the pane.
        // `Err` covers the EIO Linux gives once the child has gone and
        // nothing holds the slave any more, which is this loop's real
        // end condition rather than an error.
        let mut drain = |job: &mut Job| -> bool {
            let (Some(master), Some(screen)) = (job.pty_master.as_mut(), screen.as_ref()) else {
                return false;
            };
            let mut fed = false;
            loop {
                match master.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        screen.borrow_mut().feed(&buf[..n]);
                        fed = true;
                    }
                    Err(_) => break,
                }
            }
            fed
        };
        // A peer asking about the editor is answered from here too
        // (`session::service_while_busy`), at most every 10ms, so a job that
        // prints nonstop doesn't pay for the socket on every read.
        let mut serviced = std::time::Instant::now();
        loop {
            let fed = drain(&mut job);
            if serviced.elapsed() >= std::time::Duration::from_millis(10) {
                crate::session::service_while_busy();
                serviced = std::time::Instant::now();
            }
            let done = match job.children.last_mut() {
                Some(child) => matches!(child.try_wait(), Ok(Some(_)) | Err(_)),
                None => true,
            };
            if done {
                break;
            }
            if !fed {
                // Nothing to read, not finished. A `<( )` producer feeding
                // this command is a coroutine here, the only thing that can
                // fill its pipe, so it gets a turn before sleeping.
                self.pump_coroutines();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        // Whatever landed between the last read and the child exiting.
        drain(&mut job);
        let status = job.wait();
        // The command has finished reading, so any `<( )` feeding it can be
        // torn down.
        self.drain_proc_subs();
        status
    }

    /// Turns a bubbled `ExecResult::Fg` into a status by running the job
    /// here, for callers with more to do afterwards. Anything else is
    /// returned as is.
    fn settle_fg(&mut self, result: ExecResult) -> ExecResult {
        match result {
            ExecResult::Fg => {
                let status = self.drive_pending_fg_inline();
                self.last_status = status;
                ExecResult::Status(status)
            }
            other => other,
        }
    }

    // For `:fg` in command mode, whose nested loop (`run_command_mode`) can't
    // hand a job to the compositor: reaps it with a blocking wait instead of
    // leaving it orphaned in `pending_fg`.
    pub fn discard_pending_fg(&mut self) -> i32 {
        match self.pending_fg.take() {
            Some(mut job) => job.wait(),
            None => 0,
        }
    }

    // Hands a job stashed by `run_fg` to repl.rs, wrapped so `Job`'s private
    // fields stay private (see FgJob).
    pub fn take_pending_fg(&mut self) -> Option<FgJob> {
        self.pending_fg.take().map(FgJob)
    }

    /// Called by repl.rs when the job from `take_pending_fg` has finished:
    /// a `<( )` feeding it was left running meanwhile (see
    /// `drain_proc_subs`) and can go now.
    pub fn finished_foreground_job(&mut self) {
        self.drain_proc_subs();
    }

    // See `ExecResult::Edit`. Only called right after that signal, so "`e`
    // never ran" can't happen, and folds into the empty vector of a bare `e`.
    pub fn take_pending_edit(&mut self) -> Vec<String> {
        self.pending_edit.take().unwrap_or_default()
    }

    // See `pending_exit`. Runs the exit trap once, where the violation is
    // acted on, not inside `check_nounset`, which runs in many, sometimes
    // re-entrant, contexts.
    fn take_pending_exit(&mut self) -> Option<ExecResult> {
        self.pending_exit.take().map(|code| {
            self.run_exit_trap();
            ExecResult::Exit(code)
        })
    }

    /// A `Command` inheriting this shell's environment, not the process's.
    /// Every process the shell starts goes through here:
    /// `every_process_the_shell_starts_goes_through_command` fails on any
    /// other `Command::new` not on its allow list, since a spawn that
    /// misses this silently gets a stale environment.
    pub(crate) fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.env_clear();
        command.envs(self.exported_pairs());
        // The shell's cwd: two shells in one process can be in different
        // directories. A caller can still set another afterwards.
        command.current_dir(&self.cwd);
        command
    }

    /// `command`, for a child that is this shell continuing (a background
    /// subshell, a coproc, a pipeline stage, a redirected compound). bish
    /// re-execs where bash forks, so the child must be told `$$`, or
    /// `kill -HUP -$$` in a background subshell signals a group that doesn't
    /// exist. Not in `command`, since a user's `bish -c 'echo $$'` is a new
    /// shell and must report its own pid.
    pub(crate) fn reexec_command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = self.command(program);
        command.env(REEXEC_SHELL_PID, self.shell_pid.to_string());
        command
    }

    /// `reexec_command` with the script to run, passed through the
    /// environment rather than argv; see REEXEC_SCRIPT.
    fn reexec_running(&self, program: impl AsRef<std::ffi::OsStr>, script: String) -> Command {
        let mut command = self.reexec_command(program);
        command.env(REEXEC_SCRIPT, script);
        command.arg("--reexec");
        command
    }

    /// Every exported variable as a child should see it, resolved like
    /// `$name`, so an exported `local` shadows the global. Names and values
    /// with a NUL are dropped: a C environment string can't hold one (as in
    /// `export_to_environment`). Borrowed: building owned Strings on every
    /// spawn was about 140 allocations, more work than the spawn.
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

    /// The value a child sees for `name`; an exported but unset name exports
    /// nothing, as in bash.
    fn lookup_var_for_export(&self, name: &str) -> Option<&str> {
        for scope in self.var_scopes.iter().rev() {
            if let Some(value) = scope.get(name) {
                return value.as_deref();
            }
        }
        self.global_get(name).map(String::as_str)
    }

    // The `Stdio::inherit()` fallback for a spawned command's stdout,
    // honouring `stdio_override`, so a converted subshell or substitution
    // still captures its externals' output. Every spawn's fallback goes
    // through this.
    fn spawn_stdout_stdio(&self) -> Stdio {
        match &self.stdio_override {
            // A spawned process needs a real descriptor, so a memory
            // capture becomes one here.
            Some(o) => match o.borrow().stdout.as_ref().and_then(|c| c.descriptor()) {
                Some(f) => Stdio::from(f),
                None => Stdio::inherit(),
            },
            None => Stdio::inherit(),
        }
    }

    /// Where an external inside a redirected construct sends stderr: the
    /// mirror of `ChildStdio::sink_stderr` for builtins.
    fn effective_stderr(&self, stdio: &ChildStdio, effective_stdout: &Option<Rc<Capture>>) -> Option<std::fs::File> {
        let own = match stdio.err_follows_out {
            // `2>&1` before this construct rebound fd 1: whatever fd 1 is
            // inside it, materialised as a descriptor if still a buffer.
            Some(Follows::Outer) => effective_stdout.as_ref().and_then(|c| c.descriptor()),
            Some(Follows::OwnFile) => stdio.stdout.as_ref().and_then(|c| c.descriptor()),
            None => stdio.stderr.as_ref().and_then(|f| f.try_clone().ok()),
        };
        own.or_else(|| self.stdio_override.as_ref().and_then(|o| o.borrow().stderr.as_ref().and_then(|f| f.try_clone().ok())))
    }

    /// The same for stderr; see `StdioOverride::stderr`.
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

    // The same for stdin.
    fn spawn_stdin_stdio(&self) -> Stdio {
        match &self.stdio_override {
            Some(o) => match &o.borrow().stdin {
                // Bytes this shell's `read` fetched but didn't consume
                // aren't seen by a spawned process, which reads the fd
                // directly; re-exec'd children have the same gap through
                // `std::io::stdin()`'s buffering.
                Some(r) => match r.borrow().file.try_clone() {
                    Ok(f) => Stdio::from(f),
                    Err(_) => Stdio::inherit(),
                },
                None => Stdio::inherit(),
            },
            None => Stdio::inherit(),
        }
    }

    // Called by repl.rs's `drive_fg_job` when the pty-attached job it drives
    // has stopped (Ctrl-Z in its pty, or `kill -STOP`). That job was never in
    // the table (its `id: 0` in `run_single` is a placeholder), so it gets a
    // real id now. Returns the id and command text for the caller to print
    // bash's "[N]+  Stopped ..." in the right session's grid.
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

    // `declare -f [name...]` and `-F`: each function's definition, or just
    // `declare -f NAME` under `-F`; all functions, sorted, without names
    // (bash uses definition order).
    pub(crate) fn print_functions(&mut self, names: &[String], names_only: bool) -> i32 {
        let targets: Vec<String> = if names.is_empty() {
            let mut all: Vec<String> = self.functions.keys().cloned().collect();
            crate::sort::sort(&mut all);
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
                        // `declare -F` alone prints a re-readable
                        // `declare -f NAME` line for each.
                        match names.is_empty() {
                            true => sh_println!(self, "declare -f {}", name),
                            false => sh_println!(self, "{}", name),
                        }
                    } else {
                        // Laid out as bash lays out a function, not as the
                        // round-trip serializer writes one (see
                        // `serialize::format_function`), since this output
                        // is read. It ends at `}` with no terminator, which
                        // `sh -c "$(declare -f f); f"` depends on.
                        sh_println!(self, "{}", crate::serialize::format_function(&name, &body));
                    }
                }
                None => {
                    // As in bash, `declare -f`/`-F` on a non-function fails
                    // silently, unlike `declare -p` on a missing variable.
                    status = 1;
                }
            }
        }
        status
    }

    // `declare -p [name...]`: each variable in declare form; without names,
    // every visible variable and array (globals and current locals), sorted.
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

    // The attribute letters `declare -p`, `${v@a}` and `${v@A}` show, in
    // bash's order: `A`/`a` first, then `i`, `r`, `x`, `n`, `u`, `l`.
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

    // `${v@A}`: a statement that recreates the variable, as bash formats
    // it. A whole array is `declare -p`'s output; bash collapses a bare
    // `${arr@A}` to the first element, but the whole array is what a script
    // wants. A scalar or one element is single-quoted, prefixed by
    // `declare -flags` only when there are attributes; an array element
    // still shows `-a` (`${arr[0]@A}` is `declare -a arr='1'`).
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

    // `${arr[@]@K}`: key value pairs, values double-quoted as in
    // `declare -p`. `array_keys` and `array_all` walk the same map, so
    // zipping pairs each key with its value.
    fn array_key_value_pairs(&self, name: &str) -> String {
        let keys = self.array_keys(name);
        let values = self.array_all(name);
        keys.iter().zip(values.iter()).map(|(k, v)| format!("{k} {}", declare_p_quote(v))).collect::<Vec<_>>().join(" ")
    }

    // `${v@P}`: expands `s` as a PS1 string, bash's backslash escapes,
    // computed from live state (local time, hostname, cwd, job count). Not
    // bish's own prompt (prompt.rs), just a value transform like every
    // `${v@X}`. `\!` and `\#` read 0: those counts live in repl.rs's
    // SessionState.
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
            // `\nnn`: 1-3 octal digits.
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
                // The non-printing brackets are dropped, as bash's `${v@P}`
                // does; they only matter to a live prompt's column count.
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

    // `\w`/`\W`, bash's forms (the path with `~` for home, or its basename),
    // not prompt.rs's `shorten_path`.
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

        // A name with the attribute and no map was declared, not assigned,
        // and prints without `=` (below). An empty map was assigned by
        // `M=()` and prints `=()`.
        if let Some(map) = self.assoc_names.contains(name).then(|| self.assoc_arrays.get(name)).flatten() {
            let mut body = String::new();
            for (k, v) in map.iter() {
                body.push('[');
                body.push_str(k);
                body.push_str("]=");
                body.push_str(&declare_p_quote(v));
                body.push(' ');
            }
            // The trailing space is bash's, for associative arrays only,
            // kept since `declare -p` output is meant to be re-read.
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
            // A name can have attributes and no value: `export E` on an
            // unset `E` prints `declare -x E`, as in bash. Only no
            // attributes either is "not found".
            if flags.is_empty() {
                return None;
            }
            return Some(format!("declare {} {}", flag_str, name));
        }
        let value = self.lookup_var(name);
        Some(format!("declare {} {}={}", flag_str, name, declare_p_quote(&value)))
    }

    /// Turns a shopt on from Rust, for a default a mode implies:
    /// `expand_aliases` for an interactive shell, as in bash.
    pub fn enable_shopt(&mut self, name: &str) {
        self.shopt_options.insert(name.to_string(), true);
    }

    /// Substitutes this shell's aliases into a tokenized line when
    /// `shopt -s expand_aliases` is on. Public so every path from text to a
    /// `Program` (`eval`, `source`, a script, the prompt, command mode) uses
    /// it.
    pub fn expand_aliases(&self, toks: Vec<(crate::lexer::Tok, usize)>) -> Vec<(crate::lexer::Tok, usize)> {
        if !self.shopt_is_on("expand_aliases") || self.aliases.is_empty() {
            return toks;
        }
        crate::lexer::expand_aliases(toks, &|name| self.aliases.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone()))
    }

    // A shopt's state: an explicit `-s`/`-u` this session, else its
    // default from KNOWN_SHOPT_OPTIONS. `extglob` is always on, since
    // extended globbing isn't gated (see glob.rs).
    pub(crate) fn shopt_is_on(&self, name: &str) -> bool {
        if name == "extglob" {
            return true;
        }
        if name == "login_shell" {
            return self.started_as_login;
        }
        self.shopt_options.get(name).copied().unwrap_or_else(|| shopt_default_on(name).unwrap_or(false))
    }

    pub(crate) fn print_shopt_line(&mut self, name: &str, reusable: bool) {
        let on = self.shopt_is_on(name);
        if reusable {
            sh_println!(self, "shopt -{} {}", if on { "s" } else { "u" }, name);
        } else {
            // 20 then a tab: bash's column, so `shopt | grep` lines up the
            // same.
            sh_println!(self, "{:<20}\t{}", name, if on { "on" } else { "off" });
        }
    }

    // A bishopt's value: an explicit setting, then (unless `name` is
    // `theme` itself) what the active theme declared for it (see
    // `run_bish_theme_end`), then the registered default (`false` for a
    // Bool). `None` for an unregistered name. A Color default is CSS text,
    // parsed like a `--set` value; it `expect`s, since an unparseable
    // registered default is a bug, not user input.
    pub(crate) fn bishopt_value(&self, registry: &[(&str, BishOptDefault)], name: &str) -> Option<BishOptValue> {
        let default = registry.iter().find(|(n, _)| *n == name)?.1.clone();
        if let Some(v) = self.bishopts.get(name) {
            return Some(v.clone());
        }
        if name != "theme"
            && let Some(BishOptValue::Str(active)) = self.bishopts.get("theme")
        {
            if let Some(v) = self.themes.get(active).and_then(|theme| theme.opts.get(name)) {
                return Some(v.clone());
            }
            // Then a shipped theme, per name below a declared one:
            // declaring a `kaamos` that sets one colour leaves the rest to
            // the shipped one. An unparseable colour in BUILTIN_THEMES is a
            // bug a test catches.
            if let Some(text) = crate::theme::builtin(active).and_then(|t| t.opts.iter().find(|(n, _)| *n == name)).map(|(_, css)| *css)
                && let Ok(parsed) = crate::csscolor::parse_terminal_list(text)
            {
                return Some(BishOptValue::Color(text.to_string(), parsed));
            }
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

    // `::bish hl --set`'s write, captured by a theme declaration as
    // `store_bishopt` captures bishopts, so a theme holds colours and options
    // together.
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

    /// The colour for `name`, following the active theme when nothing is
    /// set, as for a bishopt. `None` means nothing was said and the caller
    /// keeps its default: the names are open (see `Shell::hl`), so there
    /// are no registered defaults.
    pub fn hl_color(&self, name: &str) -> Option<vt100::Color> {
        self.hl_color_for(name, detect_color_support())
    }

    // `hl_color` with the terminal's support passed in, so tests can cover
    // every tier without changing the environment.
    fn hl_color_for(&self, name: &str, support: crate::csscolor::ColorSupport) -> Option<vt100::Color> {
        let text = match self.hl.get(name) {
            Some(text) => text.clone(),
            None => {
                let BishOptValue::Str(active) = self.bishopts.get("theme")? else { return None };
                // A declared theme first, then a shipped one, per name.
                match self.themes.get(active).and_then(|theme| theme.hl.get(name)) {
                    Some(text) => text.clone(),
                    None => crate::theme::builtin(active)?.hl.iter().find(|(n, _)| *n == name)?.1.to_string(),
                }
            }
        };
        let candidates = crate::csscolor::parse_terminal_list(&text).ok()?;
        Some(match crate::csscolor::pick(&candidates, support) {
            crate::csscolor::TermColor::Rgba(rgba) => vt100::Color::Rgb(rgba.r, rgba.g, rgba.b),
            crate::csscolor::TermColor::Ansi(n) => vt100::Color::Indexed(n),
        })
    }

    /// Every highlight colour in force, theme included: what a redraw builds
    /// its overrides from, and what bare `::bish hl` lists.
    pub fn hl_colors(&self) -> Vec<(String, String)> {
        let mut out: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if let Some(BishOptValue::Str(active)) = self.bishopts.get("theme") {
            // Bottom tier first, so a declared theme's own entries land
            // on top of the shipped ones name by name.
            if let Some(shipped) = crate::theme::builtin(active) {
                out.extend(shipped.hl.iter().map(|(k, v)| (k.to_string(), v.to_string())));
            }
            if let Some(theme) = self.themes.get(active) {
                out.extend(theme.hl.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }
        // Anything set directly wins over the theme, same as a bishopt.
        out.extend(self.hl.iter().map(|(k, v)| (k.clone(), v.clone())));
        let mut list: Vec<(String, String)> = out.into_iter().collect();
        crate::sort::sort(&mut list);
        list
    }

    // Every `bishopt --set`'s write, captured in `pending_theme` instead
    // while a theme declaration is open. `--unset` doesn't come through
    // here, since unsetting isn't declaring (see `run_bish_theme_end`), so
    // it acts on live state even mid-declaration.
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

    // One block per option: its name, what it accepts, its current value,
    // and its BISHOPT_HELP line. Shared by `bishopt --describe` and
    // `:help options`, so they can't drift.
    /// `describe_bishopts` over the real registry, for callers outside this
    /// module.
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
            // The value as `bishopt NAME` prints it.
            let current = self.bishopt_display(registry, name);
            out.push(name.to_string());
            out.push(format!("    {description}"));
            out.push(format!("    accepts: {accepts}    default: {default_text}    now: {current}"));
        }
        out
    }

    // What `bishopt NAME` prints, as a string.
    fn bishopt_display(&self, registry: &[(&str, BishOptDefault)], name: &str) -> String {
        match self.bishopt_value(registry, name) {
            Some(BishOptValue::Bool(on)) => if on { "on" } else { "off" }.to_string(),
            Some(BishOptValue::Int(n)) => n.to_string(),
            Some(BishOptValue::Str(s)) => format!("{s:?}"),
            Some(BishOptValue::Color(text, _)) => text,
            None => String::new(),
        }
    }

    // `--lang=GLOB` or `--lang GLOB`, returning it and what follows, so `add`
    // and `ls` agree on the spelling.
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

    // `--root-cmd=COMMAND` or `--root-cmd COMMAND`, separate from
    // `lsp_root_flag` so the two can come in either order.
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

    // `--setting KEY=VALUE` or `--setting=KEY=VALUE`, one pair per call; the
    // caller's loop makes it repeatable anywhere among the flags.
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
        // Split on the first `=`: values may contain more, keys may not.
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

    // `--apply-edits=scoped|never|always`, default `scoped`.
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

    /// The declared server for a `language` file: the first whose `--lang`
    /// matches, so an earlier, more specific one beats a later catch-all, as
    /// in `hooks_for`.
    pub fn lsp_server_for(&self, language: &str) -> Option<&LspServer> {
        if !self.bishopt_bool("lsp") {
            return None;
        }
        self.lsp_servers.iter().find(|s| crate::glob::matches(&s.lang, language))
    }

    // `--lang=GLOB` or `--lang GLOB`, as in `hook_lang_flag`.
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

    // `--root=NAME[,NAME...]`, default `.git`, where
    // `gitignore::Stack::for_directory` also stops.
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

    /// The commands for `event` on a `language` file, in the order added.
    /// Usually empty, which keeps firing an event free.
    pub fn hooks_for(&self, event: &str, language: &str) -> Vec<String> {
        if self.firing_hooks {
            return Vec::new();
        }
        self.hooks.iter().filter(|h| h.event == event && crate::glob::matches(&h.lang, language)).map(|h| h.command.clone()).collect()
    }

    /// Brackets a run of hooks, so what they do can't fire more hooks.
    pub fn set_firing_hooks(&mut self, firing: bool) {
        self.firing_hooks = firing;
    }

    // A registered Bool, Int or Str option's value. Panics on an unknown
    // name: every caller names a KNOWN_BISHOPTS constant, so a miss is a typo
    // here.
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

    // A Color bishopt's value as a `vt100::Color`: Rgb for CSS, Indexed for
    // a `-bish-*` palette slot (see `csscolor::TermColor`). `None` for an
    // unknown or non-Color name. repl.rs builds a session's ColorOverrides
    // from these each redraw; the terminal's support is detected each call,
    // which is two environment lookups.
    pub fn bishopt_color(&self, name: &str) -> Option<vt100::Color> {
        self.bishopt_color_for(name, detect_color_support())
    }

    // `bishopt_color` with the terminal's support passed in, so tests can
    // cover every tier without changing the environment (racy in parallel
    // tests).
    fn bishopt_color_for(&self, name: &str, support: crate::csscolor::ColorSupport) -> Option<vt100::Color> {
        let BishOptValue::Color(_, candidates) = self.bishopt_value(KNOWN_BISHOPTS, name)? else {
            return None;
        };
        Some(match crate::csscolor::pick(&candidates, support) {
            crate::csscolor::TermColor::Rgba(rgba) => vt100::Color::Rgb(rgba.r, rgba.g, rgba.b),
            crate::csscolor::TermColor::Ansi(n) => vt100::Color::Indexed(n),
        })
    }

    // `${!prefix*}`/`${!prefix@}`: every visible variable or array name
    // starting with `prefix`, sorted and deduplicated, as bash prints them.
    fn var_names_with_prefix(&self, prefix: &str) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = crate::sort::btree_set(self.global_names().cloned());
        for scope in &self.var_scopes {
            names.extend(scope.keys().cloned());
        }
        names.extend(self.arrays.keys().cloned());
        names.extend(self.assoc_arrays.keys().cloned());
        // Computed variables aren't in the tables (see `lookup_var`'s tail)
        // but are set as far as anything can tell, so `${!BASH*}` lists
        // them.
        names.extend(COMPUTED_VAR_NAMES.iter().map(|n| n.to_string()));
        names.into_iter().filter(|n| n.starts_with(prefix)).collect()
    }

    // Everything compgen.rs's contextual actions need, built per call (and
    // once per prompt by repl.rs for Tab completion).
    pub(crate) fn action_context(&self) -> compgen::ActionContext {
        let mut arrays: Vec<String> = self.arrays.keys().cloned().collect();
        arrays.extend(self.assoc_arrays.keys().cloned());
        let mut variables: Vec<String> = self.global_names().cloned().collect();
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
            signal_names: all_signals().into_iter().map(|(n, _)| n).collect(),
            jobs: jobs.jobs.iter().map(|j| j.cmd_text.clone()).collect(),
            running_jobs: jobs.jobs.iter().filter(|j| !j.stopped).map(|j| j.cmd_text.clone()).collect(),
            stopped_jobs: jobs.jobs.iter().filter(|j| j.stopped).map(|j| j.cmd_text.clone()).collect(),
            // This session's PATH, not the environment's (see
            // `HighlightContext::path`).
            path_commands: highlight::enumerate_path_matches("", Some(&self.raw_var_lookup("PATH"))),
        }
    }

    // The registered specs, read-only, for repl.rs's per-prompt Tab
    // completion snapshot; changes go through `run_complete`.
    pub(crate) fn completions_snapshot(&self) -> std::collections::HashMap<String, compgen::CompgenSpec> {
        self.completions.clone()
    }

    pub(crate) fn default_completion_snapshot(&self) -> Option<compgen::CompgenSpec> {
        self.default_completion.clone()
    }

    // Prints a compgen.rs ParseError as bash words it for `compgen` or
    // `complete` (`who` in the prefix) and returns 2, bash's usage-error
    // status.
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
        crate::sort::sort(&mut names);
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
        // Joined with spaces: the shell split `= 3 * (2 + 7)` into words,
        // but it's one expression.
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

    // Switches to the terminal's alternate screen (mode 1049) on the first
    // window command, so what was on screen stays in the terminal's own
    // scrollback. repl.rs draws the tab bar right after, since the window
    // list and terminal size are its. Also called by repl.rs on Ctrl+G,
    // which bypasses the command path (see `ensure_promoted`).
    pub(crate) fn promote_if_needed(&mut self) {
        if self.promoted.get() {
            return;
        }
        self.promoted.set(true);
        sh_print!(self, "\x1b[?1049h\x1b[2J\x1b[H");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }

    /// Rebuilds `FUNCNAME`, `BASH_SOURCE` and `BASH_LINENO` from the call
    /// stack on every push and pop, as real arrays, so every array form
    /// works unchanged. Index 0 is innermost, and each is one longer than
    /// the stack: the script itself, `main`, with the top-level file and
    /// line 0. Inside `inner`, called by `outer` from a sourced library,
    /// bash gives
    ///
    ///   FUNCNAME=(inner outer main)
    ///   BASH_SOURCE=(./lib.sh ./lib.sh main.sh)
    ///   BASH_LINENO=(7 2 0)
    ///
    /// so `BASH_SOURCE[i]` is where `FUNCNAME[i]` was defined and
    /// `BASH_LINENO[i]` the line in `BASH_SOURCE[i+1]` that called it; the
    /// defining file is recorded separately for that.
    fn refresh_call_arrays(&mut self) {
        let (mut names, mut sources, mut lines) = (Vec::new(), Vec::new(), Vec::new());
        for frame in self.call_stack.iter().rev() {
            names.push(frame.called.clone());
            sources.push(self.function_sources.get(&frame.called).cloned().unwrap_or_else(|| frame.source.clone()));
            lines.push(frame.call_line.to_string());
        }
        // The outer `main` frame exists only for a script: under `bash -c`
        // the same functions print `g f`, from a file `g f main`.
        if self.running_a_script {
            names.push("main".to_string());
            lines.push("0".to_string());
        }
        sources.push(self.script_name.clone());
        // At top level bash has no FUNCNAME at all, not `main`.
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
        self.arrays.insert(name.to_string(), crate::sort::btree_map(values.into_iter().enumerate()));
    }

    /// Whether `name` is a builtin that would run. `enable -n NAME` takes
    /// one out of service, and everything asking "is this a builtin" must
    /// agree with the dispatch: `type echo` and a pipeline stage both. A
    /// method, unlike `is_known_builtin`, since it depends on this shell's
    /// state.
    pub(crate) fn is_active_builtin(&self, name: &str) -> bool {
        // `win`/`window` are builtins only on the editor's colon line (see
        // their dispatch arm).
        if matches!(name, "win" | "window") && !self.restrict_to_builtins {
            return false;
        }
        is_known_builtin(name) && !self.disabled_builtins.contains(name)
    }

    // For repl.rs's Alt+Left/Right/Up directory navigation, which has an
    // absolute path: `cd`'s directory change, OLDPWD/PWD updates and error
    // reporting, without its argument parsing.
    pub fn cd_to(&mut self, path: &std::path::Path) -> i32 {
        crate::builtins::dirs::run_cd(self, &[path.to_string_lossy().into_owned()])
    }

    /// `target` resolved against `CDPATH`, and whether it came from there.
    /// Unchanged, with `false`, for an absolute path, one starting `.` or
    /// `..`, an unset or empty `CDPATH`, or a name no component holds. An
    /// empty component means the current directory, as in `PATH`, and is
    /// the only hit not announced: bash announces for a non-empty one, a
    /// literal `.` included.
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
            // Against this shell's cwd, not the process's, which differs
            // for other windows.
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

    // Moves this shell to another directory: the single path, keeping `cwd`,
    // `OLDPWD` and `PWD` in step and enforcing restricted mode, for `cd` and
    // the file browser's Ctrl-Y (repl.rs's `expand_browse_targets`).
    pub fn change_directory(&mut self, target: &std::path::Path) -> Result<(), String> {
        if self.opt_restricted {
            return Err(RESTRICTED.to_string());
        }
        let old = self.cwd.to_string_lossy().into_owned();
        // The logical path, not `getcwd`'s: the shell remembers the route,
        // so after `cd link`, `$PWD` ends in `link` and `cd ..` goes back
        // to where `link` sits. Only `pwd -P` resolves symlinks. Falls back
        // to the resolved path if the lexical one can't be entered, since
        // removing `..` textually can name somewhere that no longer exists.
        let logical = logical_path(&self.cwd, target);
        self.note_cwd_change();
        match std::env::set_current_dir(&logical) {
            Ok(()) => self.cwd = logical,
            Err(_) => {
                std::env::set_current_dir(target).map_err(|e| os_message(&e))?;
                self.cwd = std::env::current_dir().unwrap_or_else(|_| target.to_path_buf());
            }
        }
        let new = self.cwd.to_string_lossy().into_owned();
        // Into the variable table, which `$PWD` reads. Both are exported, so
        // children get them like any exported variable.
        let globals = Rc::make_mut(&mut self.globals);
        globals.insert("OLDPWD".to_string(), old.clone());
        globals.insert("PWD".to_string(), new.clone());
        let exported = Rc::make_mut(&mut self.exported_names);
        exported.insert("OLDPWD".to_string());
        exported.insert("PWD".to_string());
        {}
        // And into this session's remembered environment:
        // `sync_real_state_in` reapplies it before every command, so a
        // change made outside a command (the file browser's Ctrl-Y) would
        // otherwise be reverted at the next one. The same as
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
            // Discarded in an interactive shell, as bash does: `set +o`
            // still shows `noexec` off and `$-` gains no `n`. A `-n` given
            // at invocation applies before any prompt and is kept, as in
            // bash.
            'n' if !self.interactive => self.opt_noexec = on,
            'T' => self.opt_functrace = on,
            'E' => self.opt_errtrace = on,
            'u' => self.opt_nounset = on,
            'x' => self.opt_xtrace = on,
            'f' => self.opt_noglob = on,
            'm' => self.opt_monitor = on,
            'C' => self.opt_noclobber = on,
            // One-way: `set +r` is ignored, so restricted mode can't be
            // turned off. bash makes `set +r` an error instead; the
            // guarantee is what matters.
            'r' if on => self.opt_restricted = true,
            _ => {}
        }
    }

    // The read side of `apply_shell_option`, for `set -o`/`set +o` listings.
    // `None` for a name this shell doesn't act on (see SET_O_OPTIONS).
    pub(crate) fn shell_option_enabled(&self, name: &str) -> Option<bool> {
        Some(match name {
            "pipefail" => self.opt_pipefail,
            "noexec" => self.opt_noexec,
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
            "noexec" if !self.interactive => self.opt_noexec = on,
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

    /// Runs the handlers of trapped signals that arrived since the last check
    /// (see PENDING_SIGNALS for why traps run at checkpoints, not in the
    /// handler), once per top-level statement in `run_program`.
    ///
    /// Returns `Some(Exit)` when a handler called `exit`, which the caller
    /// must propagate: `trap "exit 1" TERM` is how a script makes itself
    /// interruptible. Only `Exit`: a trap body is its own context, so a
    /// `break` in one has no loop of the interrupted script's to break.
    #[must_use]
    fn check_pending_signals(&mut self) -> Option<ExecResult> {
        let pending = PENDING_SIGNALS.swap(0, std::sync::atomic::Ordering::SeqCst);
        if pending == 0 {
            return None;
        }
        let mut exit = None;
        for sig in 1..=64 {
            if pending & (1u64 << (sig - 1)) == 0 {
                continue;
            }
            if let Some(TrapAction::Run(code)) = self.traps.get(&sig).cloned() {
                // Every pending handler still runs once one has asked to
                // exit, since they arrived first. The first `exit` counts,
                // as with consecutive `exit`s.
                if let ExecResult::Exit(code) = self.run_source_here(&code, "trap")
                    && exit.is_none()
                {
                    exit = Some(ExecResult::Exit(code));
                }
            }
        }
        exit
    }

    /// `run_program` for a top-level source, where a statement can change how
    /// the rest reads; see `run_program_rereadable`.
    pub fn run_program_of_source(&mut self, prog: &Program, src: &str) -> ExecResult {
        self.run_program_rereadable(prog, Some((src, None)))
    }

    pub fn run_program(&mut self, prog: &Program) -> ExecResult {
        self.run_program_rereadable(prog, None)
    }

    /// `run_program`, able to reread the rest of its source. A statement
    /// defining an alias changes what the following statements mean, but
    /// they were already read: bash reads one statement at a time, while
    /// bish reads the whole source first. With `source` given, a statement
    /// that changes the alias table sends the remaining text back through
    /// the lexer.
    ///
    /// Only across a newline, bash's granularity: `alias hi=...; hi`
    /// doesn't expand, `alias hi=...<newline>hi` does (`end_line` tells
    /// them apart; see ListItem). The whole source is relexed, keeping line
    /// numbers absolute for the debugger and `$LINENO`, skipping statements
    /// already run, at the cost of a lexing pass only when aliases changed.
    fn run_program_rereadable(&mut self, prog: &Program, source: Option<(&str, Option<&str>)>) -> ExecResult {
        let mut result = ExecResult::Status(self.last_status);
        let Some(last_item) = prog.last() else { return result };
        // The alias table when this tree was read. A statement may change it,
        // which takes effect at the next newline, so it's compared per
        // statement, not reset.
        let parsed_with = source.is_some().then(|| self.alias_epoch());
        for (index, item) in prog.iter().enumerate() {
            self.current_line = item.line;
            // `set -n`: everything is parsed, which is all it asks, so
            // nothing runs. Before the signal check and debug hook, since a
            // trap handler is code too. `break`, since `set +n` couldn't
            // run to undo it; the status is the previous statement's (0 for
            // `bish -n`).
            if self.opt_noexec && !self.interactive {
                break;
            }
            if let Some(exit) = self.check_pending_signals() {
                return exit;
            }
            // Nothing reads this stage's output any more. A separate
            // process would have died of SIGPIPE; the in-process stage
            // stops here with that status.
            if broken_pipe_seen() {
                return ExecResult::Exit(128 + 13);
            }
            // The Rc is cloned rather than borrowed: the hook may call back
            // into Shell methods (the debugger's hover and print), and a
            // live borrow of `debug_hook` would panic then.
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
            // Only the last statement may hand a pty-backed job off to be
            // driven elsewhere, since `a; b` must still run `b`. And not
            // even it when a trap or `set -e` needs the status: `Fg`
            // reports 0 before the command runs, so an ERR trap would miss
            // a failure.
            let must_know_the_status = self.err_trap.is_some() || self.opt_errexit;
            if !std::ptr::eq(item, last_item) || must_know_the_status {
                result = self.settle_fg(result);
            }
            self.last_status = result.status();
            // A `set -u` violation from an expansion `run_single` doesn't
            // check (a `for` or `case` list, arithmetic); see
            // `pending_exit`. Usually `run_single` already returned
            // `ExecResult::Exit`.
            if let Some(exit) = self.take_pending_exit() {
                return exit;
            }
            // Cloned: `ExecResult` isn't `Copy` since `WindowAction`
            // carries a name.
            if result.is_signal() {
                return result;
            }
            // `set -e` aborts on a failing top-level statement, except
            // where POSIX exempts it (conditions, negated pipelines).
            // Checking each ListItem's overall and-or status also exempts
            // the non-final commands of a `&&`/`||` chain. ERR fires under
            // exactly those rules, as bash means it, whether or not errexit
            // is on: a script traps ERR so as not to exit.
            let exempt = std::mem::take(&mut self.errexit_exempt);
            if self.suppress_errexit == 0 && !exempt && result.status() != 0 {
                self.run_pseudo_trap(PseudoTrap::Err);
            }
            if self.opt_errexit && self.suppress_errexit == 0 && !exempt && result.status() != 0 {
                self.run_exit_trap();
                return ExecResult::Exit(result.status());
            }
            // This statement changed what the following ones say: reread
            // them and continue at the first not yet run, which starts the
            // next line.
            if let Some((src, label)) = source
                && let Some(parsed_with) = parsed_with
                && parsed_with != self.alias_epoch()
                && let Some(next) = prog.get(index + 1)
                && next.line > item.end_line
            {
                let resume = next.line;
                let reread = match self.parse_source(src, label) {
                    Ok(prog) => prog,
                    Err(status) => return status,
                };
                let rest: Program = reread.into_iter().skip_while(|i| i.line < resume).collect();
                return self.run_program_rereadable(&rest, source);
            }
        }
        result
    }

    fn run_and_or(&mut self, and_or: &AndOr, background: bool) -> ExecResult {
        // Every member but the last is exempt from `set -e`, and the
        // exemption reaches inside it: in bash
        // `set -e; f() { false; }; f && echo ok` carries on, so `f`'s
        // failing body mustn't abort the shell.
        let last = and_or.rest.len();
        if last > 0 {
            self.suppress_errexit += 1;
        }
        let mut result = self.run_pipeline(&and_or.first, background);
        if last > 0 {
            self.suppress_errexit -= 1;
            // Something follows, so a foreground pty job can't be bubbled
            // out to be driven elsewhere; see `settle_fg`.
            result = self.settle_fg(result);
        }
        // Set after the member runs, since a function body's lists write it
        // too. A single pipeline is its own final command; in a chain only
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
                    result = self.settle_fg(result);
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

    // `time`'s report: bash's default is a blank line and three `0m0.000s`
    // rows; `-p` is POSIX's three bare seconds. `TIMEFORMAT` replaces it with
    // the subset people write: `%R`, `%U`, `%S`, `%P` for the percentage, `l`
    // for the `0m0.000s` form, a precision digit, and `%%`.
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
                // `0m0.000s`: whole minutes, then the seconds, unpadded: 65
                // seconds is `1m5.000s`.
                true => out.push_str(&format!("{}m{:.precision$}s", (value as i64) / 60, value % 60.0)),
                false => out.push_str(&format!("{value:.precision$}")),
            }
        }
        out.push('\n');
        out
    }

    fn run_pipeline_untimed(&mut self, pipeline: &Pipeline, background: bool) -> ExecResult {
        if pipeline.negate {
            // POSIX exempts a `!`-negated pipeline's failure from `-e`.
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
            // A one-command pipeline still sets PIPESTATUS, as in bash:
            // `${PIPESTATUS[0]}` after a plain command is `$?`.
            if let ExecResult::Status(code) = result {
                self.set_pipestatus(&[code]);
            }
            return result;
        }
        ExecResult::Status(self.run_multi(&pipeline.commands, background))
    }

    /// Publishes PIPESTATUS, each stage's status in order, so `cmd | tee log`
    /// can still tell whether `cmd` failed.
    fn set_pipestatus(&mut self, codes: &[i32]) {
        // Nearly every command is one stage, so usually one status replaces
        // one: writing over the existing entry avoids three allocations per
        // command. The keys are 0..n in both, so equal lengths line up.
        if let Some(existing) = self.arrays.get_mut("PIPESTATUS")
            && existing.len() == codes.len()
        {
            use std::fmt::Write;
            for (slot, code) in existing.values_mut().zip(codes) {
                slot.clear();
                let _ = write!(slot, "{code}");
            }
            return;
        }
        let map: std::collections::BTreeMap<usize, String> = crate::sort::btree_map(codes.iter().enumerate().map(|(i, c)| (i, c.to_string())));
        self.arrays.insert("PIPESTATUS".to_string(), map);
    }

    // Dispatches on `cmd`'s variant once its own redirects are handled.
    // Separate so `run_in_child_shell`'s `ChildSource::Parsed` case can run
    // a compound's content directly; going through `command_own_redirects`
    // again would see the same redirects and recurse forever.
    pub(crate) fn run_command_body(&mut self, cmd: &parser::Command, background: bool) -> ExecResult {
        match cmd {
            parser::Command::Simple(sc) => {
                let depth = self.underscore_pending.len();
                let result = self.run_single(sc, background);
                // What the body pushed it has already popped, so what sits
                // at `depth` is this command's own.
                if self.underscore_pending.len() > depth {
                    let value = self.underscore_pending[depth].clone();
                    self.underscore_pending.truncate(depth);
                    self.assign_var("_", value);
                }
                result
            }
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
                    sh_eprintln!(self, "bish: ((: {}: {}", raw, e);
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

    // `read` is a builtin and can't use the child-process Stdio redirects,
    // so its input source is worked out here; `read x <<< "..."` is too
    // common to leave out.
    fn read_input_source(&mut self, cmd: &SimpleCommand) -> Box<dyn std::io::BufRead> {
        for r in cmd.redirects.iter().rev() {
            match r {
                Redirect::HereString(w) => {
                    let mut content = self.expand_word(w);
                    content.push('\n');
                    return Box::new(std::io::Cursor::new(content.into_bytes()));
                }
                Redirect::HereDoc(w, _) => {
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
                // `read <&3`: fd 3 was opened by `exec 3<file` and is a
                // real fd, so read it directly.
                Redirect::FdDup { fd: 0, target, .. } => {
                    return Box::new(UnbufferedFd::new(*target as i32));
                }
                Redirect::FdDupWord { fd: 0, word, .. } => {
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

    // Where a builtin reads when the command has no `<` of its own: the
    // enclosing construct's stdin override if any (a `while read` loop's
    // `< file` is on the loop, not on `read`), else fd 0. `select` reads
    // through here too, so `select ... done <<< "1"` sees the here-string.
    fn current_stdin_reader(&self) -> Box<dyn std::io::BufRead> {
        if let Some(o) = &self.stdio_override {
            if let Some(state) = &o.borrow().stdin {
                return Box::new(SharedStdinReader { state: state.clone(), local: Vec::new() });
            }
        }
        // One byte per syscall; see UnbufferedFd.
        Box::new(UnbufferedFd::new(0))
    }

    /// `run_source_here` for a body that runs repeatedly: the subshell forms
    /// keep their body as text, and reparsing it each time (`v=$(true)` in a
    /// loop) was a substitution's largest cost.
    pub(crate) fn run_source_cached(&mut self, src: &str, label: &str) -> ExecResult {
        self.run_source_cached_maybe_reread(src, label, true)
    }

    /// The same for a body that isn't its own parse unit, so mustn't be
    /// reread part-way; see `ChildBody::Enclosed`.
    fn run_source_enclosed(&mut self, src: &str, label: &str) -> ExecResult {
        self.run_source_cached_maybe_reread(src, label, false)
    }

    fn run_source_cached_maybe_reread(&mut self, src: &str, label: &str, reread: bool) -> ExecResult {
        let source = reread.then_some((src, Some(label)));
        match self.cached_program(src, label) {
            Ok(Some(prog)) => self.run_program_rereadable(&prog, source),
            // Not cacheable: parse normally, with the same rereadability.
            Ok(None) => match self.parse_source(src, Some(label)) {
                Ok(prog) => self.run_program_rereadable(&prog, source),
                Err(status) => status,
            },
            Err(status) => status,
        }
    }

    /// The cache lookup for `run_source_cached`. `Ok(None)`: not cacheable;
    /// `Err`: the status of a body that failed to parse.
    fn cached_program(&mut self, src: &str, label: &str) -> Result<Option<Rc<parser::Program>>, ExecResult> {
        // Alias expansion is the one parsing step reading shell state, so a
        // cached tree is sound only while it can't apply, the condition
        // `expand_aliases` itself checks.
        if self.shopt_is_on("expand_aliases") && !self.aliases.is_empty() {
            return Ok(None);
        }
        if let Some(hit) = self.parse_cache.borrow().get(src) {
            return Ok(Some(Rc::clone(hit)));
        }
        let prog = match crate::lexer::Lexer::new(src).tokenize() {
            Ok(toks) => match crate::parser::Parser::new(toks).parse_program() {
                Ok(prog) => Rc::new(prog),
                Err(e) => {
                    sh_eprintln!(self, "bish: {}: syntax error: {}", label, e);
                    return Err(ExecResult::Status(2));
                }
            },
            Err(e) => {
                sh_eprintln!(self, "bish: {}: syntax error: {}", label, e);
                return Err(ExecResult::Status(2));
            }
        };
        // A script has few distinct subshell bodies; the cap only stops a
        // generated one growing this without bound.
        let mut cache = self.parse_cache.borrow_mut();
        if cache.len() >= 512 {
            cache.clear();
        }
        cache.insert(src.to_string(), Rc::clone(&prog));
        Ok(Some(prog))
    }

    // Runs source text in this shell (for `eval`, `source` and main.rs's
    // config loading), so it can set variables, functions and the cwd here,
    // unlike a subshell.
    pub(crate) fn run_source_here(&mut self, src: &str, label: &str) -> ExecResult {
        // A file sourcing itself crashes like a function calling itself
        // without any function call, so it needs its own check. No
        // `nesting_unwind`: each level's `source` returns when the one
        // below fails. bash dumps core on this.
        if crate::stackguard::nearly_exhausted() {
            sh_eprintln!(self, "bish: {}: maximum source nesting level exceeded", label);
            if !self.interactive {
                self.pending_exit = Some(1);
            }
            return ExecResult::Status(1);
        }
        match self.parse_source(src, Some(label)) {
            // With the source, since a statement may define an alias and
            // change how the rest reads; see `run_program_rereadable`.
            Ok(prog) => self.run_program_rereadable(&prog, Some((src, Some(label)))),
            Err(status) => status,
        }
    }

    /// Text to a tree, reporting a syntax error as callers want. Separate
    /// because a source is reread when its statements change aliases.
    fn parse_source(&mut self, src: &str, label: Option<&str>) -> Result<Program, ExecResult> {
        match crate::lexer::Lexer::new(src).tokenize() {
            Ok(toks) => match crate::parser::Parser::new(self.expand_aliases(toks)).parse_program() {
                Ok(prog) => Ok(prog),
                Err(e) => Err(self.report_syntax_error(label, &e)),
            },
            Err(e) => Err(self.report_syntax_error(label, &e)),
        }
    }

    /// A named source says its name; the top-level `-c` or script path
    /// doesn't, and scripts match on that wording.
    fn report_syntax_error(&mut self, label: Option<&str>, e: &str) -> ExecResult {
        match label {
            Some(label) => sh_eprintln!(self, "bish: {}: syntax error: {}", label, e),
            None => sh_eprintln!(self, "bish: syntax error: {}", e),
        }
        ExecResult::Status(2)
    }

    // Functions and locals live only in this process, so a re-exec'd child
    // starts without them, where a fork would inherit them (`$(fib $((n-1)))`
    // with a local `n` works in bash). Redeclaring visible locals and every
    // function at the top of the child's script closes the gap without
    // fork(2) or an IPC channel.
    pub(crate) fn functions_preamble(&self) -> String {
        let mut s = String::new();
        let mut flattened: HashMap<&str, &str> = HashMap::new();
        // Unexported globals: exported ones are already in the child's
        // environment, and without this a re-exec'd stage would start with no
        // unexported ones.
        for (k, v) in self.globals.iter().chain(self.underscore.as_ref().map(|v| (&*UNDERSCORE, v))) {
            if !self.exported_names.contains(k) {
                flattened.insert(k.as_str(), v.as_str());
            }
        }
        // Locals over globals: the child belongs to this frame.
        for scope in &self.var_scopes {
            for (k, v) in scope {
                // A declared but unset local has no assignment to replay:
                // `k=` would make it set to empty in the child.
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
            // The child seeds this one itself and makes it readonly, so
            // redeclaring it would be refused.
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
                end_line: 0,
            }]));
        }
        // The directory stack, traps and completion specs, which a
        // re-exec'd construct couldn't otherwise see. Only a coproc and a
        // backgrounded subshell still re-exec, and they start once, so
        // replaying these is paid once. Not `jobs`: bash's coproc can't see
        // the job table either, and inventing entries for processes the
        // child doesn't own would be worse. In reverse, since each
        // `pushd -n` goes on the front.
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
        // Whatever `shopt -s`/`-u` changed from its default.
        for (name, on) in &self.shopt_options {
            if shopt_default_on(name) == Some(*on) {
                continue;
            }
            s.push_str(if *on { "shopt -s " } else { "shopt -u " });
            s.push_str(name);
            s.push('\n');
        }
        // The `set` options last: an earlier `set -e` would let the
        // preamble's own assignments abort the child, and `set -u` would
        // trip on a name not yet assigned.
        for name in SET_O_OPTIONS {
            if self.shell_option_enabled(name) == Some(true) {
                s.push_str("set -o ");
                s.push_str(name);
                s.push('\n');
            }
        }
        // `set -r` after even those: it makes the preamble illegal, since
        // `PATH` is readonly in a restricted shell. It has no `-o` name, so
        // it isn't in SET_O_OPTIONS, and without this every re-exec'd
        // construct (`( ... ) &`, `coproc`) would run unrestricted.
        // In-process children copy the flag in `new_virtual_child`.
        if self.opt_restricted {
            s.push_str("set -r\n");
        }
        s
    }

    // Runs `raw` in an in-process Shell from `new_virtual_child`, the
    // primitive behind every foreground `( )`, `$( )`, `<( )` and `>( )`:
    // the child already has real copies of variables, arrays and functions,
    // so no preamble is needed. A coproc and a backgrounded subshell still
    // re-exec, since they outlive this call.
    //
    // `stdio.stdout`, if given, is where this construct's output goes (a
    // redirect or a substitution's capture); `None` means wherever the
    // parent's goes, as a fork shares fd 1, so a bare `( cmd )` inside
    // `$( )` lands in the outer capture. Likewise `stdio.stdin`.
    // `stdio.stderr` and the dup flags come only from
    // `run_compound_redirected`.
    fn run_in_child_shell(&mut self, raw: &str, stdio: ChildStdio) -> ExecResult {
        self.run_body_in_child_shell(ChildBody::Source(raw), stdio)
    }

    /// `run_in_child_shell` for a `( )`, whose body isn't a source of its
    /// own; see `ChildBody::Enclosed`.
    fn run_enclosed_in_child_shell(&mut self, raw: &str, stdio: ChildStdio) -> ExecResult {
        self.run_body_in_child_shell(ChildBody::Enclosed(raw), stdio)
    }

    /// `run_in_child_shell` for something already parsed: a pipeline stage
    /// arrives as a `parser::Command`, and serializing it only to reparse it
    /// would be wasted, and lossy where the round trip isn't exact.
    fn run_command_in_child_shell(&mut self, cmd: &parser::Command, stdio: ChildStdio) -> ExecResult {
        self.run_body_in_child_shell(ChildBody::Parsed(cmd), stdio)
    }

    /// A virtual child wired to `stdio`, returned rather than run: a pipeline
    /// builds and connects every stage before any runs, since each blocks on
    /// a pipe another holds.
    fn child_for_stdio(&mut self, mut stdio: ChildStdio) -> Shell {
        let mut child = self.new_virtual_child();

        let effective_stdin: Option<Rc<RefCell<SharedReaderState>>> = match stdio.stdin.take() {
            Some(f) => {
                Some(Rc::new(RefCell::new(SharedReaderState { file: f, pending: Vec::new(), coroutines: Rc::clone(&self.background_coroutines) })))
            }
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdin.clone()),
        };
        let effective_stdout: Option<Rc<Capture>> = match &stdio.stdout {
            Some(c) => Some(Rc::clone(c)),
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdout.clone()),
        };
        let effective_stderr = self.effective_stderr(&stdio, &effective_stdout);
        child.stdio_override = if effective_stdin.is_some() || effective_stdout.is_some() || effective_stderr.is_some() {
            Some(Rc::new(RefCell::new(StdioOverride { stdin: effective_stdin, stdout: effective_stdout, stderr: effective_stderr })))
        } else {
            None
        };
        // No stdout or stderr override: the child's sink is exactly the
        // parent's current one, not a wrapper.
        child.sink = if stdio.redirects_output() {
            OutputSink::Builtin { previous: Box::new(self.sink.clone()), stdout: stdio.sink_stdout(), stderr: stdio.sink_stderr() }
        } else {
            self.sink.clone()
        };
        child
    }

    fn run_body_in_child_shell(&mut self, body: ChildBody<'_>, stdio: ChildStdio) -> ExecResult {
        let mut child = self.child_for_stdio(stdio);
        // A subshell (`$( )` and `( )`, arriving as text) resets the traps
        // it caught, so DEBUG doesn't fire per command inside a `$( )`, and
        // ERR not both inside `( false )` and for the subshell, as in bash
        // (EXIT has its own rule; see `run_exit_trap`). A pipeline stage,
        // arriving parsed, doesn't reset them: bash fires DEBUG for each
        // stage. A redirected group isn't a subshell at all.
        if matches!(body, ChildBody::Source(_) | ChildBody::Enclosed(_)) {
            child.debug_trap = None;
            child.err_trap = None;
            child.return_trap = None;
        }
        // A `( )` and a pipeline stage aren't the login shell, but a `$( )`
        // is, as in bash: `x=$(logout 3)` leaves with 3, `( logout 3 )`
        // refuses.
        if matches!(body, ChildBody::Enclosed(_) | ChildBody::Parsed(_)) {
            child.login_shell = false;
            // ...and one bash forks, which keeps its redirects (see
            // `run_exit_trap`).
            child.forked_subshell = true;
        }

        // Nothing is snapshotted here. The cwd, umask and fds 0-2 are
        // process-wide and shared with the parent, so a `cd` or a bare
        // `exec > file` inside must not leak out; each is recorded by
        // whatever moves it and only that is put back (see ProcessRestore).
        // Variables need nothing: `child` owns its `globals`, and spawned
        // processes get those (see `Shell::command`), not `environ`.

        let result = match body {
            ChildBody::Source(raw) => child.run_source_cached(raw, "subshell"),
            ChildBody::Enclosed(raw) => child.run_source_enclosed(raw, "subshell"),
            ChildBody::Parsed(cmd) => crate::builtins::shell::run_command(&mut child, cmd, false),
        };
        // A pty-backed job may be handed back as `ExecResult::Fg` for
        // repl.rs to drive, which a subshell's last statement is allowed to
        // do. Nothing can drive this one: it belongs to `child`, about to
        // be dropped, which would lose the job and report 0. So it's
        // settled here.
        let result = child.settle_fg(result);
        // bash runs a subshell's EXIT trap when it finishes normally too
        // (`(trap "echo bye" EXIT; echo hi)` prints both).
        // `ExecResult::Exit`'s producer already ran it, so only the other
        // outcomes run it here.
        if !matches!(result, ExecResult::Exit(_)) {
            child.run_exit_trap();
        }

        // What the body said it moved, taken after the EXIT trap, which can
        // move things too.
        let moved = std::mem::take(&mut child.process_restore);
        if let Some(d) = moved.cwd {
            let _ = std::env::set_current_dir(d);
        }
        if let Some(m) = moved.umask {
            crate::platform::set_umask(m);
        }
        if let Some(fds) = moved.fd012 {
            restore_fd012(fds);
        }

        match result {
            // A subshell's `exit`, `set -e` or `set -u` mustn't end the
            // enclosing shell, as with bash's fork for `(exit 3)`. The EXIT
            // trap already ran.
            ExecResult::Exit(code) => ExecResult::Status(code),
            other => other,
        }
    }

    // `run_compound_redirected`'s in-process primitive, not
    // `run_in_child_shell`: a redirected compound (`{ ...; } > file`,
    // `while ...; done < file`) isn't a subshell, and shares all state with
    // the shell, as in bash; only its stdio differs. So `cmd` runs on
    // `self` with `sink` and `stdio_override` swapped for the call, as
    // `push_builtin_output_sink` does for one builtin.
    fn run_with_redirected_stdio(&mut self, cmd: &parser::Command, stdio: ChildStdio) -> ExecResult {
        let saved = self.install_redirected_stdio(stdio);
        let result = self.run_command_body(cmd, false);
        self.restore_redirected_stdio(saved);
        result
    }

    // The install and restore halves of `run_with_redirected_stdio`, so a
    // function call can use them: `f > out` redirects the whole body as
    // `{ ...; } > out` does, builtins and externals alike.
    fn install_redirected_stdio(&mut self, mut stdio: ChildStdio) -> (OutputSink, Option<Rc<RefCell<StdioOverride>>>) {
        let saved_sink = self.sink.clone();
        let saved_stdio_override = self.stdio_override.clone();

        let effective_stdin: Option<Rc<RefCell<SharedReaderState>>> = match stdio.stdin.take() {
            Some(f) => {
                Some(Rc::new(RefCell::new(SharedReaderState { file: f, pending: Vec::new(), coroutines: Rc::clone(&self.background_coroutines) })))
            }
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdin.clone()),
        };
        let effective_stdout: Option<Rc<Capture>> = match &stdio.stdout {
            Some(c) => Some(Rc::clone(c)),
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdout.clone()),
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

    // A foreground `( )` runs in-process (see `run_in_child_shell`); a
    // backgrounded one re-execs bish on its source, since it has to keep
    // running alongside the parent.
    fn run_subshell(&mut self, raw: &str, background: bool) -> i32 {
        if !background {
            return self.run_enclosed_in_child_shell(raw, ChildStdio::default()).status();
        }
        self.spawn_background_script(raw, format!("({})", raw))
    }

    /// Runs `raw` as a background job: a real child with this shell's
    /// functions in front, its own pty, and a job table entry for `$!`,
    /// `jobs` and `wait`. Used for `( ) &` and for `&` on anything else
    /// that runs in this shell (a group, loop, builtin, function). `label`
    /// is what `jobs` shows.
    fn spawn_background_script(&mut self, raw: &str, label: String) -> i32 {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                sh_eprintln!(self, "bish: subshell: {}", e);
                return 1;
            }
        };
        let script = self.functions_preamble() + raw;
        let mut command = self.reexec_running(exe, script);
        command.current_dir(&self.cwd);
        // On its own pty, as `run_single`'s background spawn is (see
        // `use_pty`): inherited stdio writes onto the real screen, which
        // the compositor's next redraw from the grid wipes. No
        // process-group isolation conflicts with `spawn_attached`'s setsid
        // here.
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

    // `coproc [NAME] command` (see `Command::Coproc` for the grammar): runs
    // `body` in the background wired to two pipes: NAME[0] reads its stdout,
    // NAME[1] writes its stdin, NAME_PID is its pid. Also an ordinary job in
    // `jobs` and `wait`, as in bash.
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
        let mut command = self.reexec_running(exe, script);
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

    // A numbered-descriptor redirect this process can make on itself and
    // undo: one naming only fds 3 and up, so nothing the shell's own stdio
    // handling relies on moves.
    fn stays_in_process(r: &Redirect) -> bool {
        match r {
            Redirect::FdOut { fd, .. }
            | Redirect::FdIn { fd, .. }
            | Redirect::FdInOut { fd, .. }
            | Redirect::FdClose { fd }
            | Redirect::FdHereString { fd, .. }
            | Redirect::FdHereDoc { fd, .. } => *fd >= 3,
            Redirect::FdDup { fd, target, .. } => *fd >= 3 && *target >= 3,
            _ => false,
        }
    }

    // A compound with numbered-descriptor redirects, run in this shell. The
    // descriptors move on the process itself, where builtins and commands
    // inside find them, and are put back as they were, open or closed. The
    // 0-2 redirects beside them go the ordinary in-process way. In a separate
    // bish, `{ x=1; } 3>f` would leave `x` unset.
    fn run_with_numbered_fds(&mut self, cmd: &parser::Command, numbered: &[Redirect], simple: &[Redirect]) -> ExecResult {
        // Each target as it was: a close-on-exec copy when open, so nothing
        // started inside inherits it, or -1 when closed.
        fn save(targets: impl Iterator<Item = i32>) -> Vec<(i32, i32)> {
            let mut saved: Vec<(i32, i32)> = Vec::new();
            for fd in targets {
                if !saved.iter().any(|(target, _)| *target == fd) {
                    saved.push((fd, crate::platform::duplicate_above(fd, 10).unwrap_or(-1)));
                }
            }
            saved
        }
        fn restore(saved: Vec<(i32, i32)>) {
            for (fd, copy) in saved.into_iter().rev() {
                match copy >= 0 {
                    true => {
                        crate::platform::duplicate_onto(copy, fd);
                        crate::platform::close_fd(copy);
                    }
                    false => crate::platform::close_fd(fd),
                }
            }
        }
        let redirs = match self.resolve_redirect_list(numbered) {
            Ok(r) => r,
            Err(e) => {
                sh_eprintln!(self, "bish: {}", e);
                return ExecResult::Status(1);
            }
        };
        let saved = save(redirs.actions.iter().map(FdAction::target));
        if let Err(e) = apply_fds_to_self(redirs.actions) {
            restore(saved);
            sh_eprintln!(self, "bish: {}", e);
            return ExecResult::Status(1);
        }
        let result = match self.resolve_simple_redirects_for_compound(simple) {
            Ok(stdio) => self.run_with_redirected_stdio(cmd, stdio),
            Err(e) => {
                sh_eprintln!(self, "bish: {}", e);
                ExecResult::Status(1)
            }
        };
        restore(saved);
        result
    }

    // Closes a redirect scope `run_exit_trap` waits on. When it's the last
    // and the body was the shell exiting, the deferred trap runs now, with
    // the redirects bash would undo first already restored.
    fn leave_redirect_scope(&mut self, result: &ExecResult) {
        self.redirect_scopes = self.redirect_scopes.saturating_sub(1);
        if self.redirect_scopes == 0 && std::mem::take(&mut self.exit_trap_deferred) && matches!(result, ExecResult::Exit(_)) {
            self.run_exit_trap();
        }
    }

    // A compound (`if`, `while`, `for`, `case`, a group) with a trailing
    // redirect. Foreground with 0-2 redirects it runs in-process, so its
    // variable and cwd changes stay visible, as a `{ }` group isn't a
    // subshell. A numbered fd this shell can't move on itself, or a
    // background run, falls back to a re-exec'd bish.
    pub(crate) fn run_compound_redirected(&mut self, cmd: &parser::Command, redirects: &[Redirect], background: bool) -> ExecResult {
        // Both in-process ways are redirect scopes in `run_exit_trap`'s
        // sense; the separate-process fallback isn't, since an `exit` there
        // leaves only that process.
        if !background && Self::compound_redirects_are_simple(redirects) {
            self.redirect_scopes += 1;
            let result = match self.resolve_simple_redirects_for_compound(redirects) {
                Ok(stdio) => self.run_with_redirected_stdio(cmd, stdio),
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    ExecResult::Status(1)
                }
            };
            self.leave_redirect_scope(&result);
            return result;
        }
        if !background && redirects.iter().all(|r| Self::stays_in_process(r) || Self::compound_redirects_are_simple(std::slice::from_ref(r))) {
            let (numbered, simple): (Vec<Redirect>, Vec<Redirect>) = redirects.iter().cloned().partition(Self::stays_in_process);
            self.redirect_scopes += 1;
            let result = self.run_with_numbered_fds(cmd, &numbered, &simple);
            self.leave_redirect_scope(&result);
            return result;
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
        // The body only: the compound's own redirects are applied to the
        // child here, and writing them into its script too would apply them
        // twice and spawn another child.
        let script = self.functions_preamble() + &crate::serialize::serialize_command_body(cmd);
        let mut command = self.reexec_running(exe, script);
        command.current_dir(&self.cwd);
        // The streams the redirects didn't claim go to a pty of its own, as
        // for `run_multi`'s background pipelines (see `background_pty`):
        // inherited, they'd be wiped by the next repaint. Plain fds, not
        // `spawn_attached`, whose setsid would undo the process group
        // below.
        // A backgrounded pipeline's own pty (see `background_pty`): the first
        // stage reads it, the last writes it, and every stderr goes there.
        // Plain fds, not `spawn_attached`, whose setsid would undo the
        // process group below.
        let bg_pty = if background { self.background_pty() } else { None };
        let bg_slave = |pty: &Option<pty::Pty>| -> Option<Stdio> {
            let p = pty.as_ref()?;
            std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok().map(Stdio::from)
        };
        // Only a foreground run (here for a numbered-fd redirect with no
        // in-process model; see `compound_redirects_are_simple`) honours an
        // enclosing capture.
        if background {
            command.stdin(bg_slave(&bg_pty).unwrap_or_else(Stdio::inherit));
            command.stdout(bg_slave(&bg_pty).unwrap_or_else(Stdio::inherit));
        } else {
            command.stdin(self.spawn_stdin_stdio());
            command.stdout(self.spawn_stdout_stdio());
        }
        command.stderr(bg_slave(&bg_pty).unwrap_or_else(|| self.spawn_stderr_stdio()));
        apply_fd_redirects(&mut command, redirs.actions);
        // A process group for a backgrounded redirected compound only, set
        // in the child and the parent after spawn to avoid the race, as
        // `run_single` and `run_multi` do, so `kill %N` and `bg` reach it.
        // A foreground run stays in bish's group: there's no `tcsetpgrp` or
        // stop handling here to route Ctrl-C to it.
        if background && self.opt_monitor {
            #[cfg(unix)]
            unsafe {
                command.pre_exec(|| {
                    crate::platform::set_process_group(0, 0);
                    crate::platform::default_signal(crate::term::SIGTTIN);
                    crate::platform::default_signal(crate::term::SIGTTOU);
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
                        crate::platform::set_process_group(pid, pid);
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

    /// Drops every NUL from a command substitution's output, saying so
    /// once, as bash does. A NUL can't survive into a word, which becomes
    /// an `execve` argument that stops at the first one, and binary output
    /// where a string was expected deserves a line on stderr.
    fn strip_nuls_from_substitution(&mut self, text: String) -> String {
        if !text.contains('\0') {
            return text;
        }
        sh_eprintln!(self, "bish: line {}: warning: command substitution: ignored null byte in input", self.current_line);
        text.replace('\0', "")
    }

    fn run_command_substitution(&mut self, raw: &str) -> String {
        // `$(<file)`: bash's no-fork file read (a pidfile, something in
        // /proc), parsed as a substitution holding only a redirect, which
        // would run nothing.
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
            // The trailing newlines every substitution strips.
            while text.ends_with('\n') {
                text.pop();
            }
            self.last_subst_status = Some(0);
            return self.strip_nuls_from_substitution(text);
        }
        // No file until one is needed: a body of builtins (`$(pwd)`) writes
        // through this shell's sink; a descriptor appears only if it spawns
        // something. See Capture.
        let capture = Capture::memory();
        // `set -e` doesn't reach inside `$( )` unless
        // `shopt -s inherit_errexit`, which exists for that reason. A `( )`
        // does inherit it, so this is specific to substitutions.
        let errexit = self.opt_errexit;
        if !self.shopt_is_on("inherit_errexit") {
            self.opt_errexit = false;
        }
        let result = self.run_in_child_shell(raw, ChildStdio { stdout: Some(Rc::clone(&capture)), ..Default::default() });
        self.opt_errexit = errexit;
        self.last_subst_status = Some(result.status());
        let mut s = capture.take_text();
        while s.ends_with('\n') {
            s.pop();
        }
        self.strip_nuls_from_substitution(s)
    }

    /// Runs `cmd` with `input` on stdin and returns its stdout, stderr and
    /// status: the editor's `:[range]!cmd` filter. Built like `$( )`,
    /// because an external's stdout goes to a real descriptor, which
    /// `OutputSink::Capture` can't catch. The trailing newline is kept,
    /// since these are lines going back into a buffer. stderr comes back
    /// separately: on the alternate screen it would be painted over, and a
    /// failed filter still replaces the range (as in vim), so it must say
    /// why.
    pub(crate) fn filter_through_command(&mut self, cmd: &str, input: &str) -> std::io::Result<(String, String, i32)> {
        use std::io::{Seek, SeekFrom, Write};
        // An anonymous file if the kernel provides one, else a named
        // temporary, as in `run_command_substitution`.
        let mut stdin = match capture_file() {
            Some(f) => f,
            None => {
                let (f, path) = crate::platform::exclusive_temp_file("procsub").ok_or_else(|| std::io::Error::other("no temporary file"))?;
                // The name has done its job; the child gets the descriptor.
                let _ = std::fs::remove_file(&path);
                f
            }
        };
        stdin.write_all(input.as_bytes())?;
        stdin.seek(SeekFrom::Start(0))?;

        let out = Captured::open()?;
        let err = Captured::open()?;
        let result = self.run_in_child_shell(
            cmd,
            ChildStdio { stdin: Some(stdin), stdout: Some(Capture::file(out.writer()?)), stderr: Some(err.writer()?), ..Default::default() },
        );
        Ok((out.read(), err.read(), result.status()))
    }

    /// The single external command a `<( )` body is, if that's all it is.
    /// The common shapes (`<(yes)`, `<(seq 1 10)`, `<(cat f)`) need no
    /// shell, and a coroutine spawning a process would have to wait for it,
    /// which a coroutine can't. Spawned directly, as bash does, it's a real
    /// process writing into the pipe that ends when the reader goes.
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
        // A redirect or assignment prefix needs the shell.
        if !sc.redirects.is_empty() || !sc.assigns.is_empty() {
            return None;
        }
        let argv = self.expand_words(&sc.words);
        (!argv.is_empty()).then_some(argv)
    }

    // `<(cmd)`: substitutes `/dev/fd/N` for a pipe that `cmd` writes into
    // concurrently, as a process or a coroutine.
    fn run_proc_sub_in(&mut self, raw: &str) -> String {
        let (read_end, write_end) = match make_pipe() {
            Ok(pair) => pair,
            Err(e) => {
                sh_eprintln!(self, "bish: process substitution: {}", e);
                return String::new();
            }
        };
        match self.proc_sub_plain_command(raw) {
            // A real command, a real process, as in bash.
            Some(argv) => {
                let mut command = self.command(&argv[0]);
                command.args(&argv[1..]);
                command.stdout(stdio_from_owned(write_end));
                command.stdin(self.spawn_stdin_stdio());
                match command.spawn() {
                    Ok(child) => self.proc_sub_children.push((child, true)),
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}: {}", argv[0], os_message(&e));
                        return String::new();
                    }
                }
            }
            // Anything needing a shell runs as a coroutine, given time when
            // this shell would otherwise wait.
            None => {
                let mut child = self.new_virtual_child();
                let body = raw.to_string();
                let started = self.background_coroutines.borrow_mut().add_with_fds(
                    move || {
                        // Armed as for a pipeline stage: `EPIPE` is the
                        // only way this body learns its reader went, and an
                        // unbounded producer would otherwise spin forever
                        // inside one resume.
                        arm_broken_pipe();
                        child.run_source_cached(&body, "process substitution");
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

        // `/dev/fd/N`, not a FIFO: opening a FIFO blocks until the other
        // end opens, and the other end is a coroutine that can't run while
        // this one blocks. The consumer opens the name, and an external
        // inherits this descriptor across `exec`, so it mustn't be
        // close-on-exec. Cleared after the producer starts, or it would
        // hold its own output pipe open.
        clear_cloexec(std::os::fd::AsRawFd::as_raw_fd(&read_end));
        let name = format!("/dev/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&read_end));
        self.proc_sub_pipes.push(read_end);
        name
    }

    /// `child.wait()`, giving live coroutines the time otherwise spent
    /// blocked: the command reading a `<( )` is what's waited for, and its
    /// producer must run meanwhile. With nothing live, a plain wait.
    fn wait_pumping(&self, child: &mut std::process::Child) -> std::io::Result<std::process::ExitStatus> {
        // A coroutine can't wait: blocking takes every stage down,
        // including the one that would let this child finish (`<(yes)`
        // deadlocked waiting for `yes` while `head` had exited).
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
                // Nothing left to run, so block instead of spinning on
                // `try_wait`.
                return child.wait();
            }
            self.pump_coroutines();
        }
    }

    /// Gives coroutines that outlive their command a turn, wherever the shell
    /// would otherwise wait in the kernel, which is when what they feed isn't
    /// running either.
    pub(crate) fn pump_coroutines(&self) {
        let Ok(mut scheduler) = self.background_coroutines.try_borrow_mut() else {
            // Already inside a step: a coroutine reached a pumping point.
            return;
        };
        scheduler.step();
    }

    /// Whether anything is waiting for a turn.
    pub(crate) fn has_live_coroutines(&self) -> bool {
        self.background_coroutines.try_borrow().is_ok_and(|s| !s.is_idle())
    }

    /// `>(cmd)`: the mirror of `<(cmd)`. The command writes to the name
    /// while `cmd` reads it concurrently, so `echo hi > >(cat); echo done`
    /// can print `done hi` as bash does.
    fn run_proc_sub_out(&mut self, raw: &str) -> String {
        let (read_end, write_end) = match make_pipe() {
            Ok(pair) => pair,
            Err(e) => {
                sh_eprintln!(self, "bish: process substitution: {}", e);
                return String::new();
            }
        };
        match self.proc_sub_plain_command(raw) {
            // A real command reading the pipe, as in bash, ending when the
            // writing end closes.
            Some(argv) => {
                let mut command = self.command(&argv[0]);
                command.args(&argv[1..]);
                command.stdin(stdio_from_owned(read_end));
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
                        // Armed as for a pipeline stage; see
                        // `run_proc_sub_in`.
                        arm_broken_pipe();
                        child.run_source_cached(&body, "process substitution");
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

        // As in `run_proc_sub_in`, after the body starts: a body inheriting
        // the write end of its own input pipe never reaches end of input,
        // so `>(wc -l)` would never answer.
        clear_cloexec(std::os::fd::AsRawFd::as_raw_fd(&write_end));
        let name = format!("/dev/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&write_end));
        self.proc_sub_pipes.push(write_end);
        name
    }

    // Ends the substitutions used by the command that just finished.
    fn drain_proc_subs(&mut self) {
        // Not while a foreground job is waiting to be driven: on the pty
        // path the command hasn't run yet (it's in `pending_fg`), and
        // tearing down now closes the producer's pipe before the reader
        // starts (`wc -l <(seq 1 20000)` counted one pipe buffer). Whoever
        // drives the job calls this after (see `drive_pending_fg_inline`
        // and repl.rs's fg handling).
        if self.pending_fg.is_some() {
            return;
        }
        for path in self.proc_sub_cleanup.drain(..) {
            let _ = std::fs::remove_file(path);
        }
        // Closing this shell's ends tells both kinds the command is over: a
        // `<( )` producer's next write fails, a `>( )` consumer sees end of
        // input.
        self.proc_sub_pipes.clear();
        // Then let them finish: a consumer (`printf x > >(wc -l)`) has only
        // just had its input, and a producer takes a turn or two to stop.
        for _ in 0..PROC_SUB_WINDDOWN_TURNS {
            if !self.has_live_coroutines() {
                break;
            }
            self.pump_coroutines();
        }
        // What's still running is a producer nobody is reading.
        if let Ok(mut scheduler) = self.background_coroutines.try_borrow_mut() {
            scheduler.cancel_running();
            scheduler.retire_finished();
        }
        // Real-process substitutions: a producer nobody reads is stopped; a
        // consumer is left to finish and reaped later. Waiting for it would
        // block on something deliberately backgrounded, which bash doesn't.
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

    /// `FUNCNEST`, if set to something meaningful. Not `lookup_var`, which
    /// ends in a scan of `environ`, on every call; an inherited `FUNCNEST` is
    /// already in `globals`.
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

    /// Whether this call to `name` provably repeats one already on the
    /// stack with nothing in between. `Shell::effects` counts every
    /// command, assignment and volatile read, but not function calls, so a
    /// frame of this function with the same `effects` means nothing
    /// observable ran since, only frames were pushed. With the same
    /// arguments too, the whole state is identical: a fixed point that
    /// repeats forever.
    ///
    /// O(1) in practice: the scan stops at the first frame whose
    /// `effects_at_entry` differs, the first one in any program doing work.
    /// It walks far only along the cycle itself, which also catches mutual
    /// recursion. Returns the cycle's length in frames.
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

    /// `Some` when this call must not happen: `FUNCNEST` forbids it, or
    /// there isn't stack enough to survive it. bash has only the first and
    /// dies on a signal without it; the stack check makes a limit real when
    /// `FUNCNEST` isn't set (see stackguard). The message is bash's, with
    /// the line and the limit; without `FUNCNEST` it reports the depth
    /// reached.
    fn refuse_deeper_nesting(&mut self, name: &str, args_hash: u64) -> Option<ExecResult> {
        // Already unwinding from a refusal below: every call on the way out
        // is refused silently, the message given once, as bash's jump to
        // the top level amounts to.
        if self.nesting_unwind {
            return Some(ExecResult::Status(1));
        }
        // bash reads FUNCNEST as a number, anything else (unset, zero,
        // negative, `abc`) meaning no limit. Checked before the
        // non-termination proof: a script that sets `FUNCNEST` asks for
        // bash's mechanism, message and depth included; every other script
        // gets the proof.
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
            // The backstop under both, so a `FUNCNEST` too large to reach
            // can't lead to a crash. Reports the depth reached, since the
            // stack was the limit.
            _ if crate::stackguard::nearly_exhausted() => {
                format!("bish: line {}: {name}: maximum function nesting level exceeded ({})\n", self.current_line, self.function_depth)
            }
            _ => return None,
        };
        self.sink_err(&message);
        self.nesting_unwind = true;
        // bash stops the script and exits 1. Not in an interactive session,
        // where the unwind flag returns to the prompt, as for readonly
        // assignments.
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
        // Before the body runs, while `current_line` is still the call's
        // line.
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
        // Back at top level: the unwind is over.
        if self.function_depth == 0 {
            self.nesting_unwind = false;
        }
        // RETURN, after the frame is gone, so the trap runs in the caller's
        // scope. It fires for the function that set it and every one it
        // returns into, not for calls made from there (which `functrace`
        // would add):
        //
        //   trap RETURN; f() { :; }; f          set at 0, f returns at 1: no
        //   f() { trap RETURN; :; }; f          set at 1, f returns at 1: yes
        //   f() { trap RETURN; g; }; g          set at 1, g returns at 2: no
        //   g() { trap RETURN; :; }; f() { g; } set at 2, f returns at 1: yes
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
            // A branch always follows a condition, so a foreground pty job
            // can't escape from one; see `settle_fg`.
            let cond_result = self.settle_fg(cond_result);
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
        // Separate from `last_status`, which the condition's own run
        // overwrites, including the final failing check: a loop reports the
        // body's last status, as bash does.
        let mut last_body_status = 0;
        loop {
            self.suppress_errexit += 1;
            let cond_result = self.run_program(cond);
            self.suppress_errexit -= 1;
            // As for `if`: the body or the next test follows.
            let cond_result = self.settle_fg(cond_result);
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
                // The next iteration always follows, so a body's job can't
                // escape; see `settle_fg`.
                ExecResult::Fg => {
                    let status = self.drive_pending_fg_inline();
                    self.last_status = status;
                    last_body_status = status;
                }
                ret @ (ExecResult::Return(_) | ExecResult::Window(_) | ExecResult::Edit | ExecResult::Exit(_)) => return ret,
            }
        }
        if ran_body {
            self.last_status = last_body_status;
            ExecResult::Status(last_body_status)
        } else {
            ExecResult::Status(0)
        }
    }

    /// Names a compound's header and fires the DEBUG trap for it, as written:
    /// `for i in 1`, not the expanded list.
    fn name_and_trace_header(&mut self, header: &str) {
        if self.debug_trap.is_none() || self.in_trap {
            return;
        }
        self.bash_command = Rc::from(header);
        self.run_pseudo_trap(PseudoTrap::Debug);
    }

    fn run_for(&mut self, var: &str, words: Option<&[Word]>, body: &Program) -> ExecResult {
        let values = match words {
            Some(words) => self.expand_words(words),
            None => self.arg_frames.last().cloned().unwrap_or_default(),
        };
        // A `for`, `select` or `case` header is a command to the DEBUG trap
        // (`if` and `while` conditions fire on their own). Once per
        // iteration, and not at all for an empty list, as in bash.
        let header = loop_header("for", var, words);
        let mut ran_body = false;
        for val in values {
            self.name_and_trace_header(&header);
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
                // A loop body has the next iteration behind it, so the job
                // is settled here (`for f in a b c; do cp "$f" d/; done`).
                // See `settle_fg`.
                ExecResult::Fg => {
                    let status = self.drive_pending_fg_inline();
                    self.last_status = status;
                }
                ret @ (ExecResult::Return(_) | ExecResult::Window(_) | ExecResult::Edit | ExecResult::Exit(_)) => return ret,
            }
        }
        if ran_body { ExecResult::Status(self.last_status) } else { ExecResult::Status(0) }
    }

    // `select var [in words]; do body; done`: a numbered menu on stderr, as
    // bash does, and a choice read from stdin until `break` or end of input.
    // As in bash: the menu prints before the first prompt and after any blank
    // line; a blank line reprompts without running the body; an invalid
    // choice runs the body with `var` empty (REPLY keeps the text); end of
    // input ends the loop without the body, status 1.
    fn run_select(&mut self, var: &str, words: Option<&[Word]>, body: &Program) -> ExecResult {
        self.name_and_trace_header(&loop_header("select", var, words));
        let items: Vec<String> = match words {
            Some(words) => self.expand_words(words),
            None => self.arg_frames.last().cloned().unwrap_or_default(),
        };
        let print_menu = |this: &Self, items: &[String]| {
            for (i, item) in items.iter().enumerate() {
                sh_eprintln!(this, "{}) {}", i + 1, item);
            }
        };
        // Nothing to choose from: bash prints no menu, doesn't prompt, and
        // leaves at once with status 0.
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
                // At end of input bash prints a bare newline to stdout (not
                // stderr, where the menu is), even non-interactively.
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
                // The next iteration always follows, so a body's job can't
                // escape; see `settle_fg`.
                ExecResult::Fg => {
                    let status = self.drive_pending_fg_inline();
                    self.last_status = status;
                }
                ret @ (ExecResult::Return(_) | ExecResult::Window(_) | ExecResult::Edit | ExecResult::Exit(_)) => return ret,
            }
        }
    }

    // `for ((init; cond; step))`: `continue` still runs `step` before the
    // next test, so the Continue arm falls through to it.
    fn run_cfor(&mut self, init: &str, cond: &str, step: &str, body: &Program) -> ExecResult {
        // Each section is its own command to the DEBUG trap, named
        // `((expr))`; an omitted one is `((1))`, as bash names it.
        self.name_and_trace_header(&format!("(({}))", if init.is_empty() { "1" } else { init }));
        if !init.is_empty() {
            if let Err(e) = arith::eval(init, self) {
                sh_eprintln!(self, "bish: ((: {}: {}", init, e);
                return ExecResult::Status(1);
            }
        }
        let mut ran_body = false;
        loop {
            self.name_and_trace_header(&format!("(({}))", if cond.is_empty() { "1" } else { cond }));
            let keep_going = if cond.is_empty() {
                true
            } else {
                match arith::eval(cond, self) {
                    Ok(v) => v != 0,
                    Err(e) => {
                        sh_eprintln!(self, "bish: ((: {}: {}", cond, e);
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
                // The next iteration always follows; see `settle_fg`.
                ExecResult::Fg => {
                    let status = self.drive_pending_fg_inline();
                    self.last_status = status;
                }
                ret @ (ExecResult::Return(_) | ExecResult::Window(_) | ExecResult::Edit | ExecResult::Exit(_)) => return ret,
            }
            self.name_and_trace_header(&format!("(({}))", if step.is_empty() { "1" } else { step }));
            if !step.is_empty() {
                if let Err(e) = arith::eval(step, self) {
                    sh_eprintln!(self, "bish: ((: {}: {}", step, e);
                    return ExecResult::Status(1);
                }
            }
        }
        if ran_body { ExecResult::Status(self.last_status) } else { ExecResult::Status(0) }
    }

    fn run_case(&mut self, word: &Word, arms: &[(Vec<Word>, Program, parser::CaseTerm)]) -> ExecResult {
        // With the trailing space, as bash writes `case x in `.
        self.name_and_trace_header(&format!("case {} in ", crate::serialize::serialize_word(word)));
        let val = self.expand_word(word);
        let mut i = 0;
        // Set by `;&`: run the next arm's body without testing its pattern
        // (`;;&` instead resumes testing at the next arm).
        let mut force_run = false;
        let mut last_body_status = 0;
        while i < arms.len() {
            let (patterns, body, term) = &arms[i];
            // `shopt -s nocasematch` covers `case` too.
            let fold_case = self.shopt_is_on("nocasematch");
            let should_run = force_run
                || patterns.iter().any(|p| {
                    // A quoted part of a pattern is literal:
                    // `case abc in "$p")` with `p='*'` looks for an
                    // asterisk. Built chunk by chunk, as `[[ =~ ]]` does.
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
                // `[[ a == b == c ]]`: no form takes four operands, and
                // reading it as "non-empty, so true" let a stray word pass.
                if word_atoms.len() > 3 {
                    let text = word_atoms.iter().map(|w| self.expand_word(w)).collect::<Vec<_>>();
                    return Err(format!("syntax error in conditional expression: unexpected token `{}'", text[3]));
                }
                let text = word_atoms.iter().map(|w| self.expand_word(w)).collect::<Vec<_>>();
                if let Some(e) = Self::conditional_shape_error(&text) {
                    return Err(e);
                }
                Ok(self.eval_simple_test(&word_atoms))
            }
            // Nothing where an expression should be: `[[ ]]`, or a `]]`
            // before its expression.
            None => Err("syntax error near `]]'".to_string()),
            // `&&`, `||` or `!` without a left side (`[[ && a ]]`), named,
            // not `Debug`-printed.
            Some(atom) => Err(format!("unexpected token `{}' in conditional command", Self::test_atom_name(atom))),
        }
    }

    // What `[ -v NAME ]` and `[ -o OPTNAME ]` need answered, for the operands
    // that appear; see `builtins::ShellFacts`.
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

    /// `${!name}`: the variable named by `name`'s value. bash makes it
    /// fatal, like `${x:?}`, when `name` is unset, empty or not a parameter
    /// name, so a typo doesn't silently become an empty string. The value
    /// may name an element (`x=a[1]`), a positional (`x=1`) or a special
    /// (`x=@`); bash follows them all.
    fn indirect_var(&mut self, name: &str) -> String {
        // For a nameref, `${!x}` gives the name it points at, following the
        // chain, rather than indirecting through its value.
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

    // `[[ -v NAME ]]`, `[[ -v arr[0] ]]` included: whether the name, array
    // or element is set, not just empty.
    fn name_is_set(&mut self, name: &str) -> bool {
        match name.split_once('[').and_then(|(base, rest)| rest.strip_suffix(']').map(|index| (base, index))) {
            Some((base, index)) => self.array_element_is_set(base, index),
            None => self.var_is_set(name) || !self.array_all(name).is_empty(),
        }
    }

    /// What a `[[ ]]` says when its words aren't an expression, in bash's
    /// three shapes, read off `bash -c`. `[[ -a -z ]]` isn't an error (does a
    /// file `-z` exist?), so an operand that looks like an operator can't be
    /// the test.
    fn conditional_shape_error(text: &[String]) -> Option<String> {
        match text {
            // `[[ -z ]]`: a unary operator without its operand.
            [op] if crate::builtins::is_unary_op(op) => Some("unexpected argument `]]' to conditional unary operator".to_string()),
            // `[[ a -eq ]]`: a binary operator without its right side,
            // checked on the second word, which tells it from the case
            // below.
            [_, op] if crate::builtins::is_binary_op(op) => Some("unexpected argument `]]' to conditional binary operator".to_string()),
            // `[[ a b ]]`, `[[ = a ]]`, `[[ -eq a ]]`: two words with
            // nothing to join them. bash names the second, which came where
            // an operator was due.
            [first, second] if !crate::builtins::is_unary_op(first) => {
                Some(format!("unexpected token `{second}', conditional binary operator expected"))
            }
            _ => None,
        }
    }

    /// A name for a non-word atom in an error message, rather than the
    /// enum's `{:?}`.
    fn test_atom_name(atom: &parser::TestAtom) -> &'static str {
        match atom {
            parser::TestAtom::And => "&&",
            parser::TestAtom::Or => "||",
            parser::TestAtom::Not => "!",
            parser::TestAtom::Group(_) => "(",
            parser::TestAtom::Word(_) => "word",
        }
    }

    fn eval_simple_test(&mut self, words: &[&Word]) -> bool {
        match words {
            [] => false,
            [s] => !self.expand_word(s).is_empty(),
            [op, a] => {
                let op = self.expand_word(op);
                let a = self.expand_word(a);
                // `-v` asks about a name, not the text, so it can't go
                // through `builtins::unary`, which has no shell to ask.
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
                    // `shopt -s nocasematch` applies to `=~` too.
                    match crate::regex::match_captures(&a, &pattern, self.shopt_is_on("nocasematch")) {
                        Some(groups) => {
                            let map: std::collections::BTreeMap<usize, String> = crate::sort::btree_map(groups.into_iter().enumerate());
                            self.arrays.insert("BASH_REMATCH".to_string(), map);
                            true
                        }
                        None => {
                            self.arrays.remove("BASH_REMATCH");
                            false
                        }
                    }
                } else {
                    // `==`, `!=` and `=` take a pattern on the right, so
                    // its quoted parts are escaped; other operators compare
                    // plain text.
                    let b = match op.as_str() {
                        "=" | "==" | "!=" => self.expand_glob_pattern_operand(b),
                        _ => self.expand_word(b),
                    };
                    let fold_case = self.shopt_is_on("nocasematch");
                    builtins::binary_with_case(&a, &op, &b, true, fold_case)
                }
            }
            // Four or more words are rejected by the caller first.
            _ => !words.is_empty(),
        }
    }

    // The right side of `[[ str =~ pattern ]]`. A quoted or backslash-escaped
    // part matches literally, as bash documents: each quoted chunk's text is
    // escaped for the regex engine and unquoted chunks stay regex syntax, so
    // in `^[0-9]+\.[0-9]+$` only the escaped `.` is literal.
    fn expand_regex_operand(&mut self, w: &Word) -> String {
        let mut out = String::new();
        for c in &w.chunks {
            match c {
                Chunk::Tilde { name } => {
                    let name = name.clone();
                    out.push_str(&crate::regex::escape(&self.expand_tilde(&name)));
                }
                Chunk::Str(t) => out.push_str(t),
                Chunk::LiteralStr(t, _) => out.push_str(&crate::regex::escape(t)),
                Chunk::Var { name, quoted, .. } => {
                    let name = name.clone();
                    self.check_nounset(&name);
                    let v = self.lookup_var(&name);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::Sub { raw, quoted, .. } => {
                    let v = self.run_command_substitution(raw);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::Arith { raw, quoted } => match self.eval_arith(raw) {
                    Ok(v) => {
                        let v = v.to_string();
                        out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                    }
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}: {}", raw, e);
                        // Fatal, unlike the `(( ))` command; see
                        // `expansion_failed`.
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

    // The same for `[[ x == PATTERN ]]`: a quoted part is literal, so
    // `[[ abc == "a*" ]]` is false and `[[ abc == a* ]]` true.
    fn expand_glob_pattern_operand(&mut self, w: &Word) -> String {
        let mut out = String::new();
        for c in &w.chunks {
            match c {
                Chunk::Tilde { name } => {
                    let name = name.clone();
                    out.push_str(&crate::glob::escape(&self.expand_tilde(&name)));
                }
                Chunk::Str(t) => out.push_str(t),
                Chunk::LiteralStr(t, _) => out.push_str(&crate::glob::escape(t)),
                Chunk::Var { name, quoted, .. } => {
                    let name = name.clone();
                    self.check_nounset(&name);
                    let v = self.lookup_var(&name);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v.into() });
                }
                Chunk::Sub { raw, quoted, .. } => {
                    let v = self.run_command_substitution(raw);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v.into() });
                }
                Chunk::Arith { raw, quoted } => match self.eval_arith(raw) {
                    Ok(v) => {
                        let v = v.to_string();
                        out.push_str(&if *quoted { crate::glob::escape(&v) } else { v.into() });
                    }
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}: {}", raw, e);
                        // Fatal, unlike the `(( ))` command; see
                        // `expansion_failed`.
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
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v.into() });
                }
                Chunk::ArrayVar { name, index, quoted } => {
                    let name = name.clone();
                    let index = index.clone();
                    let v = self.array_element(&name, &index);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v.into() });
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
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v.into() });
                }
                Chunk::Indirect { name, quoted } => {
                    let v = self.indirect_var(name);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v.into() });
                }
                Chunk::ArrayKeys { name, quoted } => {
                    let name = name.clone();
                    let sep = self.ifs_join_char();
                    let v = self.array_keys(&name).join(&sep);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v.into() });
                }
                Chunk::VarNamesMatchingPrefix { prefix, quoted, .. } => {
                    let prefix = prefix.clone();
                    let sep = self.ifs_join_char();
                    let v = self.var_names_with_prefix(&prefix).join(&sep);
                    out.push_str(&if *quoted { crate::glob::escape(&v) } else { v.into() });
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
            // An assignment-only command is named in `$BASH_COMMAND` like
            // any other. Serialized, not expanded: DEBUG fires before the
            // assignment, and expanding here would run any `$( )` in it
            // twice.
            let named: Vec<String> = cmd
                .assigns
                .iter()
                .map(|(name, mode, val)| {
                    let op = if *mode == AssignMode::Append { "+=" } else { "=" };
                    format!("{}{}{}", name, op, crate::serialize::serialize_word(val))
                })
                .collect();
            self.bash_command = Rc::from(named.join(" ").into_boxed_str());
            // With no last word, `$_` becomes empty, as in bash:
            // `x=1; echo "$_"` prints nothing.
            self.underscore_pending.push(String::new());
            // DEBUG before the command: its use is seeing what's about to
            // run.
            self.run_pseudo_trap(PseudoTrap::Debug);
            // `&` makes an assignment-only command a job too, whose effect
            // is lost with the child, as bash does with `x=1 &`.
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
            // A refused write (readonly) fails the command, for `set -e` and
            // `|| exit` to see.
            let mut ok = true;
            // Cleared before the expansions, so what's left is this
            // command's; see `last_subst_status`.
            self.last_subst_status = None;
            // Traced, as bash does: `set -x; x=1` prints `+ x=1` with the
            // expanded value, quoted only where needed.
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
                // `a[]=1` names no element: a bad subscript, as in bash,
                // not index 0.
                if index.trim().is_empty() {
                    sh_eprintln!(self, "bish: {}[]: bad array subscript", name);
                    ok = false;
                    continue;
                }
                // `array_set_index` prints the refusal itself.
                ok &= !self.name_is_readonly(name);
                let v = self.expand_word(val);
                if self.opt_xtrace {
                    let op = if *mode == AssignMode::Append { "+=" } else { "=" };
                    traced.push(format!("{}[{}]{}{}", name, index, op, xtrace_quote(&v)));
                }
                let v = match mode {
                    AssignMode::Set => v,
                    // `m[k]+=v` appends to the element, as `x+=v` does.
                    AssignMode::Append => self.array_element(name, index) + &v,
                };
                self.array_set_index(name, index, v);
            }
            if !traced.is_empty() {
                let ps4 = self.xtrace_prefix();
                sh_eprintln!(self, "{}{}", ps4, traced.join(" "));
            }
            if !cmd.redirects.is_empty() {
                // For the side effect only: create, truncate or append the
                // targets.
                let _ = self.resolve_redirects(cmd);
            }
            // A refused assignment is fatal in a script, as in bash, which
            // is what makes `readonly` worth writing; not in an interactive
            // session.
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

        // Named before expansion, as written: `echo "$v" a*`, not
        // `echo q a*`, and a command reading `$BASH_COMMAND` in its own
        // words sees itself. Not while a trap runs: `$BASH_COMMAND` names
        // the command the trap is about.
        if !self.in_trap {
            self.bash_command = crate::serialize::rendered_command(cmd);
        }
        // DEBUG fires here, before the expansions, so a `$( )` in the command
        // runs after the trap, as in bash.
        self.run_pseudo_trap(PseudoTrap::Debug);

        let saved_stderr_target = self.current_stderr_target.take();
        self.current_stderr_target = self.peek_stderr_target(&cmd.redirects);
        let first_word_literal = match cmd.words[0].chunks.as_slice() {
            // A quoted builtin name is still recognized (`"export" FOO=bar`
            // runs export), so a fully quoted name matches too.
            [Chunk::Str(s)] | [Chunk::LiteralStr(s, _)] => Some(s.as_str()),
            _ => None,
        };
        // Only for the assignment builtins (see parser.rs's
        // `is_declare_family_command`). Position `p` is the array literal
        // sitting at `argv[p]`, which `run_declare`/`run_local` splice back
        // in rather than reparsing a string.
        let mut array_literal_args: Vec<(usize, String, AssignMode, Vec<ArrayLiteralItem>)> = Vec::new();
        let argv: Vec<String> = if matches!(first_word_literal, Some("local") | Some("export") | Some("declare") | Some("typeset") | Some("readonly"))
        {
            // For the assignment builtins, a `NAME=value` argument isn't
            // word-split, like any assignment.
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
        // Expanded once, here, for every consumer, so
        // `x=$(echo a) /bin/echo hi` runs the substitution once. After the
        // words, as bash orders them: `x=$(echo A >&2) cmd $(echo B >&2)`
        // prints B, then A.
        let prefix_values: Vec<(String, String)> = cmd
            .assigns
            .iter()
            .map(|(name, mode, val)| {
                let v = self.expand_word(val);
                let v = match mode {
                    AssignMode::Set => v,
                    AssignMode::Append => self.appended_value(name, &v),
                };
                (name.clone(), v)
            })
            .collect();
        // What `$_` becomes after this command: its last expanded word, or
        // its name with no arguments. Recorded, not assigned: the command
        // must still see the previous value.
        self.underscore_pending.push(argv.last().cloned().unwrap_or_default());
        if argv.is_empty() {
            // Every word vanished (an unquoted empty variable): nothing
            // runs, as in bash.
            return ExecResult::Status(0);
        }
        if self.opt_xtrace {
            let ps4 = self.xtrace_prefix();
            // Assignments first, where bash puts them, always as `=` with
            // the value the command sees, so a prefix `p+=y` traces as
            // `p=xy`.
            for (name, value) in &prefix_values {
                sh_eprintln!(self, "{}{}={}", ps4, name, xtrace_quote(value));
            }
            // Each word quoted if needed, so the trace reads as the command
            // that ran: bash writes `+ echo 'a b'` and `+ '[' x = x ']'`.
            let words: Vec<String> = argv.iter().map(|w| xtrace_quote_word(w)).collect();
            sh_eprintln!(self, "{}{}", ps4, words.join(" "));
        }
        let name = argv[0].clone();

        // `&` on a builtin or function makes a job, a child, so `echo a &`
        // sets `$!` and `f() { exit 5; }; f &` can't end the shell. The
        // child gets the already-expanded words, quoted, so a `$( )`
        // doesn't run twice; its redirects go as written, not yet expanded.
        // Not a `declare`-family array literal, whose display text isn't
        // its source; backgrounding a declaration does nothing observable
        // anyway.
        let runs_in_this_shell = self.is_active_builtin(&name) || (!self.restrict_to_builtins && self.functions.contains_key(&name));
        if background && runs_in_this_shell && cmd.array_assigns.is_empty() && cmd.array_word_assigns.is_empty() {
            let mut script = String::new();
            for (n, v) in &prefix_values {
                script.push_str(&format!("{}={} ", n, crate::serialize::quote_literal(v)));
            }
            script.push_str(&argv.iter().map(|w| crate::serialize::quote_literal(w)).collect::<Vec<_>>().join(" "));
            for r in &cmd.redirects {
                script.push(' ');
                script.push_str(&crate::serialize::serialize_redirect(r));
            }
            let label = script.clone();
            return ExecResult::Status(self.spawn_background_script(&script, label));
        }

        // Functions shadow builtins, as in bash, even `export`, `return` and
        // `break`; `builtin NAME` bypasses that. Not on the colon line
        // (`restrict_to_builtins`), which runs only real builtins.
        if !self.restrict_to_builtins
            && let Some(body) = self.functions.get(&name).cloned()
        {
            // In scope for the body: `p=1 f` lets `f` read `p`, and the old
            // value comes back after, as in bash.
            let restore = self.apply_prefix_assigns(&prefix_values);
            // A redirect on the call applies to the whole body: `f > out`
            // sends everything the function prints, builtins and externals
            // alike.
            if cmd.redirects.is_empty() {
                let result = self.call_function(&name, &body, argv[1..].to_vec());
                self.restore_prefix_assigns(restore);
                return result;
            }
            if !Self::compound_redirects_are_simple(&cmd.redirects) {
                // A numbered-fd redirect on a call has no in-process model
                // (see `run_compound_redirected`), so the body runs as a
                // redirected compound.
                let group = parser::Command::Group(
                    vec![ListItem {
                        and_or: AndOr { first: Pipeline { commands: vec![body.clone()], negate: false, timed: None }, rest: Vec::new() },
                        sep: Sep::Seq,
                        line: self.current_line,
                        end_line: self.current_line,
                    }],
                    Vec::new(),
                );
                self.arg_frames.push(argv[1..].to_vec());
                let result = self.run_compound_redirected(&group, &cmd.redirects, background);
                self.arg_frames.pop();
                self.restore_prefix_assigns(restore);
                return result;
            }
            let stdio = match self.resolve_simple_redirects_for_compound(&cmd.redirects) {
                Ok(stdio) => stdio,
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    self.restore_prefix_assigns(restore);
                    return ExecResult::Status(1);
                }
            };
            let saved = self.install_redirected_stdio(stdio);
            let result = self.call_function(&name, &body, argv[1..].to_vec());
            self.restore_redirected_stdio(saved);
            self.restore_prefix_assigns(restore);
            return result;
        }
        // A prefix assignment (`IFS=: read a b`) is in scope for the
        // command and gone after. An external gets it as environment (see
        // `run_external`), but a builtin reads the shell's variables, so
        // it's applied around the dispatch here, where it's known not to be
        // a function call (bash lets an assignment outlive a function call,
        // a rule of its own). Every builtin and external is an effect;
        // function calls returned above.
        self.effects += 1;
        let restore = self.apply_prefix_assigns(&prefix_values);
        let result = self.dispatch_builtin_or_external(&argv, name, cmd, background, false, &array_literal_args);
        self.restore_prefix_assigns(restore);
        // Every simple command returns through here, so a `>( )` used by a
        // builtin runs too, not only those on the external paths. A no-op
        // when nothing is queued.
        self.drain_proc_subs();
        result
    }

    // Applies prefix assignments, returning the old values to restore. `None`
    // for a name that was unset, restored as unset, not empty, which
    // `${FOO-default}` and `-u` would notice.
    fn apply_prefix_assigns(&mut self, values: &[(String, String)]) -> Vec<(String, Option<String>)> {
        let mut saved = Vec::new();
        for (name, v) in values {
            let v = v.clone();
            // Only what was written needs restoring: a refused write left
            // nothing, and restoring would print the refusal twice.
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
                // Restored as unset, as `unset` does it.
                None => self.remove_var(&name),
            }
        }
    }

    // `dispatch_builtin_or_external_impl` with a per-command
    // output-redirect sink pushed around it (see
    // `push_builtin_output_sink`). One push and pop here covers every
    // return path: builtins return from the match, and the external tail's
    // child writes to real fds regardless, so only the shell's own
    // diagnostics there are affected, as intended.
    fn dispatch_builtin_or_external(
        &mut self,
        argv: &[String],
        name: String,
        cmd: &SimpleCommand,
        background: bool,
        builtin_only: bool,
        array_literal_args: &[(usize, String, AssignMode, Vec<ArrayLiteralItem>)],
    ) -> ExecResult {
        // Only for a command that runs in this shell. An external's
        // redirects are applied to the process by `resolve_redirects`
        // later, and expanding them here too would expand each target
        // twice, possibly naming two different files.
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
        // A builtin's own redirects are a scope `run_exit_trap` waits on:
        // `exit > f`, `eval exit > f` and `. file > f` restore the
        // descriptor before bash says goodbye. Popped only if this call
        // pushed: the sink may already be a `Builtin` installed for a whole
        // converted construct (see `run_in_child_shell`), which mustn't be
        // stripped.
        if pushed {
            self.redirect_scopes += 1;
        }
        let result = self.dispatch_builtin_or_external_impl(argv, name, cmd, background, builtin_only, array_literal_args);
        if pushed {
            self.pop_builtin_output_sink();
            self.leave_redirect_scope(&result);
        }
        // With the command's redirect sink gone, a bare `exec` can say
        // where this shell's output goes from now on.
        if let Some(moved) = self.descriptors_to_follow.take() {
            self.follow_moved_descriptors(&moved);
        }
        result
    }

    // Split from `run_single` so `builtin NAME...` can reach the builtin
    // dispatch past `run_single`'s function shadowing. `builtin_only` is set
    // for that call: no match means "not a shell builtin" instead of falling
    // through to an external. `array_literal_args` is empty for it, so
    // `builtin declare -A m=(...)` isn't supported.
    fn dispatch_builtin_or_external_impl(
        &mut self,
        argv: &[String],
        name: String,
        cmd: &SimpleCommand,
        background: bool,
        builtin_only: bool,
        array_literal_args: &[(usize, String, AssignMode, Vec<ArrayLiteralItem>)],
    ) -> ExecResult {
        // `enable -n NAME` hands the dispatch a name no arm matches, so the
        // external of that name runs: the external path follows the match. No
        // exception for `builtin NAME`: bash refuses a disabled builtin there
        // too, with "not a shell builtin", which falling through gives.
        let dispatch_name: &str = if self.disabled_builtins.contains(&name) { "\u{0}disabled" } else { name.as_str() };
        match dispatch_name {
            // `:` does nothing and exits 0. Its arguments are already
            // expanded (`: ${x:=default}`), and its redirects still create
            // their targets (`: > file`) through the builtin sink.
            ":" => return ExecResult::Status(0),
            // `command [-v|-V] name [args...]`. `-v`/`-V` report what
            // `name` resolves to without running it
            // (`command -v git >/dev/null`). Bare, it runs `name` as an
            // external, past functions and, unlike bash, builtins too.
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
                // `-p` searches a fixed standard path (`getconf PATH`'s
                // answer) instead of the caller's PATH, for the lookup and
                // the command's environment, since `Command` uses one PATH
                // for both.
                if mode_p {
                    ext.env("PATH", "/usr/bin:/bin");
                }
                ext.args(&argv[i + 1..]);
                ext.current_dir(&self.cwd);
                // The construct's stdio, so `command foo` inside a
                // converted subshell or substitution is captured (mise's
                // activation shadows itself with a function calling
                // `command "$__MISE_EXE"`, inside `$( )`).
                ext.stdin(self.spawn_stdin_stdio());
                ext.stdout(self.spawn_stdout_stdio());
                // See `apply_fd_redirects`' pre_exec: otherwise
                // `command foo` inherits bish's ignored SIGINT and ignores
                // Ctrl-C.
                #[cfg(unix)]
                unsafe {
                    ext.pre_exec(|| {
                        crate::platform::default_signal(crate::platform::SIGINT);
                        Ok(())
                    });
                }
                self.note_external_spawn();
                return match ext.status() {
                    Ok(status) => ExecResult::Status(exit_code_from_status(status)),
                    Err(e) => {
                        // The same answers and statuses as an ordinary
                        // command word; see `spawn_external`'s Err arm.
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
            // `builtin NAME [args...]`: the real builtin even when a
            // function shadows it, never an external ("not a shell builtin"
            // instead, as in bash). Bare `builtin` does nothing, as in
            // bash.
            "builtin" => {
                if argv.len() < 2 {
                    return ExecResult::Status(0);
                }
                let inner_name = argv[1].clone();
                return self.dispatch_builtin_or_external(&argv[1..], inner_name, cmd, background, true, &[]);
            }
            "type" => return ExecResult::Status(crate::builtins::shell::run_type(self, &argv[1..])),
            // There's no command path cache (PATH is resolved at every
            // spawn), so `hash -r` and `hash cmd` just succeed, and bare
            // `hash` prints the empty cache as bash would.
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
            // Builtins, as in bash, so `while true` doesn't spawn per
            // iteration.
            "true" => return ExecResult::Status(0),
            "false" => return ExecResult::Status(1),
            // A shell without job control can't hand itself back, as bash
            // says.
            "suspend" => {
                if !self.opt_monitor {
                    sh_eprintln!(self, "bish: suspend: cannot suspend: no job control");
                    return ExecResult::Status(1);
                }
                send_signal(crate::platform::process_id(), 19);
                return ExecResult::Status(0);
            }
            "cd" => return ExecResult::Status(crate::builtins::dirs::run_cd(self, &argv[1..])),
            // A builtin, as in bash: it reports the shell's own idea of
            // where it is, works with an empty PATH, and `type pwd` says
            // so.
            "pwd" => {
                // `-P` is the resolved path and `-L` the shell's; the same
                // here, since `cd` resolves symlinks as it goes (a
                // divergence recorded in bashdiff).
                let physical = argv[1..].iter().any(|a| a == "-P");
                let here = match physical {
                    true => std::fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.cwd.clone()),
                    false => self.cwd.clone(),
                };
                sh_println!(self, "{}", here.display());
                return ExecResult::Status(0);
            }
            // `e [ARG...]` bubbles up as `ExecResult::Edit` (see it and
            // `Fg`), its arguments unparsed: what they mean is the editor's
            // to decide.
            "e" => {
                self.pending_edit = Some(argv[1..].to_vec());
                return ExecResult::Edit;
            }
            // Builtins (see `run_echo`, `run_printf`), which also matters
            // on the colon line, where externals aren't allowed.
            "echo" => return ExecResult::Status(crate::builtins::io::run_echo(self, &argv[1..])),
            "printf" => return ExecResult::Status(crate::builtins::io::run_printf(self, &argv[1..])),
            "umask" => return ExecResult::Status(crate::builtins::limits::run_umask(self, &argv[1..])),
            "times" => return ExecResult::Status(crate::builtins::limits::run_times(self, &argv[1..])),
            "enable" => return ExecResult::Status(crate::builtins::shell::run_enable(self, &argv[1..])),
            "help" => return ExecResult::Status(crate::builtins::shell::run_help(self, &argv[1..])),
            "caller" => return ExecResult::Status(crate::builtins::shell::run_caller(self, &argv[1..])),
            "ulimit" => return ExecResult::Status(crate::builtins::limits::run_ulimit(self, &argv[1..])),
            // `alias`/`unalias`: the table; expansion happens when lines
            // are read (see `expand_aliases`).
            "alias" => {
                // `-p` prints them all, like no arguments: bash's "give me
                // something re-runnable".
                if argv.len() == 1 || argv[1..].iter().all(|a| a == "-p") {
                    let mut sorted: Vec<&(String, String)> = self.aliases.iter().collect();
                    crate::sort::sort_by(&mut sorted, |a, b| a.0.cmp(&b.0));
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
                            let table = self.aliases_mut();
                            match table.iter_mut().find(|(n, _)| *n == name) {
                                Some(existing) => existing.1 = val,
                                None => table.push((name, val)),
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
                    self.aliases_mut().clear();
                    return ExecResult::Status(0);
                }
                let mut status = 0;
                for a in &argv[1..] {
                    match self.aliases.iter().position(|(n, _)| n == a) {
                        Some(pos) => {
                            self.aliases_mut().remove(pos);
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
            // `= EXPR`: evaluate and print, the Enter half of the inline
            // calculator whose answer shows as ghost text while typing
            // (`ArithSuggestionProvider`), so it can be piped or captured.
            "=" => return ExecResult::Status(self.run_arith_print(&argv[1..])),
            "shopt" => return ExecResult::Status(crate::builtins::shell::run_shopt(self, &argv[1..])),
            "bishopt" => return ExecResult::Status(crate::builtins::bish::run_bishopt(self, &argv[1..], KNOWN_BISHOPTS)),
            // `::bish SUBCOMMAND...`: bish's own namespace (see `run_bish`).
            "::bish" => return crate::builtins::bish::run_bish(self, &argv[1..]),
            "compgen" => return ExecResult::Status(crate::builtins::completion::run_compgen(self, &argv[1..])),
            "complete" => return ExecResult::Status(crate::builtins::completion::run_complete(self, &argv[1..])),
            "compopt" => return ExecResult::Status(crate::builtins::completion::run_compopt(self, &argv[1..])),
            // On the colon line only, where builtins run exclusively, so
            // there's no external for the short name to shadow; elsewhere
            // it's `::bish window`. `w` isn't an alias: it's kept for a
            // vim-style `:w`.
            "window" | "win" if self.restrict_to_builtins => {
                return crate::builtins::bish::run_window(self, &argv[1..]);
            }
            "pushd" => return ExecResult::Status(crate::builtins::dirs::run_pushd(self, &argv[1..])),
            "popd" => return ExecResult::Status(crate::builtins::dirs::run_popd(self, &argv[1..])),
            "dirs" => return ExecResult::Status(crate::builtins::dirs::run_dirs(self, &argv[1..])),
            // `export` is `declare -x`, as bash documents, sharing `-x`'s
            // handling of locals.
            "export" => {
                // `export -f NAME` marks a function for export. bash
                // carries it through the environment; bish's children
                // inherit the function table already (see
                // `new_virtual_child`), so it only has to be quiet and
                // reject a name that isn't a function, not fall through to
                // `declare -f`, which prints it.
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
                // `export -n NAME` removes the export attribute, keeping
                // the value. Not through `run_declare`, where `-n` means
                // nameref.
                if argv.get(1).map(String::as_str) == Some("-n") {
                    for name in &argv[2..] {
                        Rc::make_mut(&mut self.exported_names).remove(name);
                    }
                    return ExecResult::Status(0);
                }
                let mut declare_args = vec!["-x".to_string()];
                declare_args.extend(argv[1..].iter().cloned());
                // Dropping `export` and prepending `-x` cancel out, so
                // `array_literal_args` (indexed into the original argv)
                // still lines up with `declare_args`.
                return ExecResult::Status(crate::builtins::vars::run_declare(self, &name, &declare_args, array_literal_args));
            }
            "let" => {
                let mut last = 0i64;
                for a in &argv[1..] {
                    // `arith::eval`, not `eval_arith`: the argument is
                    // already expanded, and `eval_arith`'s own pass (which
                    // `$(( ))` needs, getting its interior raw) would be a
                    // second one, running `$(touch M)` hidden in a
                    // variable's value. bash doesn't expand `let`'s text at
                    // all.
                    match arith::eval(a, self) {
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
            // `[[` is a keyword (`Command::Test`) and never reaches this
            // dispatch; only `[`, `test`'s other name, does.
            "[" => {
                // The closing `]` is punctuation, dropped by not looking at
                // it.
                let args = match argv[1..].split_last() {
                    Some((last, rest)) if last == "]" => rest,
                    _ => {
                        sh_eprintln!(self, "bish: [: missing closing ]");
                        return ExecResult::Status(2);
                    }
                };
                let (vars, opts) = self.shell_test_answers(args);
                let outcome = builtins::test(args, false, &builtins::ShellFacts { var_is_set: &vars, option_on: &opts });
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
                // A sourced file is allowed too, and bash's message names
                // both places. Status 2, the usage error, not the argument.
                if self.var_scopes.is_empty() && self.source_depth == 0 {
                    sh_eprintln!(self, "bish: return: can only `return' from a function or sourced script");
                    return ExecResult::Status(2);
                }
                return ExecResult::Return(exit_status_byte(code));
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
                // Shifting past the end reports failure:
                // `while shift; do ...; done` would never stop otherwise.
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
                // `-a`/`-A` make an array. `arrays`/`assoc_arrays` stay
                // flat; the previous value is saved on
                // `array_local_stack`/`assoc_local_stack` and restored on
                // return.
                let mut array_mode: Option<bool> = None;
                let mut integer_flag = false;
                let mut nameref_flag = false;
                // `-u`/`-l`/`-x`, like `-i`, aren't undone on return: a
                // leftover case-fold or export attribute is cosmetic, not
                // wrong data.
                let mut upper_flag = false;
                let mut lower_flag = false;
                let mut export_flag = false;
                // `-g` writes the global, past any local, as in bash
                // (`f() { local -g x=5; }; f; echo "$x"` prints 5). Scalars
                // only.
                let mut global_flag = false;
                // array_literal_args is indexed into the *original* argv
                // (which still has argv[0] == "local"), so every position
                // shifts back by one to line up with argv[1..]'s own
                // enumeration below.
                let shifted_array_literals: Vec<_> =
                    array_literal_args.iter().filter_map(|(p, n, m, i)| p.checked_sub(1).map(|p2| (p2, n.clone(), *m, i.clone()))).collect();
                // `local`'s flags cluster, as `declare`'s do
                // (`local -ir v=1`), each a single letter with no argument.
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
                    // `local -A m=([a]=1 [b]=2)`: an array literal at this
                    // position (`a` is its xtrace text). Snapshotted and
                    // reset as for a plain array name, then filled by
                    // `apply_array_literal`.
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
                            (export_flag, Rc::make_mut(&mut self.exported_names)),
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
                        Rc::make_mut(&mut self.exported_names).insert(n.clone());
                    }
                    match array_mode {
                        // `local -a x` declares without assigning: the
                        // attribute is local, and there's no value until
                        // one is written.
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
                            // `local x` without `=` declares the name and
                            // leaves it unset (see `var_scopes`), so
                            // `${x-default}` can tell the caller didn't set
                            // it.
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
                                Rc::make_mut(&mut self.exported_names).insert(n.clone());
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
            // `logout` is `exit` for a login shell and refused elsewhere,
            // including a `( )` or pipeline stage of one, as in bash.
            "exit" | "logout" => {
                let name = argv[0].as_str();
                if name == "logout" && !self.login_shell {
                    sh_eprintln!(self, "bish: logout: not login shell: use `exit'");
                    return ExecResult::Status(1);
                }
                let operands = match argv.get(1).map(String::as_str) {
                    Some("--") => &argv[2..],
                    _ => &argv[1..],
                };
                let code = match operands {
                    [] => self.last_status,
                    [a] => match a.parse::<i32>() {
                        Ok(n) => n,
                        // Reported and not obeyed: bash 5.3 carries on with
                        // 2. Posix mode still leaves.
                        Err(_) => {
                            sh_eprintln!(self, "bish: {}: {}: numeric argument required", name, a);
                            if !self.opt_posix {
                                return ExecResult::Status(2);
                            }
                            self.last_status = 2;
                            self.run_exit_trap();
                            return ExecResult::Exit(2);
                        }
                    },
                    // An error that still leaves, with 1.
                    _ => {
                        sh_eprintln!(self, "bish: {}: too many arguments", name);
                        1
                    }
                };
                // The trap sees the exit status as `$?`:
                // `trap 'echo $?' EXIT; exit 4` prints 4.
                let code = exit_status_byte(code);
                self.last_status = code;
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
                // `-s`: no echo for this read (`term::NoEchoGuard`), only
                // against an interactive terminal, gated like `-p`'s prompt
                // below, and not when reading a coproc fd (`-u`).
                let mut silent = false;
                // Without `-r`, a backslash escapes the next character, the
                // delimiter included, which continues the line.
                let mut raw = false;
                let mut timeout_arg: Option<&str> = None;
                // Short options cluster: `-ra arr` is `-r -a arr`, `-rn2`
                // is `-r -n 2`.
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
                        // An option taking a value takes the rest of the
                        // cluster, or the next argument.
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
                                // `-d ''` delimits on NUL, as
                                // `find -print0` and `git ls-files -z`
                                // produce; the empty string has no first
                                // byte to use.
                                'd' => delim = value.map(|v| v.bytes().next().unwrap_or(0)).unwrap_or(b'\n'),
                                // Enforced below, only against real stdin
                                // (see `is_real_stdin`).
                                't' => timeout_arg = value,
                                'u' => read_u_flag = value,
                                // `-i text` seeds a line editor this read
                                // doesn't have: ignored, as bash does
                                // without a tty.
                                _ => {}
                            }
                            break;
                        }
                        match c {
                            'r' => raw = true,
                            's' => silent = true,
                            // `-e`/`-E`: the line editor; accepted, with
                            // nothing to edit with.
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
                let is_real_stdin = !cmd.redirects.iter().any(|r| matches!(r, Redirect::In(_) | Redirect::HereString(_) | Redirect::HereDoc(_, _)));
                if let Some(p) = prompt {
                    if is_real_stdin && stdin_is_tty() {
                        sh_eprint!(self, "{}", p);
                        let _ = std::io::Write::flush(&mut std::io::stderr());
                    }
                }
                if let Some(secs) = timeout_secs {
                    // `-t 0` asks whether the source has something now,
                    // reading and assigning nothing: a "would this block?"
                    // test.
                    if secs == 0.0 {
                        // A file, heredoc or here-string is always ready.
                        let ready = !is_real_stdin || stdin_ready(0);
                        return ExecResult::Status(i32::from(!ready));
                    }
                    if is_real_stdin {
                        let ms = (secs * 1000.0).max(0.0) as i32;
                        if !stdin_ready(ms) {
                            return ExecResult::Status(1);
                        }
                    }
                    // A redirected source has no fd to poll, so `-t` treats
                    // it as ready.
                }

                // `-u FD` reads a descriptor this shell holds (usually a
                // coproc's read end) instead of stdin, in its own block so
                // the `coproc_fds` borrow ends before the assignments
                // below.
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
                        // Not a coproc's end: a descriptor this shell
                        // holds, such as from `exec 3<file`. Unbuffered, as
                        // stdin is (see UnbufferedFd).
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
                        // Without `-r` the escapes come off, and what they
                        // protected is remembered, so an escaped separator
                        // stays in its field.
                        let (line, mask) = if raw { (line, Vec::new()) } else { unescape_read_line(&line) };
                        let chars: Vec<char> = line.chars().collect();
                        let mask = if raw { vec![false; chars.len()] } else { mask };
                        if let Some(arr) = array_name {
                            let parts = ifs_tokenize_masked(&chars, &mask, &ifs);
                            let map: std::collections::BTreeMap<usize, String> = crate::sort::btree_map(parts.into_iter().enumerate());
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
            // `mapfile`/`readarray [options] [name]`: reads lines from
            // stdin (or the command's `<`, through `read_input_source`)
            // into an indexed array, MAPFILE by default.
            "mapfile" | "readarray" => {
                let mut strip_newline = false;
                let mut array_name = "MAPFILE".to_string();
                // `-n` lines to keep, `-s` to skip first, `-O` the start
                // index, `-u` a descriptor instead of stdin. `-c`/`-C`, the
                // progress callback, are accepted and ignored.
                let mut max_lines = 0u64;
                let mut skip = 0u64;
                let mut origin = 0u64;
                let mut from_fd: Option<i32> = None;
                let mut bad_count: Option<(String, String)> = None;
                // What separates elements; `-d` clusters (`-d,`).
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
                                // `-d ''` delimits on NUL, as for `read`.
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
                                // `-C callback` and `-c quantum`: ignored,
                                // nothing to call back into.
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
                            // `-t` removes the delimiter, whatever it is:
                            // with `-d,` on `a,b,c` bash keeps the `\n`
                            // ending the last field.
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
            // `json [-r] [PATH] [FILE]`: parse JSON, apply a dotted path
            // query (`.foo.bar[2]`, default `.`) and pretty-print the
            // result, like a small jq. `-r` prints a string unquoted, as
            // jq's does (`name=$(json -r .name config.json)`). As in jq, a
            // single argument is always the path, so `json . file.json`
            // reads a file. See json.rs.
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
                // `. /dev/stdin` names this command's stdin, which a
                // redirect or enclosing construct may supply, so it's read
                // as `read` reads it rather than by opening the real fd 0.
                let names_stdin = matches!(path.as_str(), "/dev/stdin" | "/dev/fd/0" | "/proc/self/fd/0");
                let stdin_redirected = cmd.redirects.iter().any(|r| {
                    matches!(
                        r,
                        Redirect::In(_)
                            | Redirect::HereString(_)
                            | Redirect::HereDoc(..)
                            | Redirect::FdDup { fd: 0, .. }
                            | Redirect::FdDupWord { fd: 0, .. }
                    )
                }) || self.stdio_override.as_ref().is_some_and(|o| o.borrow().stdin.is_some());
                let text = if names_stdin && stdin_redirected {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut self.read_input_source(cmd), &mut text).map(|_| text)
                } else {
                    std::fs::read_to_string(self.resolve_path(&path))
                };
                match text {
                    Ok(src) => {
                        // Meanwhile this file is the script: functions
                        // defined and frames made here name it. Restored
                        // after.
                        let outer_script = std::mem::replace(&mut self.script_name, path.clone());
                        // `. file arg...` gives the file its own positional
                        // parameters, restoring the caller's after. Without
                        // arguments it keeps the caller's, so `. lib.sh` in
                        // a function still sees `$1`.
                        let outer_args = match argv.len() > 2 {
                            true => Some(std::mem::replace(self.arg_frames.last_mut().expect("a positional frame"), argv[2..].to_vec())),
                            false => None,
                        };
                        self.source_depth += 1;
                        let result = self.run_source_here(&src, &path);
                        self.source_depth -= 1;
                        // `return` in the file ends the file, and `.`
                        // reports its code. A `return` in a function
                        // defined here is caught by `call_function`.
                        let result = match result {
                            ExecResult::Return(code) => ExecResult::Status(code),
                            other => other,
                        };
                        if let Some(args) = outer_args
                            && let Some(frame) = self.arg_frames.last_mut()
                        {
                            *frame = args;
                        }
                        self.script_name = outer_script;
                        // A sourced script fires RETURN when it finishes,
                        // `functrace` or not, unlike a function or `eval`.
                        self.run_pseudo_trap(PseudoTrap::Return);
                        return result;
                    }
                    Err(e) => {
                        // A directory gets the builtin's name and bash's
                        // lowercase wording; anything else the bare path
                        // and the system's message, as in bash 5.3.
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
                // `--` ends the options, which `trap -p` prints, so `trap`
                // can read its own output back.
                let argv: Vec<String> = match argv.iter().position(|a| a == "--") {
                    Some(at) => argv[..at].iter().chain(argv[at + 1..].iter()).cloned().collect(),
                    None => argv.to_vec(),
                };
                if argv.len() == 1 || argv.get(1).map(|s| s == "-p").unwrap_or(false) {
                    // `trap -p NAME...` reports only those; without names
                    // every trap, in bash's order: EXIT, signals by number,
                    // then the pseudo-signals.
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
                    for (n, action) in trap_signal_listing(&self.traps, ignored_at_entry_list()) {
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
                // A signal this shell can't name fails the command.
                let mut status = 0;
                for sig in &argv[2..] {
                    if sig == "EXIT" || sig == "0" {
                        self.exit_trap = if cmd_str == "-" { None } else { Some(cmd_str.clone()) };
                        self.exit_trap_depth = self.subshell_depth;
                        continue;
                    }
                    // The pseudo-signals, fired by the interpreter. `-`
                    // clears one, as for a real signal.
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
                    // KILL and STOP first: real signals left out of
                    // SIGNAL_NAMES so it can answer "may I catch this?".
                    // bash silently records a trap for them that never
                    // fires; saying so is more useful than a status a
                    // script would have to expect.
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
                    // Ignored before bish started, so not ours to change
                    // (see `IGNORED_AT_ENTRY`): accepted silently with no
                    // effect, as bash does, so a defensive `trap ... INT`
                    // doesn't start failing.
                    if ignored_at_entry(num) {
                        continue;
                    }
                    if cmd_str == "-" {
                        self.traps.remove(&num);
                        set_disposition(num, Disposition::Default);
                    } else if cmd_str.is_empty() {
                        self.traps.insert(num, TrapAction::Ignore);
                        set_disposition(num, Disposition::Ignored);
                    } else {
                        self.traps.insert(num, TrapAction::Run(cmd_str.clone()));
                        set_disposition(num, Disposition::Trapped);
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
                // `array_literal_args` is indexed into the original argv,
                // so positions shift back by one to match `argv[1..]`.
                let shifted: Vec<_> =
                    array_literal_args.iter().filter_map(|(p, n, m, i)| p.checked_sub(1).map(|p2| (p2, n.clone(), *m, i.clone()))).collect();
                return ExecResult::Status(crate::builtins::vars::run_declare(self, &name, &argv[1..], &shifted));
            }
            // `readonly` is `declare -r`, as `export` is `declare -x`, so
            // array literals (`readonly a=(1)`) keep their value. See
            // `export` for the positions.
            "readonly" => {
                let mut declare_args = vec!["-r".to_string()];
                declare_args.extend(argv[1..].iter().cloned());
                return ExecResult::Status(crate::builtins::vars::run_declare(self, &name, &declare_args, array_literal_args));
            }
            // `exec CMD [args...]` replaces this process
            // (`CommandExt::exec`, an execvp, not a fork), as in bash.
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
                // argv[0] for the new process: `-a NAME`, or the command
                // word with a leading dash under `-l`, which a login shell
                // looks for.
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
                // Inside a converted subshell or substitution, or a pane's
                // virtual-child Shell (`subshell_depth > 0`), an execve
                // would replace the one process every session shares. So
                // CMD runs as a separate child, as a forked subshell's exec
                // would replace only that subshell, and this unwinds with
                // `ExecResult::Exit` to end just this child.
                if self.subshell_depth > 0 {
                    let mut command = self.command(prog);
                    set_command_arg0(&mut command, &argv0);
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
                        // `$?` stays as it was, as in the top-level case
                        // below.
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
                set_command_arg0(&mut command, &argv0);
                if opts.clear_env {
                    command.env_clear();
                }
                command.args(rest);
                // Not through `apply_fd_redirects`' pre_exec: this execs
                // this process itself, with no child. Recorded in case the
                // execve fails, so the shell doesn't carry on with the
                // redirect applied.
                self.note_fd012_change();
                if let Err(e) = apply_fds_to_self(redirs.actions) {
                    sh_eprintln!(self, "bish: exec: {}", e);
                    return ExecResult::Status(1);
                }
                let err = exec_replacing_process(&mut command);
                // bash's wording here is just "not found".
                let text = match err.kind() == std::io::ErrorKind::NotFound {
                    true => "not found".to_string(),
                    false => os_message(&err),
                };
                sh_eprintln!(self, "bish: exec: {}: {}", prog, text);
                // A script exits when exec can't run the command: 127 for a
                // name not found, 126 for one that couldn't run, as for an
                // ordinary command word (bash 5.3).
                let status = match err.kind() == std::io::ErrorKind::NotFound {
                    true => 127,
                    false => 126,
                };
                self.run_exit_trap();
                return ExecResult::Exit(status);
            }
            // Bare `exec`: applies its redirects to this process for good,
            // numbered fds (`exec 3>file`, `exec 3>&-`) and 0-2
            // (`exec > file`) alike, so later commands inherit them:
            // `exec > logfile 2>&1`, or
            // `exec 3>&1 1>logfile; cmd; exec 1>&3 3>&-`.
            "exec" => {
                let redirs = match self.resolve_redirects(cmd) {
                    Ok(r) => r,
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}", e);
                        return ExecResult::Status(1);
                    }
                };
                // This repoints the process's own fds 0-2 in place, so it
                // records that.
                self.note_fd012_change();
                let moved: Vec<i32> = redirs.actions.iter().map(FdAction::target).collect();
                if let Err(e) = apply_fds_to_self(redirs.actions) {
                    sh_eprintln!(self, "bish: exec: {}", e);
                    return ExecResult::Status(1);
                }
                self.descriptors_to_follow = Some(moved);
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
        // Read back, not expanded again: `apply_prefix_assigns` already set
        // them, and expanding here would run each `$( )` twice.
        for (k, _, _) in &cmd.assigns {
            let v = self.lookup_var(k);
            command.env(k, v);
        }
        // While promoted, a foreground external gets a fresh pty instead of
        // the real terminal (see `Job::pty_master`): output written
        // straight to the screen would be overwritten by the next redraw
        // from the grid. Not promoted, it keeps the terminal and real job
        // control. Its own redirects are layered over the pty, so the other
        // streams still show in the pane. Only when its stdout would
        // otherwise be the terminal: if a `$( )` capture or an enclosing
        // `> file` already says where output goes, a pty would send it
        // where nobody reads (`x=$(date)` came back empty).
        let stdout_already_spoken_for = self.stdio_override.as_ref().is_some_and(|o| o.borrow().stdout.is_some());
        // Never in an in-process pipeline stage, whose descriptors the
        // pipeline already set (`PIPELINE_DRAIN` armed means exactly that):
        // a pty would replace the stage's stdin pipe, and `cat` would hang.
        let use_pty = self.is_promoted() && !stdout_already_spoken_for && !pipeline_drain_armed();

        if use_pty {
            if let Ok(p) = pty::open() {
                // Size the pty to the session's area before the child asks;
                // see `pty_size`.
                let (rows, cols) = self.pty_size();
                let _ = p.resize(rows, cols);
                let slave_path = p.slave_path.clone();
                let _ = pty::attach_on_exec(&mut command, &slave_path);
                // The pty, then descriptors something else decided, then
                // the command's own: pre_exec closures run in order, each
                // overwriting only what it names. The middle layer is a
                // redirect on an enclosing construct (`{ ...; } 2>e`),
                // which arrives as a `stdio_override`, not among this
                // command's actions; without it `2>e` captured nothing.
                let mut actions = Vec::new();
                if let Some(o) = self.stdio_override.as_ref()
                    && let Some(err) = o.borrow().stderr.as_ref().and_then(|f| f.try_clone().ok())
                {
                    actions.push(FdAction::Open { fd: 2, file: err });
                }
                // And stdin: `{ cat; } < file` in a pane must read the
                // file, not the pty. The same for a heredoc on a group or
                // function.
                if let Some(o) = self.stdio_override.as_ref()
                    && let Some(input) = o.borrow().stdin.as_ref().and_then(|r| r.borrow().file.try_clone().ok())
                {
                    actions.push(FdAction::Open { fd: 0, file: input });
                }
                actions.extend(redirs.actions);
                apply_fd_redirects(&mut command, actions);
                return match command.spawn() {
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
                            // Foreground: handed to repl.rs like a job
                            // brought back with `fg` (see
                            // `ExecResult::Fg`), whose `drive_fg_job`
                            // renders it and forwards input, Ctrl-C and
                            // Ctrl-Z included. `id: 0` is a placeholder; a
                            // real id is only needed if it stops
                            // (`park_stopped_fg_job`).
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
                        let (msg, status) = spawn_failure(&name, &e);
                        self.write_command_error(cmd, &msg);
                        self.drain_proc_subs();
                        ExecResult::Status(status)
                    }
                };
            }
            // If `pty::open` fails (fd exhaustion), the job still runs on
            // inherited stdio; it just can't be rendered by `fg` later.
        }

        // Only reached with at least one stream redirected. Backgrounded, the
        // others still need somewhere other than the real screen (see
        // `run_compound_redirected`).
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

        // Under `set -m`, this command gets its own process group from its
        // pid, so Ctrl-C and Ctrl-Z from the terminal reach only it. Set in
        // the child before exec and in the parent after spawn, avoiding the
        // race; setpgid is idempotent.
        if self.opt_monitor {
            #[cfg(unix)]
            unsafe {
                command.pre_exec(|| {
                    crate::platform::set_process_group(0, 0);
                    // bish ignores SIGTTIN/SIGTTOU
                    // (`term::ignore_tty_signals`), and that survives exec
                    // like SIGINT does (see `apply_fd_redirects`); reset,
                    // or a job later `bg`'d would ignore SIGTTIN instead of
                    // stopping on a read.
                    crate::platform::default_signal(crate::term::SIGTTIN);
                    crate::platform::default_signal(crate::term::SIGTTOU);
                    Ok(())
                });
            }
        }

        self.note_external_spawn();
        match command.spawn() {
            Ok(child) => {
                let pid = child.id();
                if self.opt_monitor {
                    crate::platform::set_process_group(pid as i32, pid as i32);
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
                    // Foreground with job control: give the child the
                    // terminal, wait for it to exit or stop
                    // (`waitpid_untraced`), then take the terminal back.
                    pty::tcsetpgrp(0, pid as i32).ok();
                    let outcome = waitpid_untraced(pid);
                    pty::tcsetpgrp(0, crate::platform::process_group()).ok();
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
                            // As in bash, `$?` is 128+signal for a
                            // foreground job that stopped.
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
                let (msg, status) = spawn_failure(&name, &e);
                self.write_command_error(cmd, &msg);
                self.drain_proc_subs();
                ExecResult::Status(status)
            }
        }
    }

    /// Whether this stage needs a shell, decided syntactically: expanding
    /// would run a `$( )` before its stage. A literal first word is checked
    /// against builtins and functions; anything else (a variable, a
    /// substitution, a glob) is assumed to need one, the safe direction,
    /// since an external through a shell still works.
    fn stage_needs_interpreter(&self, cmd: &parser::Command) -> bool {
        let parser::Command::Simple(sc) = cmd else { return true };
        let Some(first) = sc.words.first() else { return true };
        match first.chunks.as_slice() {
            // A quoted name is the same name (`'wc' -l` runs wc), as in
            // `run_single`. The serializer quotes every word, so a
            // re-exec'd construct arrives this way, and classing a quoted
            // external as a shell stage could hang a pipeline.
            [crate::lexer::Chunk::Str(name)] | [crate::lexer::Chunk::LiteralStr(name, _)] => {
                self.is_active_builtin(name) || self.functions.contains_key(name)
            }
            _ => true,
        }
    }

    /// A pipeline with two or more stages needing a shell, without extra
    /// processes. One shell stage can run inline between spawning the
    /// others and waiting; two would each block on a pipe the other holds,
    /// on one thread. So they run as coroutines, the scheduler giving the
    /// thread to whichever can move (see `scheduler`). Real commands stay
    /// real processes.
    fn run_multi_scheduled(&mut self, commands: &[parser::Command]) -> i32 {
        let n = commands.len();
        let is_shell_stage: Vec<bool> = commands.iter().map(|c| self.stage_needs_interpreter(c)).collect();

        // Pipe `i` carries stage `i`'s output to stage `i + 1`.
        let mut read_ends: Vec<Option<std::os::fd::OwnedFd>> = (0..n).map(|_| None).collect();
        let mut write_ends: Vec<Option<std::os::fd::OwnedFd>> = (0..n).map(|_| None).collect();
        for i in 0..n.saturating_sub(1) {
            match make_pipe() {
                Ok((read_end, write_end)) => {
                    // Left blocking: `briefly_nonblocking` sets the flag
                    // only around this shell's own reads and writes, so a
                    // stage's children never inherit it.
                    write_ends[i] = Some(write_end);
                    read_ends[i + 1] = Some(read_end);
                }
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    return 1;
                }
            }
        }

        // The same pane output the in-process path gets (see `run_multi`), so
        // `{ echo a; } | { read x; cat; }` shows in a pane.
        let pane_pty = match self.is_promoted() && self.stdio_override.is_none() {
            true => self.background_pty(),
            false => None,
        };
        let pane_slave = |pty: &Option<pty::Pty>| -> Option<Stdio> {
            let p = pty.as_ref()?;
            std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok().map(Stdio::from)
        };
        let mut pane_output = match (&pane_pty, self.sink_grid()) {
            (Some(p), Some(screen)) => {
                p.master.try_clone().ok().zip(std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok()).map(
                    |(master, keepalive)| {
                        pty::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&master));
                        PaneOutput { master, screen, _keepalive: keepalive }
                    },
                )
            }
            _ => None,
        };

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
                    Some(fd) => stdio_from_owned(fd),
                    None => self.spawn_stdin_stdio(),
                });
                command.stdout(match stdout_end {
                    Some(fd) => stdio_from_owned(fd),
                    None => pane_slave(&pane_pty).unwrap_or_else(|| self.spawn_stdout_stdio()),
                });
                if let Some(slave) = pane_slave(&pane_pty) {
                    command.stderr(slave);
                }
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

            // A shell stage, built and connected now and run later: every
            // stage must exist before any starts.
            //
            // A stage with a pipe on its stdout keeps `Real`, since the
            // scheduler points fd 1 at that pipe during each turn, as for a
            // separate process, so builtins, `exec {fd}<&0` and inheriting
            // externals behave as before. The last stage has no pipe, and
            // its output belongs where this shell's goes (a `$( )` capture,
            // a pane), so it inherits the sink and stdio like any
            // unredirected child, or
            // `x=$(printf 'a\nb\n' | { read v; echo $v; })` would print to
            // the terminal.
            let mut child = match stdout_end.is_some() {
                true => self.new_virtual_child(),
                false => self.child_for_stdio(ChildStdio::default()),
            };
            // A stage is a subshell, not the login shell; see
            // `run_body_in_child_shell`.
            child.login_shell = false;
            // ...and one bash forks, keeping its redirects (see
            // `run_exit_trap`).
            child.forked_subshell = true;
            // No DEBUG trap inside, where bash has none either: a simple
            // stage was already traced by `run_multi`, and a compound one
            // is a subshell that doesn't inherit it without `set -T`. Here
            // a trap would write into the stage's pipe.
            if matches!(cmd, parser::Command::Simple(_)) || !self.opt_functrace {
                child.debug_trap = None;
            }
            let body = cmd.clone();
            let code = Rc::clone(&codes[i]);
            let stage = move || {
                // Armed here: the scheduler swaps the watch per task, so
                // this is when this stage's is the thread's.
                arm_broken_pipe();
                let result = crate::builtins::shell::run_command(&mut child, &body, false);
                if !matches!(result, ExecResult::Exit(_)) {
                    child.run_exit_trap();
                }
                // A stage whose reader left dies as a separate process
                // would, with SIGPIPE's status.
                code.set(if disarm_broken_pipe() { 128 + 13 } else { result.status() });
            };
            // `Scheduler::run` runs pipeline stages to completion first, so
            // cancellation never reaches them.
            if let Err(e) = scheduler.add_with_fds(stage, stdin_end, stdout_end, true) {
                sh_eprintln!(self, "bish: {}", e);
                kill_all(children);
                return 1;
            }
        }
        // Any end this shell still holds is one a reader would wait on
        // forever.
        drop(read_ends);
        drop(write_ends);

        // A stage without a pipe on stdout gets this shell's fd 1 from
        // `install_fds`, so pointing fds 1 and 2 at the pane during the run
        // gives the last stage, and what it spawns, somewhere visible.
        // Armed as the drain too, so a blocked write empties the pty
        // instead of parking.
        let saved_fds = save_fd012();
        let stage_slave = match &pane_pty {
            Some(p) => std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok(),
            None => None,
        };
        if let Some(slave) = &stage_slave {
            use std::os::fd::AsRawFd;
            crate::platform::duplicate_onto(slave.as_raw_fd(), 1);
            crate::platform::duplicate_onto(slave.as_raw_fd(), 2);
        }
        PIPELINE_DRAIN.with(|d| *d.borrow_mut() = pane_output.take());
        scheduler.run();
        pane_output = PIPELINE_DRAIN.with(|d| d.borrow_mut().take());
        drop(stage_slave);
        restore_fd012(saved_fds);

        let mut statuses: Vec<i32> = codes.drain(..).map(|c| c.get()).collect();
        for (stage, mut child) in children {
            statuses[stage] = loop {
                if let Some(output) = pane_output.as_mut() {
                    output.drain();
                }
                match child.try_wait() {
                    Ok(Some(s)) => break exit_code_from_status(s),
                    Ok(None) => match pane_output.is_some() {
                        true => std::thread::sleep(std::time::Duration::from_millis(1)),
                        false => match child.wait() {
                            Ok(s) => break exit_code_from_status(s),
                            Err(e) => {
                                sh_eprintln!(self, "bish: {}", e);
                                break 1;
                            }
                        },
                    },
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}", e);
                        break 1;
                    }
                }
            };
        }
        if let Some(output) = pane_output.as_mut() {
            output.drain();
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
        // Every simple stage is traced before any runs, as in bash: all
        // three traces of a three-stage pipeline come before its output.
        // Compound stages aren't traced, inside either: a group is a
        // subshell, and DEBUG isn't inherited into one.
        if self.debug_trap.is_some() {
            for cmd in commands {
                if let parser::Command::Simple(sc) = cmd
                    && !sc.words.is_empty()
                {
                    self.bash_command = crate::serialize::rendered_command(sc);
                    self.run_pseudo_trap(PseudoTrap::Debug);
                }
            }
        }
        // Stages that never started, and their status. A pipeline isn't over
        // because one command doesn't exist, so these join `codes`.
        let mut never_started: Vec<(usize, i32)> = Vec::new();
        // The previous stage's read end, as an fd, since `lastpipe` needs
        // one.
        let mut prev_stdout: Option<std::os::fd::OwnedFd> = None;
        // For a backgrounded pipeline only, every stage joins one process
        // group seeded from the first stage's pid (`None` until then), so
        // `kill %N` and `bg` reach them all, as in `run_single`. Not for a
        // foreground pipeline: without the `tcsetpgrp` and stop handling,
        // isolating it would stop Ctrl-C from reaching it.
        let mut pgid: Option<i32> = None;

        // `shopt -s lastpipe`: the last stage runs in this shell, so
        // `seq 3 | while read l; do n=$l; done` leaves `n` set. As in bash,
        // only with job control off and in the foreground; also only when
        // no enclosing construct redirects stdin, since the stage's stdin
        // is dup2'd onto fd 0.
        let lastpipe = n > 1 && !background && !self.opt_monitor && self.stdio_override.is_none() && self.shopt_is_on("lastpipe");

        // Two or more stages needing a shell can't take turns inline, each
        // waiting on a pipe the other holds, so they go to the scheduler.
        if !background && !lastpipe && n >= 2 && commands.iter().filter(|c| self.stage_needs_interpreter(c)).count() >= 2 {
            return self.run_multi_scheduled(commands);
        }
        // The one stage that can run in this shell: when exactly one stage
        // needs a shell, the others are real processes on the far ends of
        // its pipes, so it can run inline after they start without
        // deadlocking. `lastpipe` already claims that slot where it applies.
        let inproc_stage = if background || n < 2 || lastpipe {
            None
        } else {
            let mut shell_stages = commands.iter().enumerate().filter(|(_, c)| self.stage_needs_interpreter(c)).map(|(i, _)| i);
            match (shell_stages.next(), shell_stages.next()) {
                (Some(only), None) => Some(only),
                _ => None,
            }
        };
        // Its stdin and stdout, once the loop below works them out.
        let mut inproc_stdio: Option<(Option<std::os::fd::OwnedFd>, Option<std::os::fd::OwnedFd>)> = None;

        let bg_pty = if background { self.background_pty() } else { None };
        let bg_slave = |pty: &Option<pty::Pty>| -> Option<Stdio> {
            let p = pty.as_ref()?;
            std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok().map(Stdio::from)
        };

        // A foreground pipeline in a pane needs somewhere for its output:
        // inherited, it goes to the terminal under the compositor, so
        // `ls | grep x` showed nothing. A pty, as a single external gets,
        // so the stages still see a terminal.
        let pane_pty = match !background && self.is_promoted() && self.stdio_override.is_none() {
            true => self.background_pty(),
            false => None,
        };
        let pane_slave = |pty: &Option<pty::Pty>| -> Option<Stdio> {
            let p = pty.as_ref()?;
            std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok().map(Stdio::from)
        };
        let mut pane_output = match (&pane_pty, self.sink_grid()) {
            (Some(p), Some(screen)) => {
                p.master.try_clone().ok().zip(std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok()).map(
                    |(master, keepalive)| {
                        pty::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&master));
                        PaneOutput { master, screen, _keepalive: keepalive }
                    },
                )
            }
            _ => None,
        };

        for (i, cmd) in commands.iter().enumerate() {
            let is_last = i == n - 1;
            if is_last && lastpipe {
                break;
            }
            if Some(i) == inproc_stage {
                // Not spawned: its fds are recorded and it runs below,
                // after the others start.
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
            // Only the first stage reads the pty; the rest read the
            // previous pipe.
            let default_stdin = match prev_stdout.take() {
                Some(prev) => stdio_from_owned(prev),
                None => bg_slave(&bg_pty).unwrap_or_else(|| self.spawn_stdin_stdio()),
            };
            let default_stdout = match is_last {
                true => bg_slave(&bg_pty).or_else(|| pane_slave(&pane_pty)).unwrap_or_else(|| self.spawn_stdout_stdio()),
                false => Stdio::piped(),
            };
            let mut default_stderr = bg_slave(&bg_pty).or_else(|| pane_slave(&pane_pty));

            // What to call this stage if it won't start: only a simple
            // command has a name worth reporting; other kinds re-exec this
            // shell.
            let mut stage_name: Option<String> = None;
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
                    // Builtins and functions aren't executables, so they
                    // re-exec this shell with the already-expanded argv as
                    // single-quoted literals, so nothing is expanded twice.
                    stage_name = Some(argv[0].clone());
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
                        let command = self.reexec_running(exe, script);
                        command
                    } else {
                        let mut command = self.command(&argv[0]);
                        command.args(&argv[1..]);
                        command
                    };
                    let mut traced: Vec<String> = Vec::new();
                    for (k, mode, val) in &sc.assigns {
                        let v = self.expand_word(val);
                        let v = match mode {
                            AssignMode::Set => v,
                            AssignMode::Append => self.appended_value(k, &v),
                        };
                        // Always `=` with the value the child sees: bash
                        // traces a prefix `p+=y` as `p=xy`.
                        if self.opt_xtrace {
                            traced.push(format!("{}={}", k, xtrace_quote(&v)));
                        }
                        command.env(k, v);
                    }
                    // Traced here, since a stage never goes through
                    // `run_simple`, so `set -x; echo a | cat` traces both
                    // stages. The in-shell stage traces itself when it
                    // runs, later than bash (the
                    // `xtrace-traces-a-stage-running-in-this-shell-last`
                    // divergence).
                    if self.opt_xtrace {
                        let ps4 = self.xtrace_prefix();
                        for line in &traced {
                            sh_eprintln!(self, "{}{}", ps4, line);
                        }
                        let words: Vec<String> = argv.iter().map(|w| xtrace_quote_word(w)).collect();
                        sh_eprintln!(self, "{}{}", ps4, words.join(" "));
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
                    let mut command = self.reexec_running(exe, script);
                    command.stdin(default_stdin);
                    command.stdout(default_stdout);
                    command.stderr(default_stderr.take().unwrap_or_else(|| self.spawn_stderr_stdio()));
                    apply_fd_redirects(&mut command, redirs.actions);
                    command
                }
            };
            command.current_dir(&self.cwd);

            // Set in the child and in the parent after spawn, avoiding the
            // race, as in `run_single`.
            if background && self.opt_monitor {
                #[cfg(unix)]
                let join_pgid = pgid;
                #[cfg(unix)]
                unsafe {
                    command.pre_exec(move || {
                        crate::platform::set_process_group(0, join_pgid.unwrap_or(0));
                        crate::platform::default_signal(crate::term::SIGTTIN);
                        crate::platform::default_signal(crate::term::SIGTTOU);
                        Ok(())
                    });
                }
            }

            // The last stage's stdout is the real terminal, so record it; see
            // `ran_external_since_prompt`.
            if is_last {
                self.note_external_spawn();
            }
            match command.spawn() {
                Ok(mut child) => {
                    if background && self.opt_monitor {
                        let cpid = child.id() as i32;
                        crate::platform::set_process_group(cpid, pgid.unwrap_or(cpid));
                        if pgid.is_none() {
                            pgid = Some(cpid);
                        }
                    }
                    if !is_last {
                        prev_stdout = child_stdout_as_owned(child.stdout.take());
                    }
                    children.push((i, child));
                }
                Err(e) => {
                    // A stage that won't start exited 127; the pipeline
                    // carries on, as in bash: `nosuchcmd | head -1` leaves
                    // `head` reading empty input, giving PIPESTATUS
                    // `(127 0)` and `$?` from the last stage.
                    let (msg, status) = match &stage_name {
                        Some(name) => spawn_failure(name, &e),
                        None => (format!("bish: {e}"), 127),
                    };
                    sh_eprintln!(self, "{}", msg);
                    never_started.push((i, status));
                    // Nothing will write to the next stage's input, so it
                    // sees end of input at once.
                    prev_stdout = None;
                    continue;
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
            let saved = save_fd012();
            if let Some(read_end) = prev_stdout.take() {
                let fd = read_end;
                crate::platform::duplicate_onto(std::os::fd::AsRawFd::as_raw_fd(&fd), 0);
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

        // The stage that runs here, after every other stage is spawned and
        // before any is waited for: they drain the pipe it writes and fill
        // the pipe it reads.
        let mut inproc_code = None;
        if let (Some(stage), Some((stdin_fd, stdout_fd))) = (inproc_stage, inproc_stdio.take()) {
            use std::os::fd::AsRawFd;
            let saved = save_fd012();
            if let Some(fd) = &stdin_fd {
                crate::platform::duplicate_onto(fd.as_raw_fd(), 0);
            }
            // The last stage has no pipe to write into, so its fd 1 would
            // be the real terminal, which a pane paints over. A builtin
            // writes to the grid anyway, but what it spawns doesn't:
            // `echo a | { read x; cat; }` would lose the `cat`. The same
            // for stderr.
            let pane_slave = match (stdout_fd.is_none(), &pane_pty) {
                (true, Some(p)) => std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok(),
                _ => None,
            };
            if let Some(slave) = &pane_slave {
                crate::platform::duplicate_onto(slave.as_raw_fd(), 1);
                crate::platform::duplicate_onto(slave.as_raw_fd(), 2);
            }
            if let Some(fd) = &stdout_fd {
                crate::platform::duplicate_onto(fd.as_raw_fd(), 1);
            }
            // A virtual child, since every pipeline stage is a subshell in
            // bash and an assignment mustn't escape; `lastpipe` above
            // deliberately doesn't. `EPIPE` is watched only around it.
            arm_broken_pipe();
            // The write end is also this stage's explicit stdout, not just
            // fd 1: a builtin writes to the shell's sink, still the
            // enclosing `$( )` capture or pane, so `echo hello | wc -c`
            // would bypass the pipe. As a redirect, the sink follows it, as
            // for `echo hello >file`.
            let stdio = match stdout_fd.as_ref().and_then(|fd| fd.try_clone().ok()) {
                Some(fd) => ChildStdio { stdout: Some(Capture::file(std::fs::File::from(fd))), ..ChildStdio::default() },
                None => ChildStdio::default(),
            };
            // Armed for the duration: this stage runs inline, so nothing
            // else empties the pane's pty while it writes to a pipe whose
            // reader fills that pty. See PIPELINE_DRAIN.
            PIPELINE_DRAIN.with(|d| *d.borrow_mut() = pane_output.take());
            // No DEBUG trap meanwhile: a simple stage was traced above with
            // the others, and a compound stage is a subshell that doesn't
            // inherit it (`{ echo g; } | cat` traces only `cat` in bash).
            // In here its output would go into the pipe. `set -T` is the
            // exception.
            let hushed = match self.opt_functrace {
                true => None,
                false => self.debug_trap.take(),
            };
            let result = self.run_command_in_child_shell(&commands[stage], stdio);
            if hushed.is_some() {
                self.debug_trap = hushed;
            }
            pane_output = PIPELINE_DRAIN.with(|d| d.borrow_mut().take());
            let broken = disarm_broken_pipe();
            restore_fd012(saved);
            // Dropped before waiting: downstream sees end of input when the
            // last copy of this write end closes, and this shell holds one.
            drop(stdout_fd);
            drop(stdin_fd);
            // No `pending_exit`: the stage is a subshell, so its `exit`
            // ends the stage, as `run_command_in_child_shell` already made
            // it. `lastpipe`'s stage, not a subshell, differs there.
            inproc_code = Some(if broken { 128 + 13 } else { result.status() });
        }

        // By stage, not completion order: PIPESTATUS is positional, and an
        // in-shell stage never went through `children`.
        let mut codes: Vec<Option<i32>> = vec![None; n];
        for (stage, status) in never_started {
            codes[stage] = Some(status);
        }
        for (stage, mut c) in children {
            // Drained while waiting: a stage filling the pty stops until
            // this thread reads it.
            let code = loop {
                if let Some(output) = pane_output.as_mut() {
                    output.drain();
                }
                match c.try_wait() {
                    Ok(Some(s)) => break exit_code_from_status(s),
                    Ok(None) => match pane_output.is_some() {
                        true => std::thread::sleep(std::time::Duration::from_millis(1)),
                        // Nothing to keep flowing: a blocking wait.
                        false => match c.wait() {
                            Ok(s) => break exit_code_from_status(s),
                            Err(e) => {
                                sh_eprintln!(self, "bish: {}", e);
                                break 1;
                            }
                        },
                    },
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}", e);
                        break 1;
                    }
                }
            };
            codes[stage] = Some(code);
        }
        // Every slave is closed, so what's in the master is all there is.
        if let Some(output) = pane_output.as_mut() {
            output.drain();
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
                Chunk::Str(t) | Chunk::LiteralStr(t, _) => s.push_str(t),
                Chunk::Var { name, .. } => {
                    let name = name.clone();
                    self.check_nounset(&name);
                    s.push_str(&self.lookup_var(&name));
                }
                Chunk::Sub { raw, .. } => s.push_str(&self.run_command_substitution(raw)),
                Chunk::Arith { raw, .. } => match self.eval_arith(raw) {
                    Ok(v) => s.push_str(&v.to_string()),
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}: {}", raw, e);
                        // Fatal, unlike the `(( ))` command; see
                        // `expansion_failed`.
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

    // An associative index is a string, expanded rather than evaluated. The
    // raw text is relexed, since `with space` lexes as more than one word.
    pub(crate) fn expand_index_as_string(&mut self, index: &str) -> String {
        self.expand_raw(index)
    }

    // A negative index counts back from one past the highest set index, so
    // -1 is the last element, as bash defines it. Indexed arrays only.
    pub(crate) fn resolve_array_index(&self, name: &str, i: i64) -> Option<usize> {
        if i >= 0 {
            return Some(i as usize);
        }
        let max = *self.arrays.get(name)?.keys().next_back()?;
        let resolved = max as i64 + 1 + i;
        if resolved >= 0 { Some(resolved as usize) } else { None }
    }

    // Every array accessor starts here: through a nameref (`declare -n r=m`),
    // `${r[k]}` reaches `m`'s element, as `$r` reaches its value.
    pub(crate) fn array_target(&self, name: &str) -> String {
        if self.nameref_names.contains(name) { self.resolve_nameref(name) } else { name.to_string() }
    }

    // An element by index: `@`/`*` join every element in order with spaces;
    // anything else is arithmetic (`${arr[i+1]}`), 0-based. An unset index
    // reads empty.
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
            // A subscript that won't evaluate fails like a `$(( ))` that
            // won't: reported and fatal.
            Err(e) => {
                sh_eprintln!(self, "bish: {}: {}", index, e);
                self.expansion_failed = true;
                String::new()
            }
        }
    }

    // Whether this index is present in the sparse array, not just empty.
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

    // `@`/`*` count only set elements: `arr[10]=x` alone has length 1.
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

    // `declare 'a[0]=5'` reaches the same element assignment as `a[0]=5`,
    // from `run_declare` outside this module.
    pub(crate) fn array_set_index_public(&mut self, name: &str, index: &str, value: String) {
        self.array_set_index(name, index, value);
    }

    // `arr[i]=value`: sets exactly that index in the sparse map, leaving
    // gaps unset, as in bash. Returns the index written (`None` for an
    // associative array or a bad index), so `apply_array_literal` knows
    // where its running index resumes after an explicit `[i]=`.
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

    // Applies a `name=(...)` or `name+=(...)` literal to `arrays` or
    // `assoc_arrays`, by what `name` already is. `[index]=value` elements
    // and positional ones mix: a positional element continues from a
    // running index that a keyed one moves, as in bash (`(1 [5]=x 2)` gives
    // 0, 5, 6). In an associative array an unkeyed element is keyed by the
    // counter's text, where bash errors.
    pub(crate) fn apply_array_literal(&mut self, name: &str, mode: AssignMode, items: &[ArrayLiteralItem]) -> bool {
        // The scalar write's guard and message (see `assign_var_impl`), so
        // `readonly a=(1); a+=(2)` is refused.
        if self.name_is_readonly(name) {
            sh_eprintln!(self, "bish: {}: readonly variable", name);
            return false;
        }
        let is_assoc = self.assoc_names.contains(name);
        // The right side is expanded before the old value goes:
        // `a=("${a[@]}" 3)` has to see the array it replaces, so
        // `a=("${a[@]/x/y}")` rewrites in place. One field per element, not
        // per source word: `b=("${a[@]}")` copies, and `c=($unquoted)`
        // splits.
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

    // A display string for an array literal in a `declare`-family command,
    // for `set -x` only (see `SimpleCommand::array_word_assigns`); applying
    // it uses the structured items. Values are single-quoted
    // (`serialize::quote_literal`); a `[index]=` key shows its raw source,
    // as bash's xtrace does.
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

    // Word splitting: unquoted expansion results split on IFS into fields;
    // literal text never splits (unquoted whitespace already ended the
    // word). Only where splitting applies (argv, `for` lists); assignment
    // values, case words and redirect targets use plain `expand_word`.
    //
    // Each field comes with a pattern copy in which quoted chunks are
    // escaped with `glob::escape` and unquoted ones (an unquoted `$var`
    // holding `*` included) stay glob syntax, for `expand_words` to glob
    // against. Built in one pass, since expansions can have side effects
    // (`$( )`, `${x:=default}`, `$((x++))`) that mustn't run twice.
    fn expand_word_split(&mut self, w: &Word) -> Vec<Field> {
        let ifs = self.get_ifs();
        let mut fields: Vec<Field> = Vec::new();
        let mut current: Option<FieldBuilder> = None;
        for c in &w.chunks {
            match c {
                // The expanded home directory is text: a home holding `*`
                // mustn't glob.
                Chunk::Tilde { name } => {
                    let name = name.clone();
                    let home = self.expand_tilde(&name);
                    current.get_or_insert_with(FieldBuilder::default).push_inert(&home);
                }
                Chunk::Str(t) => {
                    current.get_or_insert_with(FieldBuilder::default).push(t);
                }
                Chunk::LiteralStr(t, _) => {
                    // Quoted or escaped source text: always escaped in the
                    // pattern copy.
                    current.get_or_insert_with(FieldBuilder::default).push_inert(t);
                }
                Chunk::Var { name, quoted, .. } => {
                    // Quoted `"$@"` expands to one field per positional
                    // parameter, unlike `"$*"`, which joins. Unquoted, both
                    // join and split, as in bash.
                    if name == "@" && *quoted {
                        let parts = self.arg_frames.last().cloned().unwrap_or_default();
                        append_parts(&mut fields, &mut current, &parts);
                    } else {
                        let name = name.clone();
                        self.check_param_name(&name);
                        self.check_nounset(&name);
                        let v = self.lookup_var(&name);
                        append_splittable(&mut fields, &mut current, &v, *quoted, &ifs);
                    }
                }
                Chunk::Sub { raw, quoted, .. } => {
                    let v = self.run_command_substitution(raw);
                    append_splittable(&mut fields, &mut current, &v, *quoted, &ifs);
                }
                Chunk::Arith { raw, quoted } => {
                    let v = match self.eval_arith(raw) {
                        Ok(n) => n.to_string(),
                        Err(e) => {
                            sh_eprintln!(self, "bish: {}: {}", raw, e);
                            // A `$(( ))` that doesn't parse is fatal,
                            // unlike the `(( ))` command.
                            self.expansion_failed = true;
                            String::new()
                        }
                    };
                    append_splittable(&mut fields, &mut current, &v, *quoted, &ifs);
                }
                Chunk::VarExpand { name, op, quoted } => {
                    let name = name.clone();
                    let op = op.clone();
                    // A quoted slice of `$@` is one field per element, as
                    // `"$@"` is.
                    if let Some(sliced) = self.list_slice(&name, None, &op) {
                        let items = self.reported_slice(sliced);
                        if name == "@" && *quoted {
                            append_parts(&mut fields, &mut current, &items);
                        } else {
                            let joined = items.join(&self.ifs_join_char());
                            append_splittable(&mut fields, &mut current, &joined, *quoted, &ifs);
                        }
                        continue;
                    }
                    let v = self.eval_var_op(&name, &op);
                    append_splittable(&mut fields, &mut current, &v, *quoted, &ifs);
                }
                Chunk::ArrayVar { name, index, quoted } => {
                    // `"${arr[@]}"` is one field per element, like `"$@"`;
                    // `"${arr[*]}"` and unquoted `${arr[@]}` join with a
                    // space first, like `$*`.
                    if index == "@" && *quoted {
                        let items = self.array_all(name);
                        append_parts(&mut fields, &mut current, &items);
                    } else if index == "@" || index == "*" {
                        let sep = self.ifs_join_char();
                        let joined = self.array_all(name).join(&sep);
                        append_splittable(&mut fields, &mut current, &joined, *quoted, &ifs);
                    } else {
                        let name = name.clone();
                        let index = index.clone();
                        let v = self.array_element(&name, &index);
                        append_splittable(&mut fields, &mut current, &v, *quoted, &ifs);
                    }
                }
                Chunk::ArrayLength { name, index } => {
                    let name = name.clone();
                    let index = index.clone();
                    let v = self.array_length(&name, &index).to_string();
                    append_splittable(&mut fields, &mut current, &v, true, &ifs);
                }
                Chunk::ArrayVarExpand { name, index, op, quoted } => {
                    let name = name.clone();
                    let index = index.clone();
                    let op = op.clone();
                    if let Some(sliced) = self.list_slice(&name, Some(&index), &op) {
                        let items = self.reported_slice(sliced);
                        if index == "@" && *quoted {
                            append_parts(&mut fields, &mut current, &items);
                        } else {
                            let joined = items.join(&self.ifs_join_char());
                            append_splittable(&mut fields, &mut current, &joined, *quoted, &ifs);
                        }
                        continue;
                    }
                    let v = self.eval_array_var_op(&name, &index, &op);
                    append_splittable(&mut fields, &mut current, &v, *quoted, &ifs);
                }
                Chunk::Indirect { name, quoted } => {
                    let v = self.indirect_var(name);
                    append_splittable(&mut fields, &mut current, &v, *quoted, &ifs);
                }
                Chunk::ArrayKeys { name, quoted } => {
                    // As for `${arr[@]}`: quoted `@` is one field per key.
                    if *quoted {
                        let items = self.array_keys(name);
                        append_parts(&mut fields, &mut current, &items);
                    } else {
                        let sep = self.ifs_join_char();
                        let joined = self.array_keys(name).join(&sep);
                        append_splittable(&mut fields, &mut current, &joined, *quoted, &ifs);
                    }
                }
                Chunk::VarNamesMatchingPrefix { prefix, at, quoted } => {
                    // The same rules as `${!arr[@]}`, with `@` and `*` told
                    // apart: only quoted `@` gives one field per name.
                    let names = self.var_names_with_prefix(prefix);
                    if *at && *quoted {
                        append_parts(&mut fields, &mut current, &names);
                    } else {
                        let joined = names.join(" ");
                        append_splittable(&mut fields, &mut current, &joined, *quoted, &ifs);
                    }
                }
                Chunk::ProcSubIn { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_in(&raw);
                    append_splittable(&mut fields, &mut current, &v, true, &ifs);
                }
                Chunk::ProcSubOut { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_out(&raw);
                    append_splittable(&mut fields, &mut current, &v, true, &ifs);
                }
            }
        }
        if let Some(b) = current {
            fields.push(b.finish());
        }
        fields
    }

    // `FIGNORE` split into the suffixes completion leaves out, without empty
    // entries, so a stray `:` can't match everything.
    pub fn fignore_suffixes(&self) -> Vec<String> {
        self.raw_var_lookup("FIGNORE").split(':').filter(|s| !s.is_empty()).map(str::to_string).collect()
    }

    // The default IFS (space, tab, newline) when unset; its value, which may
    // be empty (no splitting), when assigned.
    fn get_ifs(&mut self) -> String {
        if self.var_is_set("IFS") { self.lookup_var("IFS") } else { " \t\n".to_string() }
    }

    // Arithmetic over text that may contain a substitution: bash expands
    // `$( )` and backticks first, and arith.rs can't run commands. Only when
    // there's one: arith.rs reads `$x` and `${x}` itself, and a bare `x` must
    // stay a name so `x=y; y=2; echo $((x))` gives 2.
    pub(crate) fn eval_arith(&mut self, raw: &str) -> Result<i64, String> {
        // `${...}` expands here too: arith.rs reads plain `${x}` but not
        // ones with an operator (`${x:-5}`, `${#x}`), which would come out
        // as 0. Nothing to evaluate is 0, written or after expansion
        // (`$(( ${unset} ))`), as in bash.
        if raw.trim().is_empty() {
            return Ok(0);
        }
        // Any `$`, not only `$(` and `${`: a bare `$x` inside a larger token
        // (`$((10#$x))`) can't be read by arith.rs. bash expands the whole
        // expression before evaluating.
        if raw.contains('$') || raw.contains('`') {
            let expanded = self.expand_raw(raw);
            if expanded.trim().is_empty() {
                return Ok(0);
            }
            return arith::eval(&expanded, self);
        }
        arith::eval(raw, self)
    }

    // What a `~` prefix stands for: empty is `$HOME`, `+` and `-` are `$PWD`
    // and `$OLDPWD`, anything else a user name from /etc/passwd. A name with
    // no home stays as written, as in bash: `~nosuchuser/x` is literal.
    fn expand_tilde(&mut self, name: &str) -> String {
        match name {
            "" => self.lookup_var("HOME"),
            "+" => self.lookup_var("PWD"),
            "-" => self.lookup_var("OLDPWD"),
            user => home_of_user(user).unwrap_or_else(|| format!("~{user}")),
        }
    }

    // `PS4`, which `set -x` puts before each traced line, `+ ` by default.
    // bash repeats its first character per nesting level; nothing here tracks
    // a trace depth, so it's flat.
    fn xtrace_prefix(&mut self) -> String {
        if !self.var_is_set("PS4") {
            return String::new();
        }
        // Expanded, as bash does (parameters, substitutions, prompt
        // escapes, as `${v@P}` does): `PS4='+$LINENO '` is why anyone sets
        // it.
        let raw = self.lookup_var("PS4");
        // With tracing off meanwhile: a `$( )` in PS4 would be traced,
        // expanding PS4 again, recursing until the nesting guard stopped
        // it.
        let tracing = std::mem::replace(&mut self.opt_xtrace, false);
        let expanded = self.expand_raw(&raw);
        let prefix = self.expand_prompt_string(&expanded);
        self.opt_xtrace = tracing;
        prefix
    }

    // Relexes and expands a `${...}` operand, so it can hold expansions.
    // Parsed as one word (`parse_expansion_word`): unquoted whitespace in it
    // is content, not a separator.
    fn expand_raw(&mut self, raw: &str) -> String {
        if expands_to_itself(raw) {
            return raw.to_string();
        }
        let chunks = crate::lexer::parse_expansion_word(raw);
        self.expand_word(&Word { chunks, globbable: false })
    }

    /// `expand_raw` for the pattern of `${v#pat}`, `${v%pat}`,
    /// `${v/pat/repl}` and `${v^pat}`, where quoting decides whether a
    /// metacharacter is syntax: `${x//\*/Y}` replaces a literal asterisk,
    /// `${x//*/Y}` everything. It's `expand_glob_pattern_operand`, working on
    /// chunks, which still know what was quoted.
    fn expand_glob_pattern_raw(&mut self, raw: &str) -> String {
        if expands_to_itself(raw) {
            return raw.to_string();
        }
        let chunks = crate::lexer::parse_expansion_word(raw);
        self.expand_glob_pattern_operand(&Word { chunks, globbable: false })
    }

    /// `expand_raw` for a `${v/pat/repl}` replacement, where `&` stands for
    /// the match. Whether an `&` was typed or came from a variable has to
    /// survive quote removal: an escaped or quoted `&` is a `LiteralStr`, so
    /// it's re-escaped here and `expand_replacement` unescapes it at the end,
    /// as `expand_glob_pattern_operand` does for metacharacters.
    fn expand_replacement_operand(&mut self, raw: &str) -> String {
        if expands_to_itself(raw) {
            return raw.to_string();
        }
        let chunks = crate::lexer::parse_expansion_word(raw);
        let mut out = String::new();
        for chunk in &chunks {
            match chunk {
                // Escaped or quoted: text, whatever it is.
                crate::lexer::Chunk::LiteralStr(t, _) => {
                    for c in t.chars() {
                        if c == '&' || c == '\\' {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                }
                // Bare text and expansion results keep `&` special, which
                // `r='&'; ${s//X/$r}` relies on.
                other => out.push_str(&self.expand_word(&Word { chunks: vec![other.clone()], globbable: false })),
            }
        }
        out
    }

    fn eval_var_op(&mut self, name: &str, op: &VarOp) -> String {
        let cur = self.lookup_var(name);
        match op {
            // `${#@}` and `${#*}` are `$#`, a count, not the joined length.
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
                    // bash's wording when `${x:?}` has no message.
                    let msg = match self.expand_raw(word) {
                        m if m.is_empty() => "parameter null or not set".to_string(),
                        m => m,
                    };
                    sh_eprintln!(self, "bish: {}: {}", name, msg);
                    // Stopping is the point of `${x:?}`.
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
                let pattern = self.expand_glob_pattern_raw(pattern);
                strip_prefix_glob(&cur, &pattern, *longest)
            }
            VarOp::RemoveSuffix { pattern, longest } => {
                let pattern = self.expand_glob_pattern_raw(pattern);
                strip_suffix_glob(&cur, &pattern, *longest)
            }
            VarOp::CaseConvert { pattern, upper, all } => {
                let pattern = self.expand_glob_pattern_raw(pattern);
                apply_case_convert(&cur, &pattern, *upper, *all)
            }
            VarOp::Substring { offset, length } => {
                // An omitted offset (`${x::3}`) is 0; the evaluator rejects
                // an empty string.
                let off = match offset.trim().is_empty() {
                    true => 0,
                    false => self.eval_arith(offset).unwrap_or(0),
                };
                let len = length.as_ref().and_then(|l| self.eval_arith(l).ok());
                substring_expand(&cur, off, len)
            }
            VarOp::Replace { pattern, repl, global, anchor } => {
                let pattern = self.expand_glob_pattern_raw(pattern);
                let repl = self.expand_replacement_operand(repl);
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

    // `${a[@]:off:len}` and `${@:off:len}`: a slice of a list counts
    // elements (`${a[@]:1:2}` is two elements, not two characters), where
    // every other operator works on the joined text. `None` when it isn't
    // that, and the caller falls through to the text path.
    fn list_slice(&mut self, name: &str, index: Option<&str>, op: &VarOp) -> Option<Result<Vec<String>, String>> {
        // `${a[@]@Q}` and friends transform each element into a word of its
        // own: `"${a[@]@Q}"` is how a script requotes an array for `eval`.
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
            // `${a[@]:...}`: the elements in index order, sliced by
            // position among the set ones, as bash does for a sparse array.
            Some("@") | Some("*") => self.array_all(name),
            Some(_) => return None,
            // `${@:...}`: `$0` first, so `${@:0}` is the script and
            // `${@:1}` the first argument.
            None if name == "@" || name == "*" => {
                std::iter::once(self.script_name.clone()).chain(self.arg_frames.last().cloned().unwrap_or_default()).collect()
            }
            None => return None,
        };
        // An omitted offset (`${a[@]::2}`) is 0.
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

    // A failed slice (a negative length) reports and yields nothing, as in
    // bash.
    fn reported_slice(&mut self, sliced: Result<Vec<String>, String>) -> Vec<String> {
        match sliced {
            Ok(items) => items,
            Err(e) => {
                sh_eprintln!(self, "bish: {e}");
                // A word that couldn't expand is fatal in a script; see
                // `expansion_failed`.
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
    // character, a space when unset, nothing when empty.
    fn ifs_join_char(&mut self) -> String {
        match self.var_is_set("IFS") {
            false => " ".to_string(),
            true => self.lookup_var("IFS").chars().next().map(String::from).unwrap_or_default(),
        }
    }

    // The operators that depend only on one string, applicable to a scalar
    // or to each element.
    fn apply_string_var_op(&mut self, cur: &str, op: &VarOp) -> String {
        match op {
            VarOp::RemovePrefix { pattern, longest } => {
                let pattern = self.expand_glob_pattern_raw(pattern);
                strip_prefix_glob(cur, &pattern, *longest)
            }
            VarOp::RemoveSuffix { pattern, longest } => {
                let pattern = self.expand_glob_pattern_raw(pattern);
                strip_suffix_glob(cur, &pattern, *longest)
            }
            VarOp::CaseConvert { pattern, upper, all } => {
                let pattern = self.expand_glob_pattern_raw(pattern);
                apply_case_convert(cur, &pattern, *upper, *all)
            }
            VarOp::Replace { pattern, repl, global, anchor } => {
                let pattern = self.expand_glob_pattern_raw(pattern);
                let repl = self.expand_replacement_operand(repl);
                glob_replace(cur, &pattern, &repl, *global, *anchor)
            }
            _ => cur.to_string(),
        }
    }

    // `eval_var_op`'s operators on one array element, reading (and for `:=`
    // writing) it. `@`/`*` mean the joined elements, as in a non-splitting
    // expansion.
    fn eval_array_var_op(&mut self, name: &str, index: &str, op: &VarOp) -> String {
        // `${a[@]OP}` applies a string operator to each element and
        // rejoins: on the joined text, `${a[@]/o/0}` on `(one two)` would
        // change only the first. Not every operator is element-wise:
        // slicing, `${#a[@]}` and `${a[@]:-x}` are about the whole array
        // and are handled elsewhere.
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
                let pattern = self.expand_glob_pattern_raw(pattern);
                strip_prefix_glob(&cur, &pattern, *longest)
            }
            VarOp::RemoveSuffix { pattern, longest } => {
                let pattern = self.expand_glob_pattern_raw(pattern);
                strip_suffix_glob(&cur, &pattern, *longest)
            }
            VarOp::CaseConvert { pattern, upper, all } => {
                let pattern = self.expand_glob_pattern_raw(pattern);
                apply_case_convert(&cur, &pattern, *upper, *all)
            }
            VarOp::Substring { offset, length } => {
                // An omitted offset (`${x::3}`) is 0.
                let off = match offset.trim().is_empty() {
                    true => 0,
                    false => self.eval_arith(offset).unwrap_or(0),
                };
                let len = length.as_ref().and_then(|l| self.eval_arith(l).ok());
                substring_expand(&cur, off, len)
            }
            VarOp::Replace { pattern, repl, global, anchor } => {
                let pattern = self.expand_glob_pattern_raw(pattern);
                let repl = self.expand_replacement_operand(repl);
                glob_replace(&cur, &pattern, &repl, *global, *anchor)
            }
            VarOp::Transform(kind) => match kind {
                TransformKind::Attributes => {
                    // `@`/`*` reconstructs every element; a single index
                    // shows the array's `-a`/`-A` flag with only that
                    // element's value, as bash does.
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

    // `GLOBIGNORE`: glob results matching one of its colon-separated
    // patterns are dropped. Matched against the whole name with pathname
    // rules, so `*` doesn't cross `/`: `GLOBIGNORE='*.o'` drops `a.o` but not
    // `sub/x.o`, as in bash.
    fn apply_globignore(&mut self, matches: Vec<String>) -> Vec<String> {
        let ignore = self.lookup_var("GLOBIGNORE");
        if ignore.is_empty() {
            return matches;
        }
        let patterns: Vec<&str> = ignore.split(':').filter(|p| !p.is_empty()).collect();
        matches.into_iter().filter(|m| !patterns.iter().any(|p| crate::glob::matches_path(p, m))).collect()
    }

    // Read per expansion, not cached: `shopt -s` in a script applies to the
    // next word.
    fn glob_options(&self) -> glob::Options {
        glob::Options { dotglob: self.shopt_is_on("dotglob"), nocaseglob: self.shopt_is_on("nocaseglob"), globstar: self.shopt_is_on("globstar") }
    }

    // A pattern that matched nothing: kept as its own text (`echo *.nothing`
    // prints `*.nothing`) unless `nullglob` or `failglob` says otherwise.
    // `failglob` abandons the command, which `expansion_failed` makes fatal
    // in a script.
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

    // A simple command's words as argv, globbing each word that is
    // `Word::globbable` and contains metacharacters.
    fn expand_words(&mut self, words: &[Word]) -> Vec<String> {
        let mut out = Vec::new();
        // Once per command, not per field: each lookup walks the shopt
        // table. Nothing expanded here can change the answer, since `shopt`
        // in a `$( )` changes the child's table.
        let opts = self.glob_options();
        for w in words {
            if w.globbable {
                // Globbable means nothing quoted or expanded, so no
                // splitting.
                let s = self.expand_word(w);
                if !self.opt_noglob
                    && let Some(matches) = glob::expand(&s, opts, &self.cwd)
                {
                    let kept = self.apply_globignore(matches);
                    // Everything matched was ignored: the same as matching
                    // nothing.
                    if kept.is_empty() {
                        out.extend(self.unmatched_pattern(&s));
                    } else {
                        out.extend(kept);
                    }
                    continue;
                }
                out.push(s);
            } else {
                let fields = self.expand_word_split(w);
                if self.opt_noglob {
                    out.extend(fields.into_iter().map(|f| f.text));
                } else {
                    for field in fields {
                        match glob::expand(field.pattern(), opts, &self.cwd).map(|m| self.apply_globignore(m)) {
                            Some(matches) if !matches.is_empty() => out.extend(matches),
                            Some(_) => out.extend(self.unmatched_pattern(&field.text)),
                            None => out.push(field.text),
                        }
                    }
                }
            }
        }
        out
    }

    /// A global's value. `_` is kept outside the map (see the field);
    /// a copy inherited from the real environment may still be sitting
    /// in `globals` under that name, so the field wins when it is set.
    fn global_get(&self, name: &str) -> Option<&String> {
        match name == "_" && self.underscore.is_some() {
            true => self.underscore.as_ref(),
            false => self.globals.get(name),
        }
    }

    /// Writing one, `_` to its field.
    fn global_set(&mut self, name: &str, value: String) {
        match name == "_" {
            true => self.underscore = Some(value),
            false => {
                Rc::make_mut(&mut self.globals).insert(name.to_string(), value);
            }
        }
    }

    fn global_remove(&mut self, name: &str) {
        if name == "_" {
            self.underscore = None;
        }
        // An inherited `_` can still be in the map even once the field
        // has taken over, so both go.
        if self.globals.contains_key(name) {
            self.global_remove(name);
        }
    }

    /// Every global name, for the listings (`declare -p`, `set`,
    /// `compgen -v`). `_` is added only when the map does not already
    /// carry an inherited one, so it cannot appear twice.
    fn global_names(&self) -> impl Iterator<Item = &String> {
        let extra = match self.underscore.is_some() && !self.globals.contains_key("_") {
            true => Some(&*UNDERSCORE),
            false => None,
        };
        self.globals.keys().chain(extra)
    }

    /// The alias table, for writing. The only way to change it, so that
    /// nothing can alter it without the epoch noticing.
    fn aliases_mut(&mut self) -> &mut Vec<(String, String)> {
        self.alias_epoch += 1;
        &mut self.aliases
    }

    /// What the next statement's reading of the source depends on: the
    /// table, and whether it's consulted. Compared across a statement to
    /// decide whether the rest must be read again, since
    /// `shopt -s expand_aliases` changes what the next line means.
    fn alias_epoch(&self) -> (u64, bool) {
        (self.alias_epoch, self.shopt_is_on("expand_aliases"))
    }

    /// Point this shell's output at the descriptors a bare `exec` has just
    /// repointed.
    ///
    /// `exec > file` moves the process's descriptors, but builtins write to
    /// `self.sink` and spawned commands get `self.stdio_override`. Inside a
    /// `$( )` or a pane those are set, so `v=$(exec > f; echo hi)` would put
    /// `hi` in `v` instead of the file.
    ///
    /// The descriptor is duplicated rather than the override cleared: a spawn
    /// in a pane checks whether stdout is already taken before handing out
    /// the pane's pty. One `Capture` per stream, shared by sink and override,
    /// keeps builtin and external output in order.
    ///
    /// It lasts for this shell, and a subshell doing it doesn't affect its
    /// parent.
    fn follow_moved_descriptors(&mut self, moved: &[i32]) {
        let out = moved.contains(&1).then(|| dup_fd_as_file(1)).flatten().map(Capture::file);
        let err = moved.contains(&2).then(|| dup_fd_as_file(2)).flatten().map(Capture::file);
        if out.is_none() && err.is_none() && !moved.contains(&0) {
            return;
        }
        if out.is_some() || err.is_some() {
            let previous = std::mem::replace(&mut self.sink, OutputSink::Real);
            self.sink = OutputSink::Builtin {
                previous: Box::new(previous),
                stdout: match &out {
                    Some(c) => SinkStream::File(Rc::clone(c)),
                    None => SinkStream::OuterOut,
                },
                stderr: match &err {
                    Some(c) => SinkStream::File(Rc::clone(c)),
                    None => SinkStream::OuterErr,
                },
            };
        }
        // Rebuilt, not mutated: the override is shared with whoever handed
        // it down.
        let kept = self.stdio_override.as_ref().map(|o| {
            let o = o.borrow();
            (o.stdin.clone(), o.stdout.clone(), o.stderr.as_ref().and_then(|f| f.try_clone().ok()))
        });
        let (stdin, stdout, stderr) = kept.unwrap_or((None, None, None));
        self.stdio_override = Some(Rc::new(RefCell::new(StdioOverride {
            stdin: if moved.contains(&0) { None } else { stdin },
            stdout: out.or(stdout),
            stderr: err.and_then(|c| c.descriptor()).or(stderr),
        })));
    }

    /// Called by whatever is about to move the process's working
    /// directory, umask or fds 0/1/2, so that the construct this shell
    /// is running under can put them back. See ProcessRestore.
    fn note_cwd_change(&mut self) {
        if self.process_restore.cwd.is_none() {
            self.process_restore.cwd = std::env::current_dir().ok();
        }
    }

    pub(crate) fn note_umask_change(&mut self) {
        if self.process_restore.umask.is_none() {
            self.process_restore.umask = Some(current_umask());
        }
    }

    fn note_fd012_change(&mut self) {
        if self.process_restore.fd012.is_none() {
            self.process_restore.fd012 = Some(save_fd012());
        }
    }

    // A name's own value, bypassing nameref redirection: a nameref's target
    // name, and each link while `resolve_nameref` follows a chain.
    pub(crate) fn raw_var_lookup(&self, name: &str) -> String {
        for scope in self.var_scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return v.clone().unwrap_or_default();
            }
        }
        if let Some(v) = self.global_get(name) {
            return v.clone();
        }
        // The real environment is consulted last, for names set outside this
        // table (the session bridge's `XDG_RUNTIME_DIR`, `TZ` in a test).
        self.inherited_var(name).unwrap_or_default()
    }

    // A name's own value, bypassing nameref redirection: sets a nameref's
    // target name itself.
    pub(crate) fn raw_var_write(&mut self, name: &str, value: String) {
        for scope in self.var_scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), Some(value.clone()));
                self.export_to_environment(name, &value);
                return;
            }
        }
        self.global_set(name, value.clone());
        self.export_to_environment(name, &value);
    }

    /// Does nothing. A child's environment is built from the exported names
    /// (`Shell::command`), and writing the real environment would leak
    /// variables between shells sharing the process.
    fn export_to_environment(&self, _name: &str, _value: &str) {}

    // Removes a name from the innermost scope holding it, or the globals,
    // and hides any inherited copy. Shared by `unset` and the restore after
    // `FOO=bar cmd`.
    pub(crate) fn remove_var(&mut self, name: &str) {
        self.unset_names.insert(name.to_string());
        for scope in self.var_scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                // The shadow stays, unset: removing it would uncover the
                // caller's value, but bash leaves an unset local unset for
                // the rest of the function.
                *slot = None;
                return;
            }
        }
        Rc::make_mut(&mut self.globals).remove(name);
    }

    // The real environment's value, unless this shell unset the name.
    fn inherited_var(&self, name: &str) -> Option<String> {
        if self.unset_names.contains(name) {
            return None;
        }
        std::env::var(name).ok()
    }

    // Follows a `declare -n` chain to its final target, capped so a cycle
    // stops instead of looping. bash reports cycles as errors.
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
            // Joined on IFS's first character: `IFS=-; echo "$*"` is `a-b-c`.
            // Unquoted, the result is split on IFS again.
            "@" | "*" => {
                let sep = self.ifs_join_char();
                self.arg_frames.last().map(|a| a.join(&sep)).unwrap_or_default()
            }
            // Computed live, never stored. `SECONDS=n` is the one that can be
            // assigned; see `assign_var`.
            "$" => self.shell_pid.to_string(),
            "!" => self.jobs.borrow().last_bg_pid.map(|p| p.to_string()).unwrap_or_default(),
            // These read the RNG or the clock, so they count as effects:
            // otherwise `check_nonproductive_recursion` would take
            // `f() { f $SECONDS; }` for a fixed point.
            "RANDOM" => {
                self.effects += 1;
                self.next_random().to_string()
            }
            "LINENO" => self.current_line.to_string(),
            // Seconds since the epoch, and with microseconds.
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
            // Every option that is on, by letter: lowercase, then
            // uppercase, then the invocation letter, as bash orders them
            // (`abefhmuvxBCETc`). `h` and `B` are always on: this shell
            // hashes PATH lookups and expands braces, with no way to stop
            // either.
            "-" => {
                let mut letters: Vec<char> = vec!['h', 'B'];
                for (on, c) in [
                    (self.opt_errexit, 'e'),
                    (self.opt_noglob, 'f'),
                    (self.opt_noexec, 'n'),
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
                crate::sort::sort_by_key(&mut letters, |c| (c.is_ascii_uppercase(), *c));
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
                if let Some(v) = self.global_get(name) {
                    return v.clone();
                }
                // A bare `$a` on an array is `${a[0]}`, which is also why
                // `$FUNCNAME` names the running function. After scalars, so
                // a scalar of the same name wins.
                if let Some(first) = self.arrays.get(name).and_then(|m| m.values().next()) {
                    return first.clone();
                }
                // Indexed only: bare `$m` on an associative array is empty.
                // The real environment last, as in `raw_var_lookup`.
                if let Some(v) = self.inherited_var(name) {
                    return v;
                }
                // Variables bash sets at startup, computed on demand here. An
                // assignment (checked above) overrides them.
                match name {
                    "BASH_VERSION" => BASH_VERSION.to_string(),
                    "PPID" => crate::platform::parent_process_id().to_string(),
                    "UID" => crate::platform::real_user().to_string(),
                    "EUID" => crate::platform::effective_user().to_string(),
                    "HOSTNAME" => get_hostname(),
                    // The path this shell was started from; `$0` is what it
                    // was called as.
                    "BASH" => std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                    // This process, the same as `$$`: a subshell here isn't
                    // a process (see `run_in_child_shell`).
                    "BASHPID" => crate::platform::process_id().to_string(),
                    "BASH_SUBSHELL" => self.subshell_depth.to_string(),
                    "BASH_COMMAND" => self.bash_command.to_string(),
                    // The options that are on, colon-separated and sorted,
                    // for `[[ $SHELLOPTS == *errexit* ]]`.
                    "SHELLOPTS" => {
                        let mut on: Vec<&str> = SET_O_OPTIONS.iter().copied().filter(|n| self.shell_option_enabled(n) == Some(true)).collect();
                        crate::sort::sort(&mut on);
                        on.join(":")
                    }
                    "BASHOPTS" => {
                        let mut on: Vec<String> = KNOWN_SHOPT_OPTIONS.iter().map(|(n, _)| n.to_string()).filter(|n| self.shopt_is_on(n)).collect();
                        crate::sort::sort(&mut on);
                        on.join(":")
                    }
                    _ => String::new(),
                }
            }
        }
    }

    // Variable lookup for the debugger's hover and `:dbg print`. `&self`,
    // unlike `lookup_var`, so magic and positional names (`$RANDOM`, `$1`)
    // are `None` along with unset ones: reading them has side effects.
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
        self.global_get(name).cloned().or_else(|| self.inherited_var(name))
    }

    // Public so debugger.rs can install itself before `run_program`.
    pub fn set_debug_hook(&mut self, hook: Option<Rc<RefCell<dyn DebugHook>>>) {
        self.debug_hook = hook;
    }

    // Called by repl.rs before running a session other than the one that
    // ran last: applies this session's cwd, environment and umask to the
    // process, so relative-path file I/O and `source`, which use the
    // process cwd, see this session's.
    pub fn sync_real_state_in(&self) {
        let _ = std::env::set_current_dir(&self.cwd);
        let current: std::collections::HashSet<String> = std::env::vars().map(|(k, _)| k).collect();
        for k in &current {
            if !self.env_snapshot.contains_key(k) {
                unsafe { std::env::remove_var(k) };
            }
        }
        for (k, v) in self.env_snapshot.iter() {
            unsafe { std::env::set_var(k, v) };
        }
        crate::platform::set_umask(self.umask_snapshot);
    }

    // The other half of `sync_real_state_in`: after this session runs,
    // saves the process environment and umask for its next turn. `self.cwd`
    // is already kept current by `run_cd`.
    pub fn sync_real_state_out(&mut self) {
        self.env_snapshot = Rc::new(std::env::vars().collect());
        self.umask_snapshot = current_umask();
    }

    // Sets this session's `TERM`/`COLORTERM`, the way to change its
    // environment from outside: a plain `set_var` would be undone by
    // `sync_real_state_in` reapplying the snapshot on the next run. Empty
    // `term` leaves `TERM` alone (the client just didn't have it); empty
    // `colorterm` unsets it, so reattaching from a truecolor terminal to a
    // plain one drops it.
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
        // And into the variables, which is what a spawned command reads (see
        // `Shell::command`).
        if !term.is_empty() {
            self.assign_var("TERM", term.to_string());
            Rc::make_mut(&mut self.exported_names).insert("TERM".to_string());
        }
        if colorterm.is_empty() {
            self.global_remove("COLORTERM");
        } else {
            self.assign_var("COLORTERM", colorterm.to_string());
            Rc::make_mut(&mut self.exported_names).insert("COLORTERM".to_string());
        }
    }

    // `command -v`/`-V`: what `name` resolves to (function, builtin or PATH
    // executable), without running it. Prints nothing and returns 1 when
    // nothing matches, which makes `command -v foo >/dev/null` a test.
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

    // `$RANDOM`: xorshift64, 0 to 32767 as in bash.
    fn next_random(&mut self) -> u32 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        ((x >> 33) % 32768) as u32
    }

    // A script stops on a failed expansion, as in bash; an interactive
    // session only fails the command.
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

    // `${x!y}`, `${1bad}`, `${a[}`: brace content that isn't a parameter
    // name. The lexer passes it as a name; reading it as unset would turn a
    // typo into an empty string, so it's reported and fails, as in bash.
    fn check_param_name(&mut self, name: &str) {
        if is_parameter_name(name) {
            return;
        }
        sh_eprintln!(self, "bish: ${{{}}}: bad substitution", name);
        self.expansion_failed = true;
    }

    // `set -u`, for a bare `$VAR`: the operators (`${VAR:-x}`, `${VAR?}`)
    // exist to test for unset, so `eval_var_op` doesn't call this.
    fn check_nounset(&mut self, name: &str) {
        if !self.opt_nounset {
            return;
        }
        if self.var_is_set(name) {
            return;
        }
        // bash shows a positional with its `$`, a variable without.
        let shown = match name.chars().all(|c| c.is_ascii_digit()) {
            true => format!("${}", name),
            false => name.to_string(),
        };
        sh_eprintln!(self, "bish: {}: unbound variable", shown);
        // 127, as in bash 5.3.
        self.pending_exit = Some(127);
    }

    // Whether `name` has been assigned, as opposed to being empty: `${V-x}`
    // asks this, `${V:-x}` asks about either.
    pub(crate) fn var_is_set(&self, name: &str) -> bool {
        let resolved;
        let name = if self.nameref_names.contains(name) {
            resolved = self.resolve_nameref(name);
            resolved.as_str()
        } else {
            name
        };
        // Special parameters and the computed variables (`RANDOM`, `SECONDS`,
        // `PPID`, ...) are always set, even under `set -u`, as in bash.
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
        // A positional is set only when there is an argument at that
        // position: `$1` with no arguments is what `set -u` is for. (`$0`
        // is above.)
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) {
            let index: usize = name.parse().unwrap_or(0);
            return index <= self.arg_frames.last().map(Vec::len).unwrap_or(0);
        }
        // The innermost scope holding the name decides: a valueless `local x`
        // shadows as unset.
        for scope in self.var_scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return v.is_some();
            }
        }
        self.global_get(name).is_some() || self.inherited_var(name).is_some()
    }

    // Assigns to the innermost local of that name, else the global: bash
    // functions don't localize. False when the name is readonly and the
    // write was refused, after the message.
    pub(crate) fn assign_var(&mut self, name: &str, value: String) -> bool {
        self.assign_var_impl(name, value, false)
    }

    // What `name+=v` assigns: concatenation, or addition with the integer
    // attribute (`declare -i n=3; n+=2` is 5). Every scalar `+=` goes
    // through here, prefix assignments included.
    pub(crate) fn appended_value(&mut self, name: &str, v: &str) -> String {
        if self.integer_names.contains(name) {
            let current = self.lookup_var(name);
            let a = arith::eval(&current, self).unwrap_or(0);
            let b = arith::eval(v, self).unwrap_or(0);
            return (a + b).to_string();
        }
        self.lookup_var(name) + v
    }

    /// `export NAME=value` without going through the parser.
    pub fn export_var(&mut self, name: &str, value: String) {
        Rc::make_mut(&mut self.exported_names).insert(name.to_string());
        self.assign_var(name, value.clone());
        self.export_to_environment(name, &value);
    }

    // `declare -g`/`local -g`: `assign_var`, but always to the global,
    // past any local of the same name.
    pub(crate) fn assign_var_global(&mut self, name: &str, value: String) -> bool {
        self.assign_var_impl(name, value, true)
    }

    fn assign_var_impl(&mut self, name: &str, value: String, force_global: bool) -> bool {
        // Assigning OPTIND restarts `getopts`, even with the same value: bash
        // resets on the assignment, so `OPTIND=1` restarts a scan halfway
        // through `-ab`. `getopts` records its position after assigning. See
        // `run_getopts`.
        if name == "OPTIND" {
            self.getopts_offset = 0;
        }
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
        // `SECONDS=n` restarts the count from n: `lookup_var` computes it, so
        // this records an offset.
        if name == "SECONDS" {
            if let Ok(n) = value.trim().parse::<i64>() {
                self.seconds_offset = n - self.shell_start.elapsed().as_secs() as i64;
            }
            return true;
        }
        // `RANDOM=n` reseeds the generator, as in bash.
        if name == "RANDOM" {
            if let Ok(n) = value.trim().parse::<u64>() {
                self.rng_state = if n == 0 { 0x2545F4914F6CDD1D } else { n };
            }
            return true;
        }
        // `declare -i`: the value is evaluated as arithmetic (`n="2+3"`
        // stores 5).
        let value = if self.integer_names.contains(name) { arith::eval(&value, self).unwrap_or(0).to_string() } else { value };
        // `declare -u`/`-l`: case-fold on every assignment.
        let value = if self.upper_names.contains(name) {
            value.to_uppercase()
        } else if self.lower_names.contains(name) {
            value.to_lowercase()
        } else {
            value
        };
        // `declare -a b; b=x` writes element 0 rather than a scalar shadowing
        // the array, whether or not the array holds anything yet.
        if self.assoc_names.contains(name) {
            self.assoc_arrays.entry(name.to_string()).or_default().insert("0".to_string(), value);
            return true;
        }
        if self.array_names.contains(name) || self.arrays.contains_key(name) {
            self.arrays.entry(name.to_string()).or_default().insert(0, value);
            return true;
        }
        if force_global {
            // Past any local shadow, which `raw_var_write` would update
            // instead.
            self.global_set(name, value.clone());
            self.export_to_environment(name, &value);
            return true;
        }
        self.raw_var_write(name, value);
        true
    }

    fn resolve_redirects(&mut self, cmd: &SimpleCommand) -> Result<ResolvedRedirs, String> {
        self.resolve_redirect_list(&cmd.redirects)
    }

    // Points the sink at a builtin's `>`, `2>`, `&>`, `1>&2`, `2>&1` for
    // one dispatch; builtins write to no other fd. `Ok(false)` when nothing
    // redirects fd 1 or 2 and the sink is untouched; after `Ok(true)` the
    // caller must call `pop_builtin_output_sink`. `Err` is a failed open.
    fn push_builtin_output_sink(&mut self, redirects: &[Redirect], who: &str) -> Result<bool, String> {
        // The redirect list, simulated left to right. Order matters, and so
        // does when a destination is captured:
        //
        //     echo x 3>&1 >&2 2>&3 3>&-     the stdout/stderr swap
        //
        // `2>&3` copies whatever fd 3 names then, and the later `3>&-` must
        // not touch fd 2. In `>file 2>&1` both streams must share one open
        // file, or they overwrite each other.
        //
        // So each destination has an id: a redirect that opens one appends
        // to `dests`, a dup copies the id, and rebinding an fd makes a new
        // id. Two descriptors share exactly when they hold the same id.
        // Only fds 1 and 2 reach the builtin, but every fd is tracked since
        // 1 and 2 can be defined in terms of others.
        enum Dest {
            // The process's fd N as this command found it:
            // `echo e >&2 2>/dev/null` prints on the terminal.
            ProcessFd(i32),
            // Opened as the redirect is read: a redirect creates and
            // truncates its file even if nothing writes to it
            // (`echo x > a > b` creates both, `echo x 3>f` creates `f`),
            // and a failure to open any of them is reported.
            File(Rc<Capture>),
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
        // The id fd `n` holds now, made for the process's own descriptor if
        // this command hasn't touched it.
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
                    define!(1, Dest::File(Capture::file(self.open_out(&p, *append, *clobber)?)))
                }
                Redirect::Err { word, append, clobber } | Redirect::FdOut { fd: 2, word, append, clobber } => {
                    let p = self.expand_word(word);
                    define!(2, Dest::File(Capture::file(self.open_out(&p, *append, *clobber)?)))
                }
                Redirect::FdOut { fd, word, append, clobber } => {
                    let p = self.expand_word(word);
                    define!(*fd as i32, Dest::File(Capture::file(self.open_out(&p, *append, *clobber)?)))
                }
                // `&>file`: both descriptors, one open file.
                Redirect::Both { word, append } => {
                    let p = self.expand_word(word);
                    define!(1, Dest::File(Capture::file(self.open_out(&p, *append, false)?)));
                    let id = table[&1];
                    table.insert(2, id);
                }
                Redirect::DupErrToOut => {
                    let id = id_of!(1);
                    table.insert(2, id);
                }
                Redirect::FdDup { fd, target, .. } => {
                    let id = id_of!(*target as i32);
                    table.insert(*fd as i32, id);
                }
                // `>&$var`, how a script writes to a coproc's descriptor.
                Redirect::FdDupWord { fd, word, .. } => {
                    let text = self.expand_word(word);
                    let Ok(target) = text.trim().parse::<i32>() else {
                        return Err(format!("{}: ambiguous redirect", text));
                    };
                    let id = id_of!(target);
                    table.insert(*fd as i32, id);
                }
                Redirect::FdClose { fd } => define!(*fd as i32, Dest::Closed),
                // An input redirect fails the command if it can't be
                // opened, even for a builtin that won't read it: bash
                // performs redirects in order and stops at the first
                // failure, so `echo z <nosuch >out` neither prints nor
                // creates `out`. Opened and dropped; `read` and `mapfile`
                // open their own (`read_input_source`).
                Redirect::In(word) | Redirect::FdIn { word, .. } => {
                    let p = self.expand_word(word);
                    if opening_would_consume_it(&p) {
                        continue;
                    }
                    self.open_in(&p)?;
                }
                Redirect::InOut(word) | Redirect::FdInOut { word, .. } => {
                    let p = self.expand_word(word);
                    if opening_would_consume_it(&p) {
                        continue;
                    }
                    self.open_in_out(&p)?;
                }
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
            // One destination for both, opened once, so they share a write
            // position.
            Some(_) if shared => stdout.clone(),
            Some(d) => open_dest(d, who)?,
        };
        let previous = std::mem::replace(&mut self.sink, OutputSink::Real);
        self.sink = OutputSink::Builtin { previous: Box::new(previous), stdout, stderr };
        return Ok(true);

        fn open_dest(dest: &Dest, who: &str) -> Result<SinkStream, String> {
            match dest {
                Dest::File(f) => Ok(SinkStream::File(Rc::clone(f))),
                // For a builtin, fds 1 and 2 are the enclosing sink (a
                // capture, a grid, an outer redirect), so they resolve to
                // whatever the parent sink does. Any other descriptor is
                // real and is duped.
                Dest::ProcessFd(1) => Ok(SinkStream::OuterOut),
                Dest::ProcessFd(2) => Ok(SinkStream::OuterErr),
                // A dup of a closed descriptor is an error: `echo x >&9`
                // reports and fails, as for an external command.
                Dest::ProcessFd(fd) => match dup_existing_fd(*fd) {
                    Some(f) => Ok(SinkStream::File(Capture::file(f))),
                    None => Err(format!("{}: Bad file descriptor", fd)),
                },
                Dest::Closed => Err(format!("{}: write error: Bad file descriptor", who)),
            }
        }
    }

    // Restores what `push_builtin_output_sink` saved; a no-op if the sink
    // isn't a builtin one, so it can be called unconditionally.
    fn pop_builtin_output_sink(&mut self) {
        if let OutputSink::Builtin { previous, .. } = &mut self.sink {
            self.sink = *std::mem::replace(previous, Box::new(OutputSink::Real));
        }
    }

    // Errors for a command that never started ("not found") still go
    // through its `2>`, as in bash, or to the shell's stderr without one.
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

    /// A path as this shell sees it: relative to the shell's cwd, not the
    /// process's. They differ when two shells share the process and one runs
    /// `cd`, as an in-process pipeline stage does.
    pub(crate) fn resolve_path(&self, path: &str) -> std::path::PathBuf {
        let given = std::path::Path::new(path);
        if given.is_absolute() { given.to_path_buf() } else { self.cwd.join(given) }
    }

    // Every redirect that opens a file for writing comes here, so the
    // restricted-mode check covers them all.
    fn open_out(&self, path: &str, append: bool, clobber: bool) -> Result<std::fs::File, String> {
        if self.opt_restricted {
            return Err(format!("{}: restricted: cannot redirect output", path));
        }
        if let Some(result) = dev_socket_file(path) {
            return result;
        }
        // `set -C` refuses only a truncating `>` over an existing regular
        // file; `> /dev/null` and `> fifo` stay legal. Stat, then open, as
        // bash does: `O_EXCL` would refuse `/dev/null` too.
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

    // Every redirect that opens a file for reading (`<`, `N<`) comes here.
    // `/dev/tcp/HOST/PORT` and `/dev/udp/...` work in any direction (see
    // `dev_socket_file`). Reading is allowed in restricted mode.
    fn open_in(&self, path: &str) -> Result<std::fs::File, String> {
        if let Some(result) = dev_socket_file(path) {
            return result;
        }
        std::fs::File::open(self.resolve_path(path)).map_err(|e| format!("{}: {}", path, os_message(&e)))
    }

    // `<>`, open for both. `/dev/tcp` and `/dev/udp` are recognized too;
    // `<>` is how they're normally used, one connection for request and
    // response.
    fn open_in_out(&self, path: &str) -> Result<std::fs::File, String> {
        // `<>` writes, so a restricted shell refuses it as `open_out` does.
        if self.opt_restricted {
            return Err(format!("{}: restricted: cannot redirect output", path));
        }
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

    // Whether a write to this name will be refused: `readonly`'s list
    // plus restricted mode's. For a caller that needs to know without
    // printing a second message.
    pub(crate) fn name_is_readonly(&self, name: &str) -> bool {
        self.readonly_names.contains(name) || self.is_restricted_readonly_name(name)
    }

    // Restricted mode's SHELL/PATH/ENV/BASH_ENV, refused with the same
    // errors as a readonly name but kept out of `readonly_names`, which
    // would show them as `-r` in `declare -p` and keep them past restricted
    // mode.
    pub(crate) fn is_restricted_readonly_name(&self, name: &str) -> bool {
        self.opt_restricted && matches!(name, "SHELL" | "PATH" | "ENV" | "BASH_ENV")
    }

    /// `check_restricted_command_name`'s rule, for the program names that
    /// reach `Command::new` outside dispatch: a language server from
    /// `::bish lsp add` and a debug adapter from `:dbg launch --adapter=`.
    /// They're spawned where no `Shell` is in reach, so the name is checked
    /// where it's given. A bare name is allowed, as for a command.
    pub(crate) fn restricted_program_refused(&mut self, what: &str, name: &str) -> bool {
        if self.opt_restricted && name.contains('/') {
            sh_eprintln!(self, "bish: {}: {}: restricted: cannot specify `/' in command names", what, name);
            return true;
        }
        false
    }

    // Restricted mode refuses a command named by path, as a command word
    // or through `command NAME`. Prints bash's error and returns true when
    // refused.
    fn check_restricted_command_name(&mut self, name: &str) -> bool {
        if self.opt_restricted && name.contains('/') {
            sh_eprintln!(self, "bish: {}: restricted: cannot specify `/' in command names", name);
            true
        } else {
            false
        }
    }

    // Whether every redirect is one `resolve_simple_redirects_for_compound`
    // handles. Checked before resolving, so a target with a side effect
    // (`{ ...; } > "$(cmd)"`) isn't expanded twice by a fallback.
    fn compound_redirects_are_simple(redirects: &[Redirect]) -> bool {
        redirects.iter().all(|r| {
            matches!(
                r,
                Redirect::In(_)
                    | Redirect::HereString(_)
                    | Redirect::HereDoc(_, _)
                    | Redirect::Out { .. }
                    | Redirect::Err { .. }
                    | Redirect::Both { .. }
                    | Redirect::DupErrToOut
                    | Redirect::FdOut { fd: 1, .. }
                    | Redirect::FdOut { fd: 2, .. }
                    | Redirect::FdDup { fd: 2, target: 1, .. }
                    | Redirect::FdDup { fd: 1, target: 2, .. }
            )
        })
    }

    // Resolves a compound's plain fd 0/1/2 redirects to files, for
    // `run_compound_redirected`'s in-process path. A stream on the other's
    // destination follows it instead of opening the path again, so they
    // share a write position.
    fn resolve_simple_redirects_for_compound(&mut self, redirects: &[Redirect]) -> Result<ChildStdio, String> {
        let mut stdio = ChildStdio::default();
        // Path, append, clobber (`>|`), as `open_out` takes them.
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
                Redirect::HereDoc(w, _) => {
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
                Redirect::DupErrToOut | Redirect::FdDup { fd: 2, target: 1, .. } => {
                    stdio.err_follows_out = Some(if stdout_target.is_some() { Follows::OwnFile } else { Follows::Outer });
                }
                Redirect::FdDup { fd: 1, target: 2, .. } => {
                    stdio.out_follows_err = Some(if stderr_target.is_some() { Follows::OwnFile } else { Follows::Outer });
                }
                _ => unreachable!("compound_redirects_are_simple already filtered these out"),
            }
        }
        if let Some((p, append, clobber)) = &stdout_target {
            stdio.stdout = Some(Capture::file(self.open_out(p, *append, *clobber)?));
        }
        // A following stream opens nothing; a `2>file` before the dup was
        // superseded.
        if stdio.err_follows_out.is_none() {
            if let Some((p, append, clobber)) = &stderr_target {
                stdio.stderr = Some(self.open_out(p, *append, *clobber)?);
            }
        }
        Ok(stdio)
    }

    /// Every redirect, in source order, as actions on the descriptors the
    /// child inherits. A redirect names a descriptor as it stands at that
    /// point, so `3>&1 1>&2 2>&3` swaps the streams, and every file named is
    /// opened even if superseded (`> a > b` creates both). They run in the
    /// child (a `pre_exec` hook, or this process for `exec`).
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
                Redirect::HereDoc(w, _) => {
                    // The body already ends in a newline.
                    let content = self.expand_word(w);
                    actions.push(FdAction::Open { fd: 0, file: here_string_file(&content)? });
                }
                Redirect::FdHereString { fd, word } => {
                    let mut content = self.expand_word(word);
                    content.push('\n');
                    actions.push(FdAction::Open { fd: *fd as i32, file: here_string_file(&content)? });
                }
                Redirect::FdHereDoc { fd, body, .. } => {
                    let content = self.expand_word(body);
                    actions.push(FdAction::Open { fd: *fd as i32, file: here_string_file(&content)? });
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
                // `&>file`: both descriptors on one open file.
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
                // `{name}>file`: the descriptor is chosen here, where the
                // variable is set; the child only sees the number.
                Redirect::VarFd { var, kind, word } => {
                    let p = self.expand_word(word);
                    if *kind == crate::lexer::VarFdKind::Dup {
                        // `{v}>&-` closes the descriptor `v` names;
                        // `{v}>&N` names a new dup of N.
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
                Redirect::FdDup { fd, target, .. } => {
                    actions.push(FdAction::Dup { fd: *fd as i32, source: *target as i32 });
                }
                Redirect::FdDupWord { fd, word, .. } => {
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
        // A name whose value is an expression is evaluated: `x=y; y=2;
        // echo $((x))` is 2. Recursion is capped by the evaluator's depth
        // limit.
        match text.trim().parse::<i64>() {
            Ok(n) => n,
            Err(_) if text.trim().is_empty() => 0,
            Err(_) => arith::eval(text.trim(), self).unwrap_or(0),
        }
    }

    fn set(&mut self, name: &str, value: i64) {
        // Counted here, not at `(( ))`, because `let` and `$(( x = 1 ))`
        // assign too: `f() { ((i++)); f; }` makes progress and mustn't be
        // called a fixed point.
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

// `a[1]` -> ("a", "1"). The subscript stays text (an expression or a
// key) for `array_element`/`array_set_index` to resolve.
fn split_subscript(name: &str) -> Option<(&str, String)> {
    let (base, rest) = name.split_once('[')?;
    let index = rest.strip_suffix(']')?;
    Some((base, index.to_string()))
}

struct ResolvedRedirs {
    /// The whole redirect list in source order (see `resolve_redirect_list`),
    /// run in the child on top of what it inherits: a pipe, a capture, the
    /// shell's own stdio.
    actions: Vec<FdAction>,
}

/// Moves every file the list opened above the descriptors it rebinds.
///
/// The files are opened in the shell, at whatever numbers were free, and
/// the list may dup over them: in `3>&1 1>&2 2>&3 3>&- 2>/dev/null`,
/// /dev/null opened on fd 3 would be overwritten by `3>&1` before
/// `2>/dev/null` installs it. `F_DUPFD_CLOEXEC` picks the lowest free fd
/// above a floor; close-on-exec is fine since the copy is only ever a
/// dup2 source.
fn move_opened_files_out_of_the_way(actions: &mut [FdAction]) {
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
        let raw = std::os::fd::AsRawFd::as_raw_fd(file);
        if raw >= floor {
            continue;
        }
        if let Some(moved) = crate::platform::duplicate_above(raw, floor) {
            // SAFETY: a fresh descriptor owned by nothing else. The old
            // `file` is closed by this assignment.
            *file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(moved) };
        }
    }
}

/// One redirect step on the descriptors a child has inherited: point
/// `fd` at an open file, at another descriptor as it stands now, or
/// close it.
enum FdAction {
    Open { fd: i32, file: std::fs::File },
    Dup { fd: i32, source: i32 },
    Close(i32),
}

impl FdAction {
    fn target(&self) -> i32 {
        match self {
            FdAction::Open { fd, .. } | FdAction::Dup { fd, .. } | FdAction::Close(fd) => *fd,
        }
    }
}

// Associative-array storage (`declare -A`), in insertion order. bash
// iterates in its hash table's bucket order, which isn't worth
// reproducing and which no script can predict anyway.
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

// A coproc pipe end the shell keeps open. The read side keeps one
// `BufReader` across `read -u` calls: a fresh one would discard what it
// read ahead past the first line.
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

// A background job (`cmd &`): one child, or a pipeline's. The children
// are kept so `wait`, `jobs` and `fg` can poll or block on them.
pub(crate) struct Job {
    pub(crate) id: u32,
    pub(crate) pids: Vec<u32>,
    children: Vec<std::process::Child>,
    pub(crate) cmd_text: String,
    // The pty this job's stdio goes to, for a job started while the
    // session was promoted. Streams the job redirected still go where they
    // were told. `None` when the session wasn't promoted: there's no grid,
    // and inherited stdio already reaches the terminal.
    //
    // `fg` (`run_fg`) renders it into the session's grid, and
    // `drain_background_output` keeps it flowing into `sink_screen` while
    // the job stays in the background.
    pub(crate) pty_master: Option<std::fs::File>,
    // The grid this job's output belongs in, captured at spawn (see
    // `push_job_full`). Kept on the job because the job table is shared by
    // every session, so later there is no current session to ask. `None`
    // when the session wasn't promoted.
    sink_screen: Option<Rc<RefCell<crate::vt100::Screen>>>,
    // Set once the pty master is non-blocking, done on the first drain so
    // spawn sites don't have to.
    nonblocking: bool,
    // The job's process group, with monitor mode on: a single command from
    // `run_single`'s plain path, or a background pipeline whose stages all
    // join the first one's group (so `kill %N` and `bg` reach all of them).
    // `run_fg` and `run_bg` give it the terminal and SIGCONT it. A pty job
    // started through `spawn_attached` doesn't need one: `setsid` already
    // isolates it, and its pty generates SIGTSTP and SIGINT.
    pub(crate) pgid: Option<u32>,
    // Seen stopped by a `WUNTRACED` wait (`waitpid_untraced`). `jobs` and
    // `wait` check it first: `Job::poll`/`wait` can't see a stop, since
    // `Child::try_wait` doesn't pass `WUNTRACED`. Cleared when `fg` or `bg`
    // resumes it.
    pub(crate) stopped: bool,
}

// `Shell::jobs`; see that field for why it's shared.
pub(crate) struct JobTable {
    pub(crate) jobs: Vec<Job>,
    next_job_id: u32,
    // `$!`: the last backgrounded command (a pipeline's last stage, as in
    // bash). Kept separately from the table so it survives the job being
    // reaped.
    last_bg_pid: Option<u32>,
}

impl JobTable {
    fn new() -> Self {
        JobTable { jobs: Vec::new(), next_job_id: 1, last_bg_pid: None }
    }
}

impl Job {
    // Reaps whichever children have exited, without blocking. The job's
    // status once all have, the last child's, as for a pipeline. (bash
    // calls a pipeline job done when its last stage exits.)
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

    /// Waits for this job, emptying its pty into its screen as it goes.
    /// `wait` takes the job out of the table before blocking, so
    /// `drain_background_output` can't see it, and repl.rs's idle tick isn't
    /// running meanwhile: without this, `f() { echo a; }; f & wait` printed
    /// nothing in a pane.
    pub(crate) fn wait_draining(&mut self) -> i32 {
        if self.pty_master.is_none() || self.sink_screen.is_none() {
            return self.wait();
        }
        loop {
            self.drain_into_sink();
            match self.children.last_mut().map(std::process::Child::try_wait) {
                Some(Ok(None)) => std::thread::sleep(std::time::Duration::from_millis(1)),
                _ => break,
            }
        }
        // Whatever arrived between the last read and the exit.
        self.drain_into_sink();
        self.wait()
    }

    /// This job's pending pty output into its grid, for a job no longer in
    /// the table (`drain_background_output` walks the table).
    fn drain_into_sink(&mut self) {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let (Some(master), Some(screen)) = (&mut self.pty_master, &self.sink_screen) else {
            return;
        };
        // An editor frame owns the screen while it's up; see
        // `drain_background_output`.
        let Ok(mut screen) = screen.try_borrow_mut() else { return };
        if screen.using_alternate {
            return;
        }
        if !self.nonblocking {
            pty::set_nonblocking(master.as_raw_fd());
            self.nonblocking = true;
        }
        let mut buf = [0u8; 4096];
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => screen.feed(&buf[..n]),
                Err(_) => break,
            }
        }
    }

    // Blocks until every child has exited; the last one's status.
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

// A pty-attached job handed to repl.rs by `Shell::take_pending_fg` once
// `run_fg` returns `ExecResult::Fg`. repl.rs owns the session and grid
// state a redraw needs and keeps polling the job across main-loop
// iterations. Wrapping `Job` keeps its fields private.
pub struct FgJob(Job);

// How `FgJob::poll_untraced` found the job: the public counterpart of
// `JobWaitOutcome`.
pub enum FgWait {
    Running,
    Exited(i32),
    Stopped,
}

impl FgJob {
    pub fn wait(&mut self) -> i32 {
        self.0.wait()
    }

    // `run_fg` only makes an `FgJob` from a job with a pty.
    pub fn pty_master(&mut self) -> &mut std::fs::File {
        self.0.pty_master.as_mut().expect("FgJob always has a pty_master")
    }

    // Stops the job with SIGSTOP. A Ctrl-Z byte through the pty doesn't
    // work: `spawn_attached` makes the job a session leader (`setsid`, so
    // the pty can be its controlling terminal), which orphans its process
    // group, and the kernel discards a default-disposition SIGTSTP sent to
    // an orphaned group from any source. SIGINT isn't affected, which is
    // why Ctrl-C works. The cost is that a program with its own SIGTSTP
    // handler (vim, less) doesn't get to restore the terminal first, but
    // every pty job is orphaned this way, so it could never get one.
    pub fn send_stop(&mut self) {
        if let Some(&pid) = self.0.pids.first() {
            send_signal_to_pgrp(pid, SIGSTOP);
        }
    }

    // Non-blocking and `WUNTRACED`-aware: unlike `Job::poll`, this tells
    // `drive_fg_job` that the job stopped (Ctrl-Z, `kill -STOP`).
    pub fn poll_untraced(&mut self) -> FgWait {
        match crate::platform::wait_for_child(self.0.pids[0] as i32, false) {
            crate::platform::Waited::Running => FgWait::Running,
            crate::platform::Waited::Stopped(_) => FgWait::Stopped,
            crate::platform::Waited::Killed(signum) => FgWait::Exited(128 + signum),
            crate::platform::Waited::Exited(code) => FgWait::Exited(code),
            crate::platform::Waited::Gone => FgWait::Exited(1),
        }
    }
}

// `echo -e`'s escapes. The bool is `\c`, which stops the output and the
// caller. Built as bytes and decoded at the end: `\0303\0244` is the
// two bytes of `ä`, not two Latin-1 characters.
pub(crate) fn echo_expand_escapes(s: &str) -> (String, bool) {
    backslash_escapes(s, false)
}

// The same for `printf %b`, which also takes a bare `\nnn` as octal, as
// bash does: `echo -e 'a\101b'` prints `a\101b`, `printf '%b' 'a\101b'`
// prints `aAb`.
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
            // `\0nnn` in both, a bare `\nnn` only when the caller says so.
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

// `printf %q`: the argument as text a shell reads back as the same
// string, byte for byte as bash writes it. While every byte is printable
// ASCII, shell-special ones get a backslash; otherwise the whole string
// goes in `$'...'` with those bytes as octal escapes (`ä` is
// `$'\303\244'`), the spelling that survives encoding changes.
// `shell_quote` is the single-quote form, for `${v@Q}`.
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
        // `#` and `~` are special only at the front of a word, and bash
        // escapes them only there.
        let front_only = matches!(b, b'#' | b'~');
        if BACKSLASH_QUOTED.contains(&b) && (!front_only || i == 0) {
            out.push('\\');
        }
        out.push(b as char);
    }
    out
}

// The ASCII characters bash backslash-escapes.
const BACKSLASH_QUOTED: &[u8] = b" !\"#$&'()*,;<>?[\\]^`{|}~";

// The `$'...'` form. Only `'` and `\` are special inside it.
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
            // `\E`, which every shell reads, as bash writes it.
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

// Wraps `s` in single quotes, with an embedded `'` as `'\''`. What
// `${v@Q}` produces.
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
        // Removed from the real environment too, so an inherited `TIMEFORMAT`
        // can't change the default case.
        match format {
            Some(f) => {
                shell.assign_var("TIMEFORMAT", f.to_string());
            }
            None => unsafe { std::env::remove_var("TIMEFORMAT") },
        }
        shell.format_times(style, real, user, sys)
    }

    // Numbers a corpus case can't pin, each read off bash with the same
    // inputs.
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

    // That the shell suggests a near name on each path; `suggest`'s own
    // tests cover which name.
    #[test]
    fn a_mistyped_builtin_is_named() {
        assert!(stderr_of("ecoh hi\n").contains("did you mean 'echo'?"), "{}", stderr_of("ecoh hi\n"));
        assert!(stderr_of("expor X=1\n").contains("did you mean 'export'?"));
        // A name not near anything gets no guess.
        let miss = stderr_of("nosuchcommand_at_all\n");
        assert!(miss.contains("nosuchcommand_at_all"), "{miss}");
        assert!(!miss.contains("did you mean"), "{miss}");
    }

    // `win` is a builtin only on the editor's colon line, so elsewhere it's
    // not found, and the useful suggestion is the spelling that works there.
    #[test]
    fn win_outside_the_colon_line_says_what_to_use_instead() {
        let err = stderr_of("type win; win split\n");
        assert!(err.contains("type: win: not found"), "{err}");
        assert!(err.contains("::bish window"), "{err}");
        assert!(!err.contains("did you mean 'win'"), "{err}");
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

    // The "expected:" list and the suggestion read the same array.
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
    // A subshell sees what its parent sees, positional parameters
    // included. Expectations checked against bash.
    fn output(script: &str, args: &[&str]) -> String {
        let mut shell = super::Shell::new();
        shell.arg_frames = vec![args.iter().map(|s| s.to_string()).collect()];
        let captured = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        shell.set_sink_capture(captured.clone());
        shell.run_source_here(script, "<subshell>");
        let out = captured.borrow().clone();
        out
    }

    fn login_output(script: &str) -> String {
        let mut shell = super::Shell::new();
        shell.started_as_login = true;
        shell.login_shell = true;
        let captured = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        shell.set_sink_capture(captured.clone());
        shell.run_source_here(script, "<login>");
        captured.borrow().clone()
    }

    #[test]
    fn logout_refuses_a_shell_that_is_not_a_login_shell() {
        assert_eq!(output("logout 3 2>/dev/null; echo \"rc=$?\"\n", &[]), "rc=1\n");
        assert_eq!(output("shopt -q login_shell; echo \"q=$?\"\n", &[]), "q=1\n");
    }

    #[test]
    fn a_subshell_of_a_login_shell_is_not_one_and_a_command_substitution_is() {
        let script = "( logout 4 2>/dev/null; echo \"paren=$?\" )\nlogout 4 2>/dev/null | cat; echo \"stage=${PIPESTATUS[0]}\"\nx=$(logout 6; echo no); echo \"subst=$? [$x]\"\nlogout 7; echo not-reached\n";
        assert_eq!(login_output(script), "paren=1\nstage=1\nsubst=6 []\n");
    }

    #[test]
    fn login_shell_reports_how_the_shell_started_and_cannot_be_changed() {
        assert_eq!(login_output("shopt -u login_shell; echo \"rc=$?\"; shopt -q login_shell; echo \"q=$?\"\n"), "rc=0\nq=0\n");
        // Still a login shell's child, even where `logout` refuses.
        assert_eq!(login_output("( shopt -q login_shell; echo \"q=$?\"; logout 2>/dev/null; echo \"rc=$?\" )\n"), "q=0\nrc=1\n");
        assert_eq!(output("shopt -s login_shell; shopt -q login_shell; echo \"q=$?\"\n", &[]), "q=1\n");
    }

    #[test]
    fn exit_carries_on_after_a_word_it_cannot_use() {
        assert_eq!(output("exit x 2>/dev/null; echo \"rc=$?\"\n", &[]), "rc=2\n");
        assert_eq!(output("( exit 5 2 ) 2>/dev/null; echo \"rc=$?\"; ( exit -- 5 ); echo \"rc=$?\"\n", &[]), "rc=1\nrc=5\n");
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
        // ...and what `shift` left, not what the call started with.
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
    // `%q` output must match bash's and read back as itself in bish; the
    // two are checked separately. The values go to bash as arguments, the
    // only way to pass a control character without the quoting under test.
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
    // Checked against bash with the same specs and values, since C's
    // conversions have many corners: `%.0f` rounds half to even, `%g` picks
    // its form after rounding, `%e` writes at least two exponent digits,
    // zero padding goes after the sign but not around an infinity, integer
    // precision means minimum digits and silences `0`, `010` is octal, and
    // an argument is read as far as it parses (`3.9abc` is 3.9).
    //
    // Precision stays at 17 significant digits or fewer: bash converts
    // through `long double`, bish through `f64`. `LC_ALL=C`, or a Finnish
    // bash writes `3,14`. Skipped without bash.
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
        // Separated by SOH: `%b` can produce a newline and `%c` of nothing
        // is a NUL, so lines can't delimit results.
        let script: String = cases
            .iter()
            .map(|(spec, value)| {
                // `'A` is one of the values, so they're escaped for the
                // script.
                let (spec, value) = (spec.replace('\'', "'\\''"), value.replace('\'', "'\\''"));
                format!("printf '{spec}' '{value}'; printf '\\001'\n")
            })
            .collect();
        let out = std::process::Command::new("bash").env("LC_ALL", "C").arg("-c").arg(&script).output();
        let Ok(out) = out else { return };
        if !out.status.success() {
            return;
        }
        // bash also writes "invalid number" to stderr before using 0; bish
        // just uses 0.
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

// Whether printf's number reading consumed the whole argument, which is
// what bash calls a valid number. Leading whitespace is skipped, as by
// `strtol`, trailing isn't: ` 12` is fine, `12 ` and (for an integer)
// `1e3` aren't. `'X` counts.
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

// printf's integer: C's `strtoll` with base 0, the longest valid prefix
// in whatever base it announces. `3.9` is 3, `1e3` is 1, `010` is 8,
// `0x10` is 16. No valid prefix is 0 and an overflow saturates; bash
// warns about both. `'A` (or `"A`) is the code point of `A`, as in bash.
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
        // No digits reads as 0; too many saturate, as `strtoll` does on
        // ERANGE.
        Err(_) if valid == 0 => 0,
        Err(_) if negative => i64::MIN,
        Err(_) => i64::MAX,
    }
}

// The same for `%u`, `%o`, `%x`, `%X`: a negative argument wraps (`-42`
// is `ffffffffffffffd6`), and an overflow saturates in its sign's
// direction.
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

// The base an argument announces (`0x` hex, `0` octal, else decimal),
// as `strtol`'s base 0.
fn int_radix(rest: &str) -> (u32, &str) {
    match rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        Some(hex) => (16, hex),
        None => match rest.len() > 1 && rest.starts_with('0') {
            true => (8, &rest[1..]),
            false => (10, rest),
        },
    }
}

// Integer precision is a minimum digit count, zero-filled. Zero at
// precision zero prints nothing: `%.0d` of 0 is empty.
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

// printf's float: C's `strtod`, the longest valid prefix (decimal or
// exponent form, `0x`, `inf`/`nan`, surrounding space). No prefix is 0.
fn printf_float(arg: &str) -> f64 {
    let text = arg.trim();
    // bash's `'X` applies to float conversions too.
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

// How much of `text` is a number, as `strtod` measures it: sign,
// digits with an optional point, and an exponent only if it has digits.
// `12 34` is 12, `3.9abc` is 3.9; `.` and `abc` are nothing.
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
        // An `e` with nothing after it isn't part of the number.
        i = if j > digits_start { j } else { mantissa_end };
    }
    i
}

// String precision is a maximum length, cut on a character:
// `String::truncate` panics inside one.
fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

// `inf`/`nan` print as themselves, in the conversion's case, ignoring
// precision.
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

fn format_fixed(v: f64, precision: usize, upper: bool) -> String {
    non_finite(v, upper).unwrap_or_else(|| format!("{v:.precision$}"))
}

// `%e`. Rust's `{:e}` writes `3.14159e4` where C writes
// `3.141590e+04`, with a sign and at least two exponent digits.
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

// `%g`: the shorter of `%e` and `%f`, trailing zeros removed. Precision
// counts significant digits, and 0 means 1. The choice uses the
// exponent after rounding to that precision (999999.5 is `1e+06`), so
// the `%e` form is computed first and its exponent read back.
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

// `%g`'s cleanup of the digits: `1.230000e-05` to `1.23e-05`, `100.000`
// to `100`.
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

// One character's UTF-8 bytes, into a byte buffer.
fn push_char(buf: &mut Vec<u8>, c: char) {
    let mut bytes = [0u8; 4];
    buf.extend_from_slice(c.encode_utf8(&mut bytes).as_bytes());
}

// Up to `max` hex digits; fewer is fine (`\x7` is complete).
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

// What a pass over a printf format produced besides output, for
// `run_printf` to report: there's no shell to print through here.
#[derive(Default)]
pub(crate) struct PrintfOutcome {
    // Stop cycling the format: a `\c` in a `%b` argument, or an error.
    pub(crate) stop: bool,
    pub(crate) errors: Vec<String>,
    pub(crate) status: i32,
}

// C length modifiers, skipped (every integer here is 64-bit) so `%zu`
// and `%lld` reach their conversion. `q` is left out: `%q` is bash's
// quoting conversion.
const PRINTF_LENGTH_MODIFIERS: &str = "hlLjzt";

// Runs the format once over `values[*idx..]`, advancing `*idx` past what
// it consumed and appending to `out`. `run_printf` repeats it until the
// arguments run out.
pub(crate) fn printf_format_once(format: &str, values: &[String], idx: &mut usize, out: &mut String) -> PrintfOutcome {
    // Bytes, decoded at the end, so `\303\244` is the two bytes of `ä`.
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
                // `\nnn` is a byte in octal, up to three digits including
                // this one. `\0nnn` is the same, `0` being an octal digit.
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
                // `\u`/`\U` are a code point, written as UTF-8.
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

        // `%(FORMAT)T`: a strftime format and a Unix timestamp argument, `-1`
        // or none meaning now. Read before the flags, which don't apply.
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
            if !closed || chars.next() != Some('T') {
                // Not a time conversion: printed back like an unknown
                // conversion, but through the escape expander, since the
                // characters read looking for `)` skipped escape handling.
                // bash warns about an invalid time format.
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
                // In bash -1 is now and -2 is when the shell started. bish
                // has no start time, so both mean now.
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
                // `+` always shows a sign, ` ` a space in its place; only
                // for signed numeric conversions.
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
        // `%*d` takes the width from an argument, `%.*f` the precision.
        let mut width_digits = String::new();
        let mut star_width: Option<usize> = None;
        if chars.peek() == Some(&'*') {
            chars.next();
            let v = values.get(*idx).cloned().unwrap_or_default();
            *idx += 1;
            star_width = Some(printf_int(&v).unsigned_abs() as usize);
            // A negative `*` width left-aligns, as `-` does.
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
        // C length modifiers (`%zu`, `%lld`), skipped; see
        // `PRINTF_LENGTH_MODIFIERS`.
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
            // `printf '%'`, `printf '%5'`: no conversion character. bash
            // reports it and stops.
            outcome.errors.push(format!("`{}': missing format character", spec_so_far("")));
            outcome.status = 1;
            outcome.stop = true;
            out.push_str(&String::from_utf8_lossy(&buf));
            return outcome;
        };
        if !"sbqcdiuoxXfFeEgG".contains(conv) {
            // An unknown conversion abandons the printf, as in bash.
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
        // Collected apart from `outcome`: `next_arg` holds a mutable borrow
        // across this match.
        let mut errors: Vec<String> = Vec::new();
        let mut status = 0;
        // What zero padding and the sign flags apply to; `u`/`o`/`x`/`X` take
        // padding but no sign, as in C.
        let numeric = matches!(conv, 'd' | 'i' | 'u' | 'o' | 'x' | 'X' | 'f' | 'F' | 'e' | 'E' | 'g' | 'G');
        let signed = matches!(conv, 'd' | 'i' | 'f' | 'F' | 'e' | 'E' | 'g' | 'G');
        // An infinity or NaN has no digits, so C pads it with spaces whatever
        // the flags say. Decided from the value, not the text: `1.0e+20` and
        // `ff` pad with zeros.
        let mut zero_pad_ok = true;
        let mut piece = match conv {
            // Precision counts characters for these three.
            's' => truncate_chars(&next_arg(), precision.unwrap_or(usize::MAX)),
            'b' => {
                let (expanded, stop) = printf_b_escapes(&next_arg());
                let piece = truncate_chars(&expanded, precision.unwrap_or(usize::MAX));
                if stop {
                    // `\c` in a `%b` argument ends all the output.
                    buf.extend_from_slice(piece.as_bytes());
                    out.push_str(&String::from_utf8_lossy(&buf));
                    outcome.stop = true;
                    return outcome;
                }
                piece
            }
            'q' => truncate_chars(&printf_quote(&next_arg()), precision.unwrap_or(usize::MAX)),
            // An empty argument is still a character: bash writes a NUL.
            'c' => next_arg().chars().next().unwrap_or('\0').to_string(),
            'd' | 'i' | 'u' | 'o' | 'x' | 'X' => {
                let arg = next_arg();
                // bash prints the numeric prefix (`%d` of `1x` is 1) and
                // complains.
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
            // The float conversions. Precision defaults to 6, as in C. A
            // non-number reads as 0 without a warning, unlike bash.
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
            other => unreachable!("unhandled printf conversion {other:?}"),
        };
        // Integer precision is minimum digits, and C ignores `0` with it:
        // `%08.5d` of 42 is `   00042`.
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
            // Zero padding goes after the sign (`%05d` of -42 is `-0042`),
            // and never on an infinity or NaN.
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

// `read`'s view of a file descriptor, which never reads a byte it wasn't
// asked for: in `{ read -r x; cat; }` the rest belongs to `cat`, another
// process on the same fd, so anything buffered here would be lost.
// bash reads a pipe a byte at a time for the same reason.
struct UnbufferedFd {
    fd: i32,
    byte: [u8; 1],
    filled: bool,
}

impl UnbufferedFd {
    fn new(fd: i32) -> Self {
        UnbufferedFd { fd, byte: [0], filled: false }
    }

    // Whether the descriptor is open: `read -u 9` on an unopened fd must
    // say so, where reading would look like no input.
    fn is_open(fd: i32) -> bool {
        crate::platform::is_open(fd)
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
            // Straight to the fd: `std::io::stdin()`'s shared buffer is
            // what's being avoided.
            loop {
                // Polled before reading rather than made non-blocking
                // around the read: reading one byte can't block when the fd
                // is readable, and this is one syscall instead of three on
                // `while read`'s per-byte path.
                if crate::coroutine::in_coroutine() {
                    while !crate::poll::poll_readable_or_eof(self.fd, 0) {
                        crate::scheduler::park_readable(self.fd);
                    }
                }
                let e = match crate::platform::read_bytes(self.fd, &mut self.byte) {
                    Ok(1) => {
                        self.filled = true;
                        break;
                    }
                    Ok(_) => return Ok(&[]),
                    Err(e) => e,
                };
                match e.kind() {
                    // Nothing yet: the upstream stage is a coroutine on
                    // this thread. Only a pipe between two in-process
                    // stages is non-blocking.
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

// `${list:off:len}` over elements. A negative offset counts from the
// end; a negative length is an error for a list, though for a string
// bash reads it as a distance from the end.
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

// The CPU time a shell's children have used, in seconds, from
// `times(2)`, as the `times` builtin reads it.
fn child_cpu_times() -> (f64, f64) {
    match crate::platform::cpu_seconds() {
        Some((_, _, child_user, child_system)) => (child_user, child_system),
        None => (0.0, 0.0),
    }
}

// A process killed by a signal reports 128+signum, as in bash.
fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| crate::platform::exit_signal(&status).map(|s| 128 + s).unwrap_or(1))
}

// `read`'s line or character count from `reader`, shared by its two
// sources, so `-u`'s borrow of `coproc_fds` ends before `assign_var`.
// Hitting EOF before the delimiter or the count still yields the
// partial data, with `false`, as in bash.
//
// `raw` is `read` without `-r`: a backslash escapes the next character,
// the delimiter included, which continues the line. The escapes are
// removed later (`unescape_read_line`), because splitting must know
// which characters they protected.
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
                    // A line ending in an unescaped backslash continues:
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

// Removes `read`'s backslash escapes and marks the characters they
// protected, which are never field separators:
// `read a b <<< 'x\ y z'` gives a=`x y`, b=`z`.
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

/// bash's name for it, which distributions' integration scripts define.
const COMMAND_NOT_FOUND_HANDLER: &str = "command_not_found_handle";

// Time since the epoch; a clock set before it reads as 0.
fn unix_now() -> std::time::Duration {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
}

/// The `trap` targets that aren't signals, fired by the interpreter.
#[derive(Clone, Copy, PartialEq, Eq)]
// The discriminants index pseudo_trap_depth.
#[repr(usize)]
enum PseudoTrap {
    Debug = 0,
    Err = 1,
    Return = 2,
}

#[derive(Clone, Debug, PartialEq)]
enum TrapAction {
    Ignore,
    Run(String),
}

/// The platform layer's, under the name this file uses.
pub(crate) use crate::platform::current_umask;

// The lowest fd from 10 up that nothing has open: bash allocates
// `{name}` redirects there, clear of the 0-9 a script names.
fn next_free_fd() -> i32 {
    (10..256).find(|fd| !UnbufferedFd::is_open(*fd)).unwrap_or(10)
}

// Whether a write to this pipeline stage's stdout failed because its
// reader is gone, and whether anyone is asking. A stage that is its own
// process dies of SIGPIPE, which is what ends
// `while true; do echo x; done | head -2`; a stage in this shell can't
// die, and Rust ignores SIGPIPE, so the write returns `EPIPE` instead.
// Armed only around an in-process stage; `None` means nobody asks.
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

/// Swaps this thread's broken-pipe watch for another, returning the old
/// one. Pipeline stages share a thread, so the scheduler swaps each
/// task's watch in around every resume.
pub(crate) fn swap_broken_pipe(state: Option<bool>) -> Option<bool> {
    BROKEN_PIPE.with(|b| std::mem::replace(&mut *b.borrow_mut(), state))
}

fn broken_pipe_seen() -> bool {
    BROKEN_PIPE.with(|b| *b.borrow() == Some(true))
}

// An order-sensitive fingerprint of a call's positional parameters.
// FNV-1a, with no allocation.
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

// A `File` duplicating `fd` as it stands now: wherever a bare `exec`
// just pointed this stream.
fn dup_fd_as_file(fd: i32) -> Option<std::fs::File> {
    crate::platform::duplicate_as_file(fd)
}

// Duplicates of fds 0, 1 and 2 as they are now, for `run_in_child_shell`
// to restore after a bare `exec > file`. -1 where `dup` failed means
// nothing to restore.
fn save_fd012() -> [i32; 3] {
    [0, 1, 2].map(|fd| crate::platform::duplicate(fd).unwrap_or(-1))
}

fn restore_fd012(saved: [i32; 3]) {
    for (target, fd) in saved.into_iter().enumerate() {
        if fd < 0 {
            continue;
        }
        crate::platform::duplicate_onto(fd, target as i32);
        crate::platform::close_fd(fd);
    }
}

// Signals caught for `trap`, signal `n` at bit `n - 1`. The handler
// may only do async-signal-safe work, so it sets a bit, and
// `check_pending_signals` runs the trap between statements. So a trap
// doesn't fire while a script is blocked in `sleep` or `wait`; that would
// take EINTR handling at every blocking call.
static PENDING_SIGNALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

extern "C" fn record_pending_signal(sig: i32) {
    // No OS bish runs on has a signal past 64.
    if (1..=64).contains(&sig) {
        PENDING_SIGNALS.fetch_or(1u64 << (sig - 1), std::sync::atomic::Ordering::SeqCst);
    }
}

/// What `trap` does to one signal. Named because the OS's default and
/// ignore values are the platform layer's business, and only `Trapped`
/// is a handler, which `exec` resets.
enum Disposition {
    Default,
    Ignored,
    Trapped,
}

fn set_disposition(signum: i32, what: Disposition) {
    match what {
        Disposition::Default => crate::platform::default_signal(signum),
        Disposition::Ignored => crate::platform::ignore_signal(signum),
        Disposition::Trapped => crate::platform::handle_signal(signum, record_pending_signal),
    }
}

/// The signals ignored when this process started. POSIX doesn't let the
/// shell trap or reset those, so immunity granted from outside survives:
/// a script backgrounded by another has INT and QUIT ignored so Ctrl-C
/// at the terminal spares it, and `nohup` relies on the same for HUP.
static IGNORED_AT_ENTRY: std::sync::OnceLock<Vec<i32>> = std::sync::OnceLock::new();

/// Read once, from `main`: these are the process's dispositions, and a
/// `Shell` built later (a subshell, a test) must see the same answer.
pub fn record_signals_ignored_at_entry() {
    // Never SIGPIPE: Rust's runtime ignores it before `main`, so "ignored"
    // here would only say bish is a Rust program, and `trap '' PIPE` would
    // be refused. The cost is that a real `trap '' PIPE; exec bish` goes
    // unseen, which nothing after the runtime starts could detect.
    let pipe = signal_number("PIPE");
    let _ = IGNORED_AT_ENTRY
        .set(SIGNAL_NAMES.iter().map(|(_, num)| *num).filter(|num| Some(*num) != pipe && crate::platform::is_signal_ignored(*num)).collect());
}

/// Whether `trap` must leave this one alone. False for all when nothing
/// recorded them, as in unit tests, which don't go through `main`.
fn ignored_at_entry(signum: i32) -> bool {
    IGNORED_AT_ENTRY.get().is_some_and(|ignored| ignored.contains(&signum))
}

fn ignored_at_entry_list() -> &'static [i32] {
    IGNORED_AT_ENTRY.get().map(|v| v.as_slice()).unwrap_or_default()
}

/// The signals in `trap -p`'s listing: the traps, and those ignored at
/// entry, shown as `trap -- '' SIG` as bash does. They can't collide,
/// since `trap` refuses the ignored ones. Sorted by number, as bash
/// interleaves them.
fn trap_signal_listing(traps: &std::collections::HashMap<i32, TrapAction>, ignored: &[i32]) -> Vec<(i32, TrapAction)> {
    let mut entries: Vec<(i32, TrapAction)> = traps.iter().map(|(k, v)| (*k, v.clone())).collect();
    entries.extend(ignored.iter().filter(|n| !traps.contains_key(n)).map(|n| (*n, TrapAction::Ignore)));
    crate::sort::sort_by_key(&mut entries, |(n, _)| *n);
    entries
}

// SIGWINCH, tracked apart from `PENDING_SIGNALS`, which whichever
// session checks next clears: a resize must reach repl.rs, which owns
// everything sized to the terminal. The handler only stores a bool.
pub const SIGWINCH: i32 = crate::platform::SIGWINCH;
static WINCH_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn record_winch(_sig: i32) {
    WINCH_FLAG.store(true, std::sync::atomic::Ordering::SeqCst);
}

// At interactive startup, so repl.rs can poll `take_winch`.
pub fn install_winch_handler() {
    crate::platform::handle_signal(SIGWINCH, record_winch);
}

// True at most once per resize. repl.rs polls it once per input
// iteration.
pub fn take_winch() -> bool {
    WINCH_FLAG.swap(false, std::sync::atomic::Ordering::SeqCst)
}

/// How a re-exec'd shell learns the `$$` to report. Removed from the
/// environment as soon as it's read, so scripts never see it and a
/// grandchild that is a new shell doesn't inherit it.
const REEXEC_SHELL_PID: &str = "BISH_REEXEC_SHELL_PID";

/// Where a re-exec'd child's script goes, rather than argv. The script
/// carries every function and variable, exported or not, and
/// `/proc/<pid>/cmdline` is readable by every user; `/proc/<pid>/environ`
/// only by the owner. `main` removes it from the environment before
/// building a shell.
pub(crate) const REEXEC_SCRIPT: &str = "BISH_REEXEC_SCRIPT";

/// The `$$` this process was told to report, if it was told one.
fn inherited_shell_pid() -> Option<u32> {
    let value = std::env::var(REEXEC_SHELL_PID).ok()?;
    // Taken, not read: see `REEXEC_SHELL_PID`.
    unsafe { std::env::remove_var(REEXEC_SHELL_PID) };
    value.parse().ok()
}

/// The platform layer's: the numbers differ between OSes.
pub(crate) use crate::platform::SIGNAL_NAMES;

/// This OS's real-time range, if it has one; macOS doesn't.
fn realtime_range() -> Option<(i32, i32)> {
    crate::platform::REALTIME_SIGNALS
}

// Linux numbers the real-time range 34 to 64, leaving 32 and 33 to the
// thread library, hence the gap in `kill -l`. bash names the lower half
// from the bottom and the upper half from the top: `kill -l 49` is
// `RTMIN+15`, `kill -l 50` is `RTMAX-14`.
fn realtime_signal_name(num: i32) -> Option<String> {
    let (first, last) = realtime_range()?;
    if !(first..=last).contains(&num) {
        return None;
    }
    let above_min = num - first;
    let below_max = last - num;
    Some(match (above_min, below_max) {
        (0, _) => "RTMIN".to_string(),
        (_, 0) => "RTMAX".to_string(),
        _ if above_min <= (last - first) / 2 => format!("RTMIN+{above_min}"),
        _ => format!("RTMAX-{below_max}"),
    })
}

fn realtime_signal_number(bare: &str) -> Option<i32> {
    let (first, last) = realtime_range()?;
    let num = match (bare.strip_prefix("RTMIN"), bare.strip_prefix("RTMAX")) {
        (Some(""), _) => first,
        (_, Some("")) => last,
        (Some(rest), _) => first + rest.strip_prefix('+')?.parse::<i32>().ok()?,
        (_, Some(rest)) => last - rest.strip_prefix('-')?.parse::<i32>().ok()?,
        (None, None) => return None,
    };
    (first..=last).contains(&num).then_some(num)
}

// The two signals `trap` must refuse, kept out of `SIGNAL_NAMES` so that
// list answers "may I catch this?". Added back for naming (`kill -l`, a
// job's status) and for sending (`kill_signal_number`).
pub(crate) const UNCATCHABLE_SIGNALS: &[(&str, i32)] = &[("KILL", 9), ("STOP", crate::platform::SIGSTOP)];

/// Every signal by name and number, in numeric order, the real-time
/// range last.
pub(crate) fn all_signals() -> Vec<(String, i32)> {
    let mut all: Vec<(String, i32)> = SIGNAL_NAMES.iter().chain(UNCATCHABLE_SIGNALS.iter()).map(|(name, num)| ((*name).to_string(), *num)).collect();
    if let Some((first, last)) = realtime_range() {
        all.extend((first..=last).filter_map(|n| realtime_signal_name(n).map(|name| (name, n))));
    }
    crate::sort::sort_by_key(&mut all, |(_, n)| *n);
    all
}

// Everything `${NAME}` may name: an identifier, a positional's digits,
// or a special parameter.
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

// `INT`, `SIGINT` or a number. `EXIT`/0 isn't a signal; the caller
// handles it. KILL and STOP aren't here: see `UNCATCHABLE_SIGNALS`.
pub(crate) fn signal_number(name: &str) -> Option<i32> {
    let bare = name.strip_prefix("SIG").unwrap_or(name);
    if let Some(&(_, n)) = SIGNAL_NAMES.iter().find(|(n, _)| *n == bare) {
        return Some(n);
    }
    if let Some(n) = realtime_signal_number(bare) {
        return Some(n);
    }
    // A number must be one the kernel could deliver: bash rejects
    // `trap x 99999`.
    bare.parse::<i32>().ok().filter(|n| (1..=crate::platform::HIGHEST_SIGNAL).contains(n))
}

fn signal_name(num: i32) -> String {
    all_signals().iter().find(|(_, n)| *n == num).map(|(name, _)| name.clone()).unwrap_or_else(|| num.to_string())
}

pub(crate) fn send_signal(pid: u32, sig: i32) -> bool {
    crate::platform::send_signal(pid as i32, sig)
}

// Signals a whole process group (`Job::pgid`), as the terminal driver
// would.
pub(crate) fn send_signal_to_pgrp(pgid: u32, sig: i32) -> bool {
    crate::platform::send_signal_to_group(pgid as i32, sig)
}

/// This process's group, under the name `builtins::jobs` uses.
pub(crate) use crate::platform::process_group as getpgrp;
// `fg`/`bg` resume with SIGCONT; `FgJob::send_stop` uses SIGSTOP. BSD
// numbers them differently, so they come from the platform layer.
pub(crate) use crate::platform::{SIGCONT, SIGSTOP};

// How a `waitpid_untraced` wait ended.
pub(crate) enum JobWaitOutcome {
    // The final status, 128+signum if killed (as `exit_code_from_status`).
    Exited(i32),
    // Stopped, and resumable with SIGCONT. Callers treat the four stop
    // signals alike.
    Stopped(i32),
}

// A blocking wait for one process that also sees it stop
// (`WUNTRACED`), which `Child::wait` can't. Only for processes job
// control put in their own group (`Job::pgid`).
//
// With a socket to answer on it polls instead, so a peer asking about
// the editor is answered meanwhile (`session::service_while_busy`). The
// naps start at a quarter millisecond and double up to 20ms.
pub(crate) fn waitpid_untraced(pid: u32) -> JobWaitOutcome {
    let polling = crate::session::is_listening();
    let mut nap = std::time::Duration::from_micros(250);
    loop {
        match crate::platform::wait_for_child(pid as i32, !polling) {
            crate::platform::Waited::Running => {
                crate::session::service_while_busy();
                std::thread::sleep(nap);
                nap = (nap * 2).min(std::time::Duration::from_millis(20));
            }
            crate::platform::Waited::Stopped(signum) => return JobWaitOutcome::Stopped(signum),
            // 128 + n is bash's convention, not the OS's.
            crate::platform::Waited::Killed(signum) => return JobWaitOutcome::Exited(128 + signum),
            crate::platform::Waited::Exited(code) => return JobWaitOutcome::Exited(code),
            // Already reaped elsewhere: nothing to report, but it must
            // answer.
            crate::platform::Waited::Gone => return JobWaitOutcome::Exited(1),
        }
    }
}

// A user's home directory from /etc/passwd (field 6 where field 1 is
// the name). No NSS, so network directories aren't consulted.
fn home_of_user(user: &str) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == user).then(|| fields.nth(4).unwrap_or_default().to_string())
    })
}

// How `set -x` shows a value: bare when it can be, else single-quoted.
// `+ x=1`, not `+ x='1'`.
fn xtrace_quote(value: &str) -> String {
    let safe = value.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:=@+,%^".contains(c));
    match safe {
        true => value.to_string(),
        false => crate::serialize::quote_literal(value),
    }
}

// The same for a command word, where an empty one is quoted (`''`) or
// `echo ""` would trace as `+ echo`. An empty assignment value stays
// bare, as in bash.
fn xtrace_quote_word(value: &str) -> String {
    match value.is_empty() {
        true => "''".to_string(),
        false => xtrace_quote(value),
    }
}

// The variables `lookup_var` computes without storing, so `${!prefix*}`
// and completion can list them.
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

// `$HOSTNAME`, and the default prompt's host: from the OS, else the
// `HOSTNAME` environment variable.
pub fn get_hostname() -> String {
    crate::platform::hostname().or_else(|| std::env::var("HOSTNAME").ok()).unwrap_or_default()
}

// `${v@P}`'s `\u`.
fn prompt_username() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "user".to_string())
}

fn is_effective_root() -> bool {
    crate::platform::effective_user() == 0
}

// `\l`: the basename of the terminal's device name, empty without one.
fn tty_basename() -> String {
    let Some(name) = crate::platform::terminal_name(0) else { return String::new() };
    name.rsplit('/').next().unwrap_or("").to_string()
}

// Whether fd 0 is a terminal: `read -p` prompts only then, and
// `read -t` can only poll a real fd.
fn stdin_is_tty() -> bool {
    crate::platform::is_terminal(0)
}

// Waits up to `timeout_ms` for fd 0 to be readable (0 doesn't wait),
// for `read -t`.
fn stdin_ready(timeout_ms: i32) -> bool {
    crate::poll::poll_one(0, timeout_ms)
}

// The redirect actions applied to this process, for `exec`: alone, or
// before replacing the process. `apply_fd_redirects` does it in a child.
fn apply_fds_to_self(actions: Vec<FdAction>) -> Result<(), String> {
    for ef in actions {
        match ef {
            FdAction::Open { fd, file } => {
                let srcfd = std::os::fd::AsRawFd::as_raw_fd(&file);
                if !crate::platform::duplicate_onto(srcfd, fd) {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                clear_cloexec(fd);
                // When the file already sits on the target, dropping it
                // would close the redirect, so it's forgotten. Otherwise
                // dropping closes the spare.
                if srcfd == fd {
                    std::mem::forget(file);
                }
            }
            FdAction::Dup { fd, source } => {
                if !crate::platform::duplicate_onto(source, fd) {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                clear_cloexec(fd);
            }
            // Closing an unopened descriptor is fine: `{ echo a; } 3>&-` is
            // legal in bash.
            FdAction::Close(fd) => crate::platform::close_fd(fd),
        }
    }
    Ok(())
}

/// Cleared after every dup2: `dup2(fd, fd)` is a no-op that leaves
/// `FD_CLOEXEC` as it was, and Rust opens files close-on-exec, so a file
/// already on its target fd would be closed by the exec.
use crate::platform::clear_cloexec;

/// `for x in a b` / `select x in a b` as written. A missing list is the
/// positional parameters, printed as the list it stands for, as bash's
/// `declare -f` does.
fn loop_header(keyword: &str, var: &str, words: Option<&[Word]>) -> String {
    let items = match words {
        Some(words) => words.iter().map(crate::serialize::serialize_word).collect::<Vec<_>>().join(" "),
        None => "\"$@\"".to_string(),
    };
    format!("{} {} in {}", keyword, var, items)
}

/// Whether opening this target for reading would do more than test it:
/// a process substitution's pipe would lose data to a second reader, and
/// `/dev/tcp` and `/dev/udp` would connect.
fn opening_would_consume_it(path: &str) -> bool {
    path.starts_with("/dev/fd/") || path.starts_with("/proc/self/fd/") || path.starts_with("/dev/tcp/") || path.starts_with("/dev/udp/")
}

// A status is the low byte of what was asked for, as the kernel keeps
// it: `exit -1` is 255, `exit 300` is 44.
fn exit_status_byte(code: i32) -> i32 {
    code & 0xff
}

// The message and status for a command that couldn't be spawned, as
// bash answers: "command not found" for a bare name, a missing path
// says so, and a path that exists but can't run is 126, not 127. Shared
// by the ordinary spawn and a promoted session's pty spawn.
fn spawn_failure(name: &str, e: &std::io::Error) -> (String, i32) {
    // A near builtin name is worth suggesting, but only for NotFound.
    let not_found = e.kind() == std::io::ErrorKind::NotFound;
    let hint = match not_found {
        // A bash builtin bish answers differently points at its equivalent;
        // other unknown names get the nearest builtin.
        true => match crate::suggest::instead_of(name) {
            advice if !advice.is_empty() => advice,
            _ => crate::suggest::did_you_mean(name, KNOWN_BUILTINS.iter().copied()),
        },
        false => String::new(),
    };
    let (text, status) = match (not_found, name.contains('/')) {
        (true, false) => ("command not found".to_string(), 127),
        (true, true) => (os_message(e), 127),
        // execve on a directory fails with EACCES; bash says what it is.
        (false, _) if std::path::Path::new(name).is_dir() => ("Is a directory".to_string(), 126),
        (false, _) => (os_message(e), 126),
    };
    (format!("bish: {name}: {text}{hint}"), status)
}

// Arranges the redirect actions in the child, in one `pre_exec` hook:
// calling `pre_exec` twice on a `Command` isn't documented as safe.
fn apply_fd_redirects(command: &mut Command, actions: Vec<FdAction>) {
    // Descriptors are arranged in `pre_exec`, which only a forking platform
    // has. Elsewhere the spawn fails anyway.
    #[cfg(not(unix))]
    let _ = (command, actions);
    #[cfg(unix)]
    unsafe {
        command.pre_exec(move || {
            // bish ignores SIGINT for itself (`term::ignore_sigint`), and
            // an ignored disposition survives exec, so every child would
            // ignore Ctrl-C without this.
            crate::platform::default_signal(crate::platform::SIGINT);
            for ef in &actions {
                match ef {
                    FdAction::Open { fd, file } => {
                        if !crate::platform::duplicate_onto(std::os::fd::AsRawFd::as_raw_fd(file), *fd) {
                            return Err(std::io::Error::last_os_error());
                        }
                        clear_cloexec(*fd);
                    }
                    FdAction::Dup { fd, source } => {
                        if !crate::platform::duplicate_onto(*source, *fd) {
                            return Err(std::io::Error::last_os_error());
                        }
                        clear_cloexec(*fd);
                    }
                    // Closing an unopened descriptor is fine, as in
                    // `apply_fds_to_self`.
                    FdAction::Close(fd) => crate::platform::close_fd(*fd),
                }
            }
            Ok(())
        });
    }
}

/// One stream of a child's output: an anonymous file where there is one,
/// a named temp file where not, as `run_command_substitution` chooses.
/// A filter needs two.
struct Captured {
    /// The only handle on the stream. A named file's name is kept only to
    /// remove it.
    file: std::fs::File,
    path: Option<std::path::PathBuf>,
}

impl Captured {
    fn open() -> std::io::Result<Captured> {
        // Either way the descriptor is kept, and a named file is never opened
        // again by name, where another user could have swapped in a symlink.
        match capture_file() {
            Some(f) => Ok(Captured { file: f, path: None }),
            None => {
                let (file, path) = crate::platform::exclusive_temp_file("procsub").ok_or_else(|| std::io::Error::other("no capture file"))?;
                Ok(Captured { file, path: Some(path) })
            }
        }
    }

    /// The child's handle, separate so reading back has its own cursor.
    fn writer(&self) -> std::io::Result<std::fs::File> {
        self.file.try_clone()
    }

    fn read(self) -> String {
        let text = read_capture(self.file);
        if let Some(path) = self.path {
            let _ = std::fs::remove_file(&path);
        }
        text
    }
}

// A capture file for `$( )` with no name: a `memfd` on Linux. The child
// runs in this process but may spawn an external command, which needs a
// real fd, and a pipe would deadlock once output outgrew its buffer
// with nobody reading. `None` where there's no anonymous file.
fn capture_file() -> Option<std::fs::File> {
    crate::platform::anonymous_file()
}

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

/// A here-document's body as a readable descriptor: an anonymous file
/// (`capture_file`), so there's never a name on a shared `/tmp` for
/// another user to plant a symlink at or swap.
fn here_string_file(content: &str) -> Result<std::fs::File, String> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = crate::platform::anonymous_file().ok_or_else(|| "here-string: no temporary file available".to_string())?;
    f.write_all(content.as_bytes()).map_err(|e| format!("here-string: {}", e))?;
    // The command reads from the start; this descriptor is at the end.
    f.seek(SeekFrom::Start(0)).map_err(|e| format!("here-string: {}", e))?;
    Ok(f)
}

// `exec [-cl] [-a name] [command [argument ...]] [redirection ...]`.
// `exec -a foo` with no command is a bare `exec`, as in bash.
struct ExecFlags {
    // `-a NAME`: the argv[0] the new process sees.
    arg0: Option<String>,
    // `-c`: an empty environment.
    clear_env: bool,
    // `-l`: a dash before argv[0], which a login shell looks for.
    login: bool,
    // Index of the command word in argv.
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
                // The value may be glued on (`-afoo`) or the next word, as
                // with getopt's `a:`.
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

// The first `-X` in `args` with a letter not in `accepted`, as bash
// reports it (one letter, even from a cluster). `--` ends options and a
// lone `-` is an operand.
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

// A builtin given an option it doesn't have: the complaint, the
// builtin's usage line and status 2, as bash does.
pub(crate) fn bad_option_status(sh: &mut Shell, who: &str, opt: &str, usage: &str) -> i32 {
    sh_eprintln!(sh, "bish: {}: {}: invalid option", who, opt);
    sh_eprintln!(sh, "{}: usage: {}", who, usage);
    2
}

// `io::Error`'s message without Rust's " (os error N)", as strerror
// and bash give it, so a script can grep for "No such file or
// directory".
pub(crate) fn os_message(e: &std::io::Error) -> String {
    let text = e.to_string();
    match text.rfind(" (os error ") {
        Some(i) if text.ends_with(')') => text[..i].to_string(),
        _ => text,
    }
}

// `/dev/tcp/HOST/PORT` and `/dev/udp/HOST/PORT`, bash's socket redirect
// targets, handled here rather than as paths. `None` for any other
// path, which `open_in`/`open_out` then open normally.
fn dev_socket_file(path: &str) -> Option<Result<std::fs::File, String>> {
    let (proto, rest) = path.strip_prefix("/dev/tcp/").map(|r| ("tcp", r)).or_else(|| path.strip_prefix("/dev/udp/").map(|r| ("udp", r)))?;
    let (host, port) = rest.split_once('/')?;
    if host.is_empty() || port.is_empty() {
        return None;
    }
    Some(connect_dev_socket(proto, host, port).map_err(|e| format!("{}: {}", path, os_message(&e))))
}

// A builtin's `>&3` onto a descriptor opened elsewhere (by `exec`). A
// `dup`, so dropping the sink doesn't close the original. `None` for a
// closed fd.
fn dup_existing_fd(fd: i32) -> Option<std::fs::File> {
    crate::platform::duplicate_as_file(fd)
}

fn connect_dev_socket(proto: &str, host: &str, port: &str) -> std::io::Result<std::fs::File> {
    use std::os::fd::{FromRawFd, IntoRawFd};
    let addr = format!("{host}:{port}");
    if proto == "tcp" {
        let stream = std::net::TcpStream::connect(&addr)?;
        return Ok(unsafe { std::fs::File::from_raw_fd(stream.into_raw_fd()) });
    }
    // UDP's connect only records the peer, so it never blocks. The local
    // bind matches the target's address family, so an IPv6-only target
    // works.
    use std::net::ToSocketAddrs;
    let target = addr.to_socket_addrs()?.next().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "could not resolve host"))?;
    let bind_addr = if target.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
    let socket = std::net::UdpSocket::bind(bind_addr)?;
    socket.connect(target)?;
    Ok(unsafe { std::fs::File::from_raw_fd(socket.into_raw_fd()) })
}

// Splits `v` on IFS as bash does: a run of IFS whitespace is one
// delimiter, trimmed at the ends; each other IFS character is a
// delimiter of its own (`a::b` with `IFS=:` is `a`, ``, `b`), absorbing
// adjacent whitespace (`a : b` with `IFS=" :"` is `a`, `b`). The bools
// say whether `v` began or ended with a separator, so the caller knows
// whether the edge parts join a neighbouring chunk.
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
            // A trailing delimiter makes no empty field, unlike an embedded
            // one: `a:` with `IFS=:` is just `a`.
            trailing_sep = true;
            break;
        }
    }
    (parts, leading_sep, trailing_sep)
}

// The next IFS field of a line `read` has unescaped, from `from`;
// characters marked `escaped` never separate. The field and where the
// rest starts, which `read`'s last variable takes as it is.
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

/// One field of a split word, and the pattern it globs as. They differ
/// only where the word was quoted, so `pattern` is `None` for a field
/// with nothing quoted, which is most of them.
struct Field {
    text: String,
    pattern: Option<String>,
}

impl Field {
    /// What to glob this field as: its own text, unless quoting made part
    /// of it inert.
    fn pattern(&self) -> &str {
        self.pattern.as_deref().unwrap_or(&self.text)
    }
}

/// A field being built. The pattern copy starts at the first quoted
/// piece, from the text so far.
#[derive(Default)]
struct FieldBuilder {
    text: String,
    pattern: Option<String>,
}

impl FieldBuilder {
    /// Text that is its own pattern.
    fn push(&mut self, t: &str) {
        self.text.push_str(t);
        if let Some(p) = &mut self.pattern {
            p.push_str(t);
        }
    }

    /// Text that mustn't act as a pattern: quoted or backslashed, or an
    /// expanded `~`.
    fn push_inert(&mut self, t: &str) {
        if self.pattern.is_none() {
            self.pattern = Some(self.text.clone());
        }
        self.text.push_str(t);
        if let Some(p) = &mut self.pattern {
            p.push_str(&crate::glob::escape(t));
        }
    }

    fn finish(self) -> Field {
        Field { text: self.text, pattern: self.pattern }
    }
}

/// Ends the field in progress, if there is one.
fn close_field(fields: &mut Vec<Field>, current: &mut Option<FieldBuilder>) {
    if let Some(b) = current.take() {
        fields.push(b.finish());
    }
}

/// Ends it even when empty: a separator made it a field.
fn close_field_even_if_empty(fields: &mut Vec<Field>, current: &mut Option<FieldBuilder>) {
    fields.push(current.take().unwrap_or_default().finish());
}

// Appends an expansion's value to the fields being built. Quoted, it's
// literal text. Unquoted, it splits on IFS: leading or trailing
// separators force a field boundary (`pre$x` with `x=" y "` is `pre`,
// `y`), and a value that is all separators adds nothing.
fn append_splittable(fields: &mut Vec<Field>, current: &mut Option<FieldBuilder>, v: &str, quoted: bool, ifs: &str) {
    if quoted {
        // One field, never a pattern.
        current.get_or_insert_with(FieldBuilder::default).push_inert(v);
        return;
    }
    if v.is_empty() {
        return;
    }
    if ifs.is_empty() {
        // `IFS` empty: no splitting.
        current.get_or_insert_with(FieldBuilder::default).push(v);
        return;
    }
    let (parts, leading_sep, trailing_sep) = ifs_tokenize(v, ifs);
    if parts.is_empty() {
        close_field(fields, current);
        return;
    }
    if leading_sep {
        close_field(fields, current);
    }
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            close_field_even_if_empty(fields, current);
        }
        current.get_or_insert_with(FieldBuilder::default).push(part);
    }
    if trailing_sep {
        close_field_even_if_empty(fields, current);
    }
}

// `append_splittable` for `"$@"`: one part per positional parameter,
// never re-split, and none glob-eligible.
fn append_parts(fields: &mut Vec<Field>, current: &mut Option<FieldBuilder>, parts: &[String]) {
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            close_field_even_if_empty(fields, current);
        }
        current.get_or_insert_with(FieldBuilder::default).push_inert(part);
    }
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

// `${V^pattern}` and friends. An empty pattern (`${V^^}`) matches every
// character; otherwise each is matched as a `case` pattern. `all` picks
// every match rather than the first.
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

// `${V:offset}` and `${V:offset:length}`, by character. A negative
// offset counts from the end, and so does a negative length (an end
// position), as in bash.
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

/// The leftmost-longest match of `pattern` in `text` at or after
/// `start_from`, in character indices; `anchor` restricts it to the start
/// or end. `bounds` (see `char_bounds`) makes each candidate a slice, not
/// a new `String`: there are O(n^2) of them.
fn find_glob_match(text: &str, bounds: &[usize], start_from: usize, pattern: &str, anchor: ReplaceAnchor) -> Option<(usize, usize)> {
    let n = bounds.len() - 1;
    let slice = |a: usize, b: usize| &text[bounds[a]..bounds[b]];
    match anchor {
        ReplaceAnchor::Start => {
            if start_from > 0 {
                return None;
            }
            (start_from..=n).rev().find_map(|end| glob::matches(pattern, slice(start_from, end)).then_some((start_from, end)))
        }
        ReplaceAnchor::End => (start_from..=n).find_map(|s| glob::matches(pattern, slice(s, n)).then_some((s, n))),
        ReplaceAnchor::None => {
            // A pattern without `*` has a fixed width, so there's one end
            // to try per start: linear instead of quadratic, for the shape
            // most replacements have (`${s//a/b}`).
            if let Some(width) = glob::fixed_width(pattern) {
                // Wider than the whole string: nothing to try.
                if width > n {
                    return None;
                }
                return (start_from..=n - width).find(|&s| glob::matches(pattern, slice(s, s + width))).map(|s| (s, s + width));
            }
            for s in start_from..=n {
                if let Some((s0, e0)) = (s..=n).rev().find_map(|end| glob::matches(pattern, slice(s, end)).then_some((s, end))) {
                    return Some((s0, e0));
                }
            }
            None
        }
    }
}

/// The byte offset of each character in `s` and one past the last, so
/// `bounds[i]..bounds[j]` is characters `i` to `j`.
fn char_bounds(s: &str) -> Vec<usize> {
    let mut bounds: Vec<usize> = s.char_indices().map(|(at, _)| at).collect();
    bounds.push(s.len());
    bounds
}

/// The replacement for one match: `&` is the match, `\&` a literal `&`,
/// as in sed and bash. Done on the expanded replacement, so
/// `r='&'; ${s//X/$r}` inserts the match too, as in bash.
fn expand_replacement(repl: &str, matched: &str) -> String {
    let mut out = String::new();
    let mut chars = repl.chars();
    while let Some(c) = chars.next() {
        match c {
            '&' => out.push_str(matched),
            // The two escapes `expand_replacement_operand` writes; any other
            // backslash came from an expansion and is text.
            '\\' => match chars.next() {
                Some('&') => out.push('&'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            _ => out.push(c),
        }
    }
    out
}

/// Whether a `${...}` operand can only expand to itself, as most do
/// (`${p//\//_}`, `${s#a}`), skipping the lexer. The byte list is
/// deliberately broad: an extra byte costs the slow path, a missing one
/// a wrong answer. `the_shortcut_agrees_with_the_lexer` compares both
/// paths.
fn expands_to_itself(raw: &str) -> bool {
    !raw.bytes().any(|b| matches!(b, b'$' | b'`' | b'\'' | b'"' | b'\\' | b'~'))
}

// `${V/pat/repl}` (first match), `${V//pat/repl}` (all), and the anchored
// `${V/#pat/repl}` and `${V/%pat/repl}`, which can only match once.
fn glob_replace(s: &str, pattern: &str, repl: &str, global: bool, anchor: ReplaceAnchor) -> String {
    let chars: Vec<char> = s.chars().collect();
    let bounds = char_bounds(s);
    if matches!(anchor, ReplaceAnchor::Start | ReplaceAnchor::End) {
        return match find_glob_match(s, &bounds, 0, pattern, anchor) {
            Some((s0, e0)) => {
                let mut out: String = chars[..s0].iter().collect();
                out.push_str(&expand_replacement(repl, &chars[s0..e0].iter().collect::<String>()));
                out.extend(&chars[e0..]);
                out
            }
            None => s.to_string(),
        };
    }
    // An empty pattern, unanchored, matches nothing: `${s//""/Z}` is a
    // no-op, as in bash. A pattern that can match empty, like `@()`, does
    // splice. bash also refuses `@()`, `?()` and `@(*)` on an empty string
    // while replacing for `*`; it doesn't document why, so bish replaces
    // whenever the pattern matches.
    if pattern.is_empty() {
        return s.to_string();
    }
    let mut out = String::new();
    let mut pos = 0;
    loop {
        match find_glob_match(s, &bounds, pos, pattern, ReplaceAnchor::None) {
            Some((s0, e0)) => {
                out.extend(&chars[pos..s0]);
                out.push_str(&expand_replacement(repl, &chars[s0..e0].iter().collect::<String>()));
                pos = if e0 > s0 {
                    e0
                } else {
                    // An empty match must still advance, copying the
                    // character it steps over: `${s//@()/Z}` on `abc` is
                    // `ZaZbZc`.
                    if e0 < chars.len() {
                        out.push(chars[e0]);
                    }
                    e0 + 1
                };
                // The end ends the walk, as bash's `while (*s)`: otherwise
                // `${s//*/Z}` would also match empty at the end and give
                // `ZZ`.
                if !global || pos >= chars.len() {
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

// `declare -p`'s quoting: double quotes, with `"`, `\`, `$` and a
// backtick escaped, as bash prints it.
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

// The pure string transforms. `@A`, `@a`, `@K` and `@P` need shell
// state, and their callers handle them.
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

// The `$'...'` escapes (`read_ansi_c_string`). An unknown sequence keeps
// its backslash, as in bash.
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

// One line per builtin, for `help`: an index, not bash's manual pages.
// A test checks it against `KNOWN_BUILTINS` both ways.
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
    ("logout", "Leave a login shell."),
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

// Every builtin, for `type`, `command -v`, `enable`, completion, and
// `is_active_builtin`, which decides how a pipeline stage runs. See
// `every_dispatched_builtin_is_known`.
pub(crate) const KNOWN_BUILTINS: &[&str] = &[
    ":",
    "true",
    "false",
    "suspend",
    "times",
    "caller",
    "enable",
    "help",
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
    "logout",
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

// The options `shopt` knows, with bash 5's defaults for an ordinary
// shell, so `shopt` can list them and reject unknown names. Most have
// no effect beyond being tracked; `extglob` is always on (see
// `Shell::shopt_is_on`).
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

// `compgen -A setopt`: the `set -o` names `apply_shell_option` acts on,
// not all of bash's.
pub(crate) const SET_O_OPTIONS: &[&str] =
    &["errexit", "errtrace", "functrace", "monitor", "noclobber", "noexec", "noglob", "nounset", "pipefail", "posix", "xtrace"];

// A bishopt option's type and default. For Str and Color the default is
// text, parsed as a `--set` value would be (`bishopt_value`). Not `Copy`:
// an Int carries a range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BishOptDefault {
    Bool(bool),
    // Whole numbers in a range, so a bad column count fails at `--set`, not
    // as a rendering bug.
    Int(i64, std::ops::RangeInclusive<i64>),
    Str(&'static str),
    Color(&'static str),
}

// A bishopt option's value once `--set`. Color keeps the source text,
// which `bishopt get` prints back (`cornflowerblue`, not `#6495ed`),
// beside the parsed fallback list (`#ff0000, -bish-ansi(1)`); which
// candidate applies is decided at render time from the terminal's
// color support (`Shell::bishopt_color`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BishOptValue {
    Bool(bool),
    Int(i64),
    Str(String),
    Color(String, Vec<crate::csscolor::TermColor>),
}

// `Shell::change_directory`'s refusal under `set -r`, which the `cd`
// builtin matches to print its message.
pub(crate) const RESTRICTED: &str = "restricted";

/// The bash release `$BASH_VERSION` reports, and `--version` claims
/// compatibility with. `BASH_VERSINFO` is it taken apart.
pub const BASH_VERSION: &str = "5.2.21(1)-release";
pub(crate) const BASH_VERSINFO: &[&str] = &["5", "2", "21", "1", "release", "x86_64-pc-linux-gnu"];

// bishopt's registry, separate from `KNOWN_SHOPT_OPTIONS`: shopt's names
// are bash's to define, and a bish setting there could collide with a
// bash script's `shopt -s`. The `ui_col_*` defaults are `-bish-*` colors
// naming the ANSI slots `default_style` uses, so they follow the
// terminal's palette until `--set` pins a real color.
const KNOWN_BISHOPTS: &[(&str, BishOptDefault)] = &[
    // The active `::bish theme`, empty for none. Setting it inside a
    // `::bish theme begin`/`end` names the theme being declared instead
    // (`run_bish_theme_end`). `bishopt_value` doesn't look this one up in
    // `self.themes`, which would be circular.
    ("theme", BishOptDefault::Str("")),
    // The same for bish's own UI: the browser's entry types, rendered
    // markdown, diagnostics. Same machinery as `::bish hl` (theme.rs
    // mirrors bishedit::highlight), so both work in a theme.
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
    // How the file editor handles a line wider than the pane: off scrolls
    // sideways (vim's `nowrap`), on breaks it across rows. The three that
    // shape a wrapped line default on, unlike vim, since nearly every vimrc
    // turns them on; they do nothing while `wrap` is off.
    ("wrap", BishOptDefault::Bool(false)),
    ("linebreak", BishOptDefault::Bool(true)),
    ("breakindent", BishOptDefault::Bool(true)),
    // What a continued row starts with; empty for nothing.
    ("showbreak", BishOptDefault::Str("\u{21B3} ")),
    // Wrap at this column instead of the pane's edge. 0 is the pane width,
    // which also caps it.
    ("wrap_column", BishOptDefault::Int(0, 0..=1000)),
    // vim's: keep this many lines visible above and below the cursor.
    // Capped at half the pane.
    ("scrolloff", BishOptDefault::Int(0, 0..=200)),
    // No zero: bash's `HISTFILESIZE=0` empties the file, a trap for anyone
    // reading 0 as unlimited.
    ("history_size", BishOptDefault::Int(10_000, 100..=1_000_000)),
    // Characters besides letters and digits that `w`/`b`/`e`, `iw`, `*` and
    // Ctrl-W treat as part of a word: `-` suits kebab-case, `$` shell
    // variables.
    ("iskeyword", BishOptDefault::Str("_")),
    // For `wrap` off. `sidescrolloff` is vim's: keep this many columns
    // visible either side of the cursor.
    ("sidescrolloff", BishOptDefault::Int(0, 0..=200)),
    // vim's `listchars` `extends`/`precedes`: shown in the last/first column
    // when a line continues past that edge. Empty by default.
    ("extends", BishOptDefault::Str("")),
    ("precedes", BishOptDefault::Str("")),
    // Languages shown tabular: columns aligned on screen, file unchanged.
    // A language glob, matched as `abbr --lang` does (`!(csv)` turns off
    // CSV only). `*` means wherever a language has a tabular form
    // (`bishedit::tabular::style`).
    ("tabular", BishOptDefault::Str("*")),
    // Directory capabilities granted by owning the directory, without a
    // `::bish trust` entry: a glob over capability names. Empty by default:
    // you own a tarball as soon as you extract it, so ownership alone
    // grants nothing. `git` or `*` opts in.
    ("trust_owned", BishOptDefault::Str("")),
    // Where Ctrl+T's opener puts what you pick: `tab`, `here` (this pane),
    // `vsplit` (a pane below) or `hsplit` (beside), named for how the
    // panes end up. `opener_targets` is a glob over those for what Ctrl+T
    // cycles through. An unknown name falls back rather than erroring:
    // the dialog's border shows which is live.
    ("opener_target", BishOptDefault::Str("tab")),
    ("opener_targets", BishOptDefault::Str("*")),
    // A table in rendered markdown (`:preview`, `:help`). On, a table too
    // wide for the pane is fitted to it, its cells wrapping. Off, columns
    // keep their widths and the pager scrolls sideways, which suits tables
    // of numbers.
    ("table_wrap", BishOptDefault::Bool(true)),
    // Whether bish honours `.gitignore`: on, the file browser hides ignored
    // files until `i`. Named for the file, not the browser, since more of
    // bish will ask the same question.
    ("gitignore", BishOptDefault::Bool(true)),
    ("lsp", BishOptDefault::Bool(true)),
    ("lsp_timeout_ms", BishOptDefault::Int(1000, 0..=10000)),
    // URLs, link destinations and existing paths as OSC 8 hyperlinks.
    // Terminals without OSC 8 skip the sequence.
    ("hyperlinks", BishOptDefault::Bool(true)),
    // How much of a split, as a percentage, its dividers may take before
    // the panes furthest from focus fold behind one divider (repl.rs's
    // `collapsed_runs`). 100 never folds.
    ("divider_budget", BishOptDefault::Int(25, 0..=100)),
    // How much of the line each default-prompt component may take, as a
    // percentage; `0` is no limit. Independent maxima, so they needn't sum
    // to 100. The path gets the most, `user@host` the least.
    ("prompt_host_budget", BishOptDefault::Int(25, 0..=100)),
    ("prompt_cwd_budget", BishOptDefault::Int(40, 0..=100)),
    ("prompt_git_budget", BishOptDefault::Int(25, 0..=100)),
    // vim's `relativenumber`: lines numbered by distance from the cursor,
    // so `12j` can be read off the screen. The cursor's line keeps its
    // absolute number, as vim shows with `number` also set; bish's gutter
    // always numbers.
    ("relativenumber", BishOptDefault::Bool(false)),
    ("ignorecase", BishOptDefault::Bool(false)),
    ("inlayhints", BishOptDefault::Bool(true)),
    ("matchbrackets", BishOptDefault::Bool(true)),
    ("smartcase", BishOptDefault::Bool(false)),
    // Whether a project's `.editorconfig` overrides the settings below.
    // Off ignores it, overriding one without editing it.
    ("editorconfig", BishOptDefault::Bool(true)),
    // Whether bish asks for mouse reports: clicking places the cursor and
    // dragging selects. Off leaves selection to the terminal, for copying
    // to the system clipboard.
    ("mouse", BishOptDefault::Bool(true)),
    // Hovering a word in the editor shows what `K` would. Separate from
    // `mouse` because it makes the terminal report every pointer move
    // (DECSET 1003). Off with `mouse`.
    ("mouse_hover", BishOptDefault::Bool(true)),
    // The cursor's shape follows the mode: a block in Normal and Visual, a
    // bar in Insert, an underline in Replace.
    ("cursorshape", BishOptDefault::Bool(true)),
    // vim's names for `.editorconfig`'s `indent_style`, `indent_size`,
    // `tab_width` and `insert_final_newline`; vim has no
    // `trim_trailing_whitespace`.
    ("expandtab", BishOptDefault::Bool(true)),
    ("shiftwidth", BishOptDefault::Int(4, 1..=64)),
    ("tabstop", BishOptDefault::Int(4, 1..=64)),
    ("fixendofline", BishOptDefault::Bool(true)),
    ("trim_trailing_whitespace", BishOptDefault::Bool(false)),
    // vim's `fileformat`. Empty keeps whatever the file has, so saving a
    // CRLF file doesn't rewrite every line.
    ("fileformat", BishOptDefault::Str("")),
];

// Every bishopt name, in registry order, for completion
// (`bishedit::completion::bishopt_candidates`). Read from the registry,
// unlike `KNOWN_BUILTINS`' copy in bishedit, so it can't drift.
pub fn bishopt_names() -> Vec<&'static str> {
    KNOWN_BISHOPTS.iter().map(|(name, _)| *name).collect()
}

// The values `bishopt --set NAME` accepts when they're a fixed set: only
// a boolean's.
pub fn bishopt_values(name: &str) -> &'static [&'static str] {
    match KNOWN_BISHOPTS.iter().find(|(n, _)| *n == name) {
        Some((_, BishOptDefault::Bool(_))) => &["on", "off"],
        _ => &[],
    }
}

// The `::bish` subcommands, for completion
// (`bishedit::completion::bish_candidates`). Here, beside their
// dispatch, so they can't drift; `every_bish_subcommand_is_dispatched`
// checks. Canonical spellings only: a menu shows one name per thing,
// though aliases (`win`, `list`, `remove`, `prev`) work.
pub fn bish_subcommands() -> &'static [&'static str] {
    &["theme", "window", "hook", "hl", "lsp"]
}

// What follows `::bish <sub>`; empty where it isn't a fixed word.
pub fn bish_sub_subcommands(sub: &str) -> &'static [&'static str] {
    match sub {
        "theme" => crate::builtins::bish::THEME_SUBCOMMANDS,
        "window" | "win" => &["next", "previous", "new", "rename", "ls", "select"],
        "hook" => &["ls", "add", "rm", "help"],
        "lsp" => &["ls", "add", "rm", "status", "log", "restart", "help"],
        "map" => &["--mode=", "--erase", "--list", "help"],
        "trust" => &["--capabilities=", "--list", "--remove", "--check"],
        _ => &[],
    }
}

// `::bish lsp add`'s flags, each ending in `=` since it takes a value,
// so completion leaves the cursor where the value goes.
pub fn lsp_add_flags() -> &'static [&'static str] {
    &["--lang=", "--root=", "--root-cmd=", "--apply-edits=", "--setting="]
}

// The values `--apply-edits=` accepts, its one fixed set.
pub fn lsp_apply_edits_values() -> &'static [&'static str] {
    &["scoped", "never", "always"]
}

// One sentence per bishopt, for `bishopt --describe` and
// `:help options`, saying what it does; type, default and range come
// from `KNOWN_BISHOPTS`. A parallel table, since a third tuple field
// would touch every consumer; `every_bishopt_is_described` keeps them
// in step.
const BISHOPT_HELP: &[(&str, &str)] = &[
    ("theme", "The active theme: a `::bish theme` declaration or one bish ships. `::bish theme list` names them."),
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
    ("trust_owned", "Capabilities a directory you own is trusted for with no `::bish trust` entry (a glob; empty trusts nothing)."),
    ("opener_target", "Where Ctrl+T's opener starts: `tab`, `here`, `vsplit` (stacked) or `hsplit` (side by side)."),
    ("opener_targets", "Which of those Ctrl+T cycles through, as a glob over their names."),
    ("table_wrap", "Fit a rendered markdown table to the pane by wrapping its cells. Off, it keeps its full width and the pager scrolls sideways."),
    ("gitignore", "Honour `.gitignore`: the browser and completion leave ignored files out."),
    ("lsp", "Use the language servers registered with `::bish lsp add`."),
    ("lsp_timeout_ms", "How long to wait for a language server to answer a question like hover."),
    ("hyperlinks", "Emit OSC 8 terminal hyperlinks for URLs, links and resolved paths."),
    ("divider_budget", "How much of a split its pane dividers may take, as a percentage, before panes fold away."),
    ("prompt_host_budget", "How much of the line the prompt's `user@host` may take, as a percentage. 0 for no limit."),
    ("prompt_cwd_budget", "How much of the line the prompt's path may take, as a percentage, before whole directories are left out. 0 for no limit."),
    (
        "prompt_git_budget",
        "How much of the line the prompt's git branch may take, as a percentage, before its segments are shortened. 0 for no limit.",
    ),
    ("relativenumber", "Number lines by their distance from the cursor's."),
    ("ignorecase", "Ignore case when searching, unless smartcase says otherwise."),
    ("inlayhints", "Draw the language server's inline parameter-name and inferred-type hints."),
    ("matchbrackets", "Mark the bracket under the cursor and the one it pairs with, where `%` would jump."),
    ("smartcase", "With ignorecase on, an uppercase letter in the pattern makes that one search case-sensitive again."),
    ("editorconfig", "Let a project's `.editorconfig` override the settings below it."),
    ("mouse", "Ask the terminal to report mouse events. Off gives the terminal's own selection back."),
    ("mouse_hover", "Show what `K` would say when the pointer rests on a word in the editor."),
    ("cursorshape", "Change the terminal's cursor shape to show the editor's mode."),
    ("expandtab", "Indent with spaces rather than a literal tab."),
    ("shiftwidth", "How many columns one indent is."),
    ("tabstop", "How many columns a literal tab draws as."),
    ("fixendofline", "Give a file a final newline on save if it hasn't got one."),
    ("trim_trailing_whitespace", "Strip whitespace from the end of every line on save."),
    ("fileformat", "Line endings to write: `unix`, `dos`, `mac`. Empty keeps whatever the file had."),
];

// The terminal's color support from the environment, without
// terminfo: `COLORTERM=truecolor`/`24bit` is 24-bit, a `TERM` with
// `256color` is 256, `TERM=dumb` is none, anything else the basic 16.
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

/// Where `name` is found in `path_var`, the shell's `PATH`, which differs
/// from the process's once a script assigns it.
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

/// `base` and `target` joined and normalised as text (`.` dropped, `..`
/// cancelling the component before it), without the filesystem. This is
/// how `cd` remembers the route: `a/link/..` is `a`, where the kernel
/// would give `link`'s target's parent. `cd -P` asks for that.
fn logical_path(base: &std::path::Path, target: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let joined = if target.is_absolute() { target.to_path_buf() } else { base.join(target) };
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in joined.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => parts.clear(),
            Component::CurDir => {}
            // `/..` is `/`.
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

/// A plain `pipe(2)` as two owned fds. `Stdio::piped()` only hands the
/// read end back as a child's `ChildStdout`, and the writer here is this
/// shell.
fn make_pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    // Close-on-exec, or every stage spawned meanwhile would inherit the
    // write end and a reader downstream would never see end of input
    // (`echo x | cat` would hang). Where an end is meant to be inherited it
    // goes through `dup2`, which clears the flag.
    crate::platform::pipe_cloexec()
}

/// `make_pipe` for the scheduler's tests.
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

// Writes a diagnostic to `target`'s file (appending) if set, else to
// `sink`. For `write_command_error` and `unset`'s readonly error.
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

// Every process the shell starts for a script must inherit its
// environment, which means going through `Shell::command`. A
// `Command::new` that skips it gives a child a stale environment and
// no error, so this reads the source and fails on any other.
//
// `ALLOWED` counts per file, so adding a spawn to a listed file fails
// too; each entry says why its spawns are bish's own tooling. Test
// modules don't count: everything from a file's first `#[cfg(test)]` is
// skipped.
#[cfg(test)]
mod spawn_guard {
    /// `(file, how many, why it is not a shell spawn)`.
    const ALLOWED: &[(&str, usize, &str)] = &[
        ("src/exec.rs", 1, "`Shell::command` itself, the one place that builds a child's environment"),
        (
            "src/git.rs",
            1,
            "its own `command`, the one place that builds a `git` -- bish's own tooling rather than the script's, pinned there against a configuration that would change the output it parses",
        ),
        ("src/lspclient.rs", 1, "starts a language server for the editor, which is bish's, not the script's"),
        (
            "src/dapclient.rs",
            1,
            "starts a debug adapter for the editor -- the same kind of subprocess as a language server, and bish's rather than the script's",
        ),
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
                // A file that is all test support (`#![cfg(test)]`, as
                // `gittest.rs` and `tempdir.rs` are) counts as a test
                // module. The attribute is looked for on its first line
                // that is neither blank nor a comment.
                let opens_the_file = source.lines().find(|l| {
                    let l = l.trim();
                    !l.is_empty() && !l.starts_with("//")
                });
                if opens_the_file == Some("#![cfg(test)]") {
                    continue;
                }
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
mod replacement_tests {
    use super::{ReplaceAnchor, expands_to_itself, glob_replace};

    /// For every operand below that `expands_to_itself` accepts, the three
    /// slow paths (plain, pattern, replacement) must return it unchanged.
    #[test]
    fn the_shortcut_agrees_with_the_lexer() {
        let operands = [
            // What scripts write.
            "",
            ":",
            "/",
            ".",
            "-",
            "a",
            "abc",
            "hello ",
            " ",
            "\t",
            "\n",
            "a b c",
            // Glob syntax, which isn't expansion syntax.
            "*",
            "?",
            "[a-z]",
            "[[:space:]]",
            "[^abc]",
            "a*b",
            "*.txt",
            "@(a|b)",
            "!(x)",
            "+(y)",
            "?(z)",
            // Punctuation that matters elsewhere.
            "&",
            "&&",
            "#",
            "##",
            "%",
            "%%",
            "^",
            "^^",
            ",",
            ",,",
            "!",
            ";",
            "|",
            "(",
            ")",
            "{",
            "}",
            "=",
            "+",
            "<",
            ">",
            "[",
            "]",
            "a=b",
            "x;y",
            "a|b",
            "{a,b}",
            "a-b",
            "1..3",
            // Non-ASCII.
            "\u{e4}\u{f6}",
            "\u{1f600}",
            "caf\u{e9}",
            "a\u{300}b",
            // And what it must refuse.
            "$v",
            "${v}",
            "$(cmd)",
            "`cmd`",
            "'q'",
            "\"q\"",
            "\\*",
            "~",
            "~/x",
            "a$b",
            "a~b",
            "a\\b",
            "a'b",
            "a\"b",
            "a`b",
        ];
        let mut shell = super::Shell::new();
        for raw in operands {
            if !expands_to_itself(raw) {
                continue;
            }
            assert_eq!(shell.expand_raw(raw), raw, "expand_raw disagreed on {raw:?}");
            assert_eq!(shell.expand_glob_pattern_raw(raw), raw, "expand_glob_pattern_raw disagreed on {raw:?}");
            assert_eq!(shell.expand_replacement_operand(raw), raw, "expand_replacement_operand disagreed on {raw:?}");
        }
    }

    #[test]
    fn the_shortcut_refuses_only_what_it_has_to() {
        for raw in ["$v", "${v}", "$(echo x)", "`echo x`", "'q'", "\"q\"", "\\*", "~"] {
            assert!(!expands_to_itself(raw), "{raw:?} must take the slow path");
        }
        for raw in ["*", "[[:space:]]", "&", "a b", "#", "%%", "@(a|b)"] {
            assert!(expands_to_itself(raw), "{raw:?} is its own expansion and should not pay for the lexer");
        }
    }

    // Only reachable through extglob, which bash has off and bish can't
    // turn off, so bashdiff can't cover it. Checked against
    // `bash -O extglob`.
    #[test]
    fn an_empty_match_copies_the_character_it_steps_over() {
        assert_eq!(glob_replace("abc", "@()", "Z", true, ReplaceAnchor::None), "ZaZbZc");
        assert_eq!(glob_replace("abc", "?()", "Z", true, ReplaceAnchor::None), "ZaZbZc");
        assert_eq!(glob_replace("abc", "@()", "Z", false, ReplaceAnchor::None), "Zabc");
    }

    #[test]
    fn a_match_reaching_the_end_ends_the_walk() {
        assert_eq!(glob_replace("abc", "*", "Z", true, ReplaceAnchor::None), "Z");
        assert_eq!(glob_replace("abc", "c*", "Z", true, ReplaceAnchor::None), "abZ");
        assert_eq!(glob_replace("abab", "a", "Z", true, ReplaceAnchor::None), "ZbZb");
    }

    #[test]
    fn an_empty_pattern_matches_nothing_unanchored() {
        assert_eq!(glob_replace("abc", "", "Z", true, ReplaceAnchor::None), "abc");
        assert_eq!(glob_replace("abc", "", "Z", false, ReplaceAnchor::None), "abc");
        assert_eq!(glob_replace("abc", "", "Z", false, ReplaceAnchor::Start), "Zabc");
        assert_eq!(glob_replace("abc", "", "Z", false, ReplaceAnchor::End), "abcZ");
    }

    #[test]
    fn a_pattern_wider_than_the_string_does_not_match() {
        assert_eq!(glob_replace("ab", "abc", "Z", true, ReplaceAnchor::None), "ab");
        assert_eq!(glob_replace("x", "[ab]c", "Z", true, ReplaceAnchor::None), "x");
        assert_eq!(glob_replace("", "?", "Z", true, ReplaceAnchor::None), "");
        assert_eq!(glob_replace("", "[a]", "Z", true, ReplaceAnchor::None), "");
    }
}

#[cfg(test)]
mod tests {
    // In UTC, so the expected strings don't depend on where this runs.
    fn utc_printf(format: &str, args: &[&str]) -> String {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("TZ").ok();
        unsafe { std::env::set_var("TZ", "UTC0") };
        crate::platform::reload_timezone();
        let values: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let (mut idx, mut out) = (0, String::new());
        let _ = super::printf_format_once(format, &values, &mut idx, &mut out);
        match previous {
            Some(tz) => unsafe { std::env::set_var("TZ", tz) },
            None => unsafe { std::env::remove_var("TZ") },
        }
        crate::platform::reload_timezone();
        out
    }

    #[test]
    fn the_trap_listing_interleaves_inherited_ignores_with_traps_by_signal_number() {
        let number = |name: &str| super::signal_number(name).expect("a signal every platform names");
        let (hup, int, usr1) = (number("HUP"), number("INT"), number("USR1"));
        let mut traps = std::collections::HashMap::new();
        traps.insert(usr1, TrapAction::Run("echo t".to_string()));
        traps.insert(int, TrapAction::Ignore);

        // HUP is 1, INT 2, USR1 above both: the inherited one takes its place
        // in the order.
        let listing = super::trap_signal_listing(&traps, &[hup]);
        assert_eq!(
            listing,
            vec![(hup, TrapAction::Ignore), (int, TrapAction::Ignore), (usr1, TrapAction::Run("echo t".to_string()))],
            "by signal number, inherited ignore included"
        );
        // Nothing inherited, as in every unit test here.
        assert_eq!(super::trap_signal_listing(&traps, &[]).len(), 2);
        // `trap` refuses to record a signal ignored at entry, but if both
        // ever happened, the trap is what's reported.
        let listing = super::trap_signal_listing(&traps, &[int, usr1]);
        assert_eq!(listing, vec![(int, TrapAction::Ignore), (usr1, TrapAction::Run("echo t".to_string()))]);
    }

    // An external `ls`, since a builtin is found whatever `PATH` says.
    #[test]
    fn command_dash_p_resolves_against_a_standard_path() {
        if crate::exec::resolve_in_path("ls", "/usr/bin:/bin").is_none() {
            return; // no `ls` in the standard place; nothing to prove against
        }
        let run = |line: &str| {
            let mut sh = Shell::new();
            let out = Rc::new(RefCell::new(String::new()));
            sh.set_sink_capture(out.clone());
            sh.run_source_here(line, "<test>");
            out.borrow().clone()
        };
        assert_eq!(run("PATH=/nonexistent command -p ls / >/dev/null; echo rc=$?"), "rc=0\n");
        assert_eq!(run("PATH=/nonexistent command ls / >/dev/null 2>&1; echo rc=$?"), "rc=127\n");
    }

    #[test]
    fn a_disabled_builtin_is_not_a_builtin_to_anything_that_asks() {
        // Everything that classifies a name must agree with what runs:
        // `/usr/bin/echo`. bash reports the external for all of these.
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
        // `[ "$(type -t x)" = function ]` is how a script asks.
        let mut sh = Shell::new();
        let out = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        sh.set_sink_capture(out.clone());
        sh.run_source_here("f() { :; }\ntype -t f\ntype -t echo\ntype -t nosuchthing_at_all\n", "<test>");
        let text = out.borrow().clone();
        assert!(text.contains("function"), "{text:?}");
        assert!(text.contains("builtin"), "{text:?}");
        // An unknown name gets an empty answer.
        assert!(!text.contains("not found"), "{text:?}");
    }

    #[test]
    fn funcname_bash_source_and_bash_lineno_describe_the_stack() {
        // `BASH_SOURCE` names where each function is defined, `BASH_LINENO`
        // the line one level out where it was called; the sourced library
        // makes those different files. Checked against bash with these files.
        let dir = std::env::temp_dir().join(format!("bish-funcname-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lib = dir.join("lib.sh");
        let main = dir.join("main.sh");
        std::fs::write(&lib, "inner() {\n  n=\"${FUNCNAME[*]}\"\n  s=\"${BASH_SOURCE[*]}\"\n  l=\"${BASH_LINENO[*]}\"\n}\nouter() {\n  inner\n}\n")
            .unwrap();
        std::fs::write(&main, format!("source {}\nouter\ntop=\"${{FUNCNAME[*]}}\"\n", lib.display())).unwrap();

        let mut sh = Shell::new();
        sh.set_script_args(main.display().to_string(), Vec::new());
        // Running a script file puts the outer `main` frame on the stack;
        // `bish -c` doesn't (see `Shell::running_a_script`).
        sh.running_a_script = true;
        sh.run_source_here(&std::fs::read_to_string(&main).unwrap(), &main.display().to_string());

        assert_eq!(sh.debug_peek_var("n").as_deref(), Some("inner outer main"));
        assert_eq!(
            sh.debug_peek_var("s").as_deref(),
            Some(format!("{} {} {}", lib.display(), lib.display(), main.display()).as_str()),
            "where each function is defined, not where it was called"
        );
        // `inner` is called on line 7 of the library, `outer` on line 2 of
        // the script; 0 is the script.
        assert_eq!(sh.debug_peek_var("l").as_deref(), Some("7 2 0"));
        // Outside every function bash has no `FUNCNAME`, not "main".
        assert_eq!(sh.debug_peek_var("top").as_deref(), Some(""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn caller_walks_out_through_the_calls_that_got_here() {
        // Checked line for line against bash on the same script.
        let dir = std::env::temp_dir().join(format!("bish-caller-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("c.sh");
        std::fs::write(&script, "a() {\n  r0=$(caller 0)\n  r1=$(caller 1)\n  r2=$(caller 2)\n  rb=$(caller)\n}\nb() {\n  a\n}\nb\n").unwrap();
        let mut sh = Shell::new();
        let src = script.display().to_string();
        // Run as a script, not sourced: `caller` reports the file a frame's
        // line is in, and `source` doesn't update `script_name`.
        sh.set_script_args(src.clone(), Vec::new());
        sh.run_source_here(&std::fs::read_to_string(&script).unwrap(), &src);
        // The innermost call: its line, the function it's in, the file.
        assert_eq!(sh.debug_peek_var("r0").as_deref(), Some(format!("8 b {src}").as_str()));
        // Then the call to that function; nothing called `b`, and bash names
        // that `main`.
        assert_eq!(sh.debug_peek_var("r1").as_deref(), Some(format!("10 main {src}").as_str()));
        // Past the top is empty, which ends `while caller $i`.
        assert_eq!(sh.debug_peek_var("r2").as_deref(), Some(""));
        // Bare `caller` leaves out the function name.
        assert_eq!(sh.debug_peek_var("rb").as_deref(), Some(format!("8 {src}").as_str()));

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

        // Naming an unknown builtin is an error.
        assert_eq!(crate::builtins::shell::run_enable(&mut sh, &["-n".to_string(), "nosuch".to_string()]), 1);
        assert!(!sh.disabled_builtins.contains("nosuch"));
        // Dynamic loading is refused by name.
        assert_eq!(crate::builtins::shell::run_enable(&mut sh, &["-f".to_string(), "mod".to_string()]), 2);
    }

    #[test]
    fn every_builtin_is_described_and_every_description_is_a_builtin() {
        // Neither table is derived from the other, so they're checked against
        // each other.
        use std::collections::HashSet;
        let known: HashSet<&str> = KNOWN_BUILTINS.iter().copied().collect();
        let described: HashSet<&str> = BUILTIN_HELP.iter().map(|(n, _)| *n).collect();
        let mut missing: Vec<&&str> = known.difference(&described).collect();
        let mut extra: Vec<&&str> = described.difference(&known).collect();
        missing.sort_unstable();
        extra.sort_unstable();
        assert!(missing.is_empty(), "builtins with no help line: {missing:?}");
        assert!(extra.is_empty(), "help lines for things that are not builtins: {extra:?}");
        // One short line each.
        for (name, summary) in BUILTIN_HELP {
            assert!(!summary.contains('\n'), "{name}: one line");
            assert!(summary.len() <= 72, "{name}: {} chars is too long for an index", summary.len());
            assert!(summary.ends_with('.'), "{name}: a sentence");
        }
    }

    #[test]
    fn globignore_drops_matches_under_pathname_rules() {
        // Checked against bash; `*.o` leaves `sub/x.o` because `*` doesn't
        // cross `/`. Holds `CWD_LOCK`, since `cd` moves the process.
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
        // All ignored means nothing matched, so the pattern stays as text.
        assert_eq!(run("GLOBIGNORE='*'; r=$(echo *)"), "*");
        restore_cwd();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ifs_starts_out_set_to_the_default_the_way_bash_does() {
        // `old=$IFS; IFS=,; ...; IFS=$old` is how IFS is changed for a
        // moment; with IFS reading as empty when unset, the restore would
        // turn splitting off.
        let mut sh = Shell::new();
        sh.run_source_here("old=$IFS; IFS=,; IFS=$old; s=\"a b c\"; set -- $s; n=$#", "<test>");
        assert_eq!(sh.debug_peek_var("n").as_deref(), Some("3"), "splitting survives a save/restore");
        assert_eq!(sh.debug_peek_var("IFS").as_deref(), Some(" \t\n"));

        // Empty means no splitting; unset still means the default.
        let mut sh = Shell::new();
        sh.run_source_here("IFS=; s=\"a b c\"; set -- $s; n=$#", "<test>");
        assert_eq!(sh.debug_peek_var("n").as_deref(), Some("1"), "IFS= disables splitting");

        let mut sh = Shell::new();
        sh.run_source_here("unset IFS; s=\"a b c\"; set -- $s; n=$#", "<test>");
        assert_eq!(sh.debug_peek_var("n").as_deref(), Some("3"), "unset still falls back to the default");
    }

    #[test]
    fn a_prefix_assignment_reaches_a_builtin_and_then_goes_away() {
        // `IFS=: read a b` splits on `:`: a builtin reads the shell's
        // variables, not the environment. bash gives x/y and leaves IFS
        // alone.
        let mut sh = Shell::new();
        sh.run_source_here("IFS=: read a b <<< \"x:y\"", "<test>");
        assert_eq!(sh.debug_peek_var("a").as_deref(), Some("x"));
        assert_eq!(sh.debug_peek_var("b").as_deref(), Some("y"));

        // Restored afterwards to its old value, not to empty.
        let mut sh = Shell::new();
        sh.run_source_here("FOO=original; FOO=temp true; echo done", "<test>");
        assert_eq!(sh.debug_peek_var("FOO").as_deref(), Some("original"));

        // An unset name comes back unset: `${X-default}` and `set -u` can
        // tell.
        let mut sh = Shell::new();
        sh.run_source_here("NEVER_SET=temp true; echo hi", "<test>");
        assert_eq!(sh.debug_peek_var("NEVER_SET"), Option::None);
    }

    #[test]
    fn printf_formats_a_timestamp_the_way_bash_does() {
        // Checked against bash with the same inputs.
        assert_eq!(utc_printf("%(%Y-%m-%d)T", &["0"]), "1970-01-01");
        assert_eq!(utc_printf("%(%F %T)T", &["1000000000"]), "2001-09-09 01:46:40");
        // `%s` needs the timestamp itself, not just the broken-down time.
        assert_eq!(utc_printf("%(%s)T", &["1234567890"]), "1234567890");
        assert_eq!(utc_printf("%(%R|%D|%u|%w|%C)T", &["1000000000"]), "01:46|09/09/01|7|0|20");
        assert_eq!(utc_printf("%(%z)T", &["0"]), "+0000");
    }

    #[test]
    fn a_malformed_time_conversion_is_printed_back_rather_than_swallowed() {
        // bash warns about an invalid time format; either way the text comes
        // out, escapes processed.
        assert_eq!(utc_printf("no-time %(unterminated\n", &[]), "no-time %(unterminated\n");
        assert_eq!(utc_printf("%(%F)X", &["0"]), "%(%F)");
    }

    // Held by every test that moves the process's directory, and every
    // test that depends on it: a spawn carries the shell's cwd, so a spawn
    // while another test is between its `cd` and cleanup can land in a
    // directory about to vanish.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // Where a cwd-moving test puts the process back: the crate root, not
    // where it started, since a test that doesn't take `CWD_LOCK` may have
    // left it in a deleted temp directory.
    fn restore_cwd() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::env::set_current_dir(root).expect("the crate root is a real directory");
    }

    // `OLDPWD` set by a `cd` from outside a command (the file browser's
    // Ctrl-Y) must survive `sync_real_state_in` reapplying the session's
    // environment, or `cd -` goes elsewhere.
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
        // Checked through the shell, not `std::env::var`: other tests running
        // concurrently reapply their own snapshots over the process
        // environment.
        assert_eq!(shell.lookup_var("PWD"), inner_real.to_string_lossy());
        assert_eq!(shell.lookup_var("OLDPWD"), root_real.to_string_lossy());

        // After the session restores its environment, `cd -` still has
        // somewhere to go.
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

    // Restricted mode refuses at the write path, so nothing that moves the
    // shell, the file browser included, gets around it.
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

    // A path a script names resolves against the shell's directory, not
    // the process's. They're kept in step in a single shell, so this sets
    // them apart, as two interleaved pipeline stages are.
    #[test]
    fn a_path_resolves_against_the_shells_cwd_not_the_processs() {
        let dir = std::env::temp_dir().join(format!("bish-cwd-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::fs::write(dir.join("elsewhere/target.txt"), b"found me").unwrap();

        let mut shell = Shell::new();
        // The shell is told it's in `elsewhere`; the process never is.
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

        assert!(child.arrays.contains_key("x"), "child should inherit a snapshot of the parent's arrays");
        child.arrays.insert("y".to_string(), std::collections::BTreeMap::from([(0, "child".to_string())]));
        assert!(!parent.arrays.contains_key("y"), "parent must not see the child's later mutations");

        child.cwd = std::path::PathBuf::from("/child/only");
        assert_ne!(parent.cwd, child.cwd, "cwd must diverge independently per session");

        // Shared: a job pushed from either is visible from the other.
        let real_child = Command::new("true").spawn().expect("spawn true");
        child.push_job(vec![real_child], "true &".to_string());
        assert_eq!(parent.jobs.borrow().jobs.len(), 1, "parent must see the job the child backgrounded");
        assert_eq!(child.jobs.borrow().jobs.len(), 1);

        // Shared: promotion is about the whole terminal.
        assert!(!parent.is_promoted());
        child.promoted.set(true);
        assert!(parent.is_promoted(), "promotion must be visible from every session sharing this root");

        parent.jobs.borrow_mut().jobs[0].wait();
    }

    // Records each (line, depth) `DebugHook::on_statement` sees, to show
    // the hook fires at every depth, inside an in-process subshell or
    // substitution too (a separate `Shell` sharing `debug_hook`).
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
        // The 3 top-level statements, plus one inside each of the two
        // constructs.
        assert_eq!(calls.len(), 5, "{:?}", calls);
        assert_eq!(calls[0], (1, DebugDepth { subshell_depth: 0, call_depth: 0 }), "echo top");
        assert_eq!(calls[1], (2, DebugDepth { subshell_depth: 0, call_depth: 0 }), "the subshell statement itself");
        // A subshell's text is re-lexed from a string with no starting line,
        // so its statements report lines relative to it, from 1.
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

    // `command` has its own spawn path, which must honour a substitution's
    // capture. `mise`'s bash activation runs every `$(mise ...)` through
    // `command "$__MISE_EXE"`.
    #[test]
    fn command_builtin_honors_an_enclosing_command_substitution_capture() {
        // Spawns a program, so the directory must stay put (`CWD_LOCK`).
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x=$(command /bin/echo captured); echo "got:$x""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "got:captured\n");
    }

    // `exec CMD` inside an in-process subshell or substitution spawns CMD
    // rather than replacing the process every window shares. `/bin/true`,
    // because an external's stdout bypasses `capture_output`; the test below
    // covers output through the substitution's capture.
    #[test]
    fn exec_cmd_inside_a_subshell_does_not_kill_the_real_process() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"(exec /bin/true); echo "still alive: $?""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "still alive: 0\n");
    }

    // The editor's `:[range]!cmd`. The point is an external like `sort`,
    // whose stdout bypasses `OutputSink::Capture`, so this uses `$( )`'s
    // machinery.
    #[test]
    fn filter_through_command_feeds_stdin_and_captures_an_externals_stdout() {
        let mut shell = Shell::new();
        let (out, err, status) = shell.filter_through_command("sort", "c\na\nb\n").unwrap();
        assert_eq!((out.as_str(), err.as_str(), status), ("a\nb\nc\n", "", 0));

        // A builtin works too.
        let (out, _, _) = shell.filter_through_command("cat", "x\ny\n").unwrap();
        assert_eq!(out, "x\ny\n");

        // The trailing newline is kept, unlike `$( )`: it ends the buffer's
        // last line.
        let (out, _, _) = shell.filter_through_command("cat", "one\n").unwrap();
        assert_eq!(out, "one\n");

        // A non-zero status is returned: vim replaces the range either way.
        let (out, _, status) = shell.filter_through_command("false", "a\n").unwrap();
        assert_eq!(out, "");
        assert_ne!(status, 0);

        // stderr stays apart, so a complaint can't land in the buffer.
        let (out, err, _) = shell.filter_through_command("cat; echo trouble >&2", "a\n").unwrap();
        assert_eq!(out, "a\n");
        assert_eq!(err, "trouble\n");

        // A real command line: arguments, pipelines and quoting.
        let (out, _, _) = shell.filter_through_command("sort -r | head -n 2", "a\nb\nc\n").unwrap();
        assert_eq!(out, "c\nb\n");
    }

    #[test]
    fn exec_cmd_inside_a_command_substitution_is_captured() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"x=$(exec /bin/echo captured); echo "got:$x""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "got:captured\n");
    }

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

    // Order lives in `$1`/`$2` now, so trailing integers are plain words.
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
        // A `cmd &`'s output lands in the grid of the session that started
        // it, for the plain and the pty-attached case.
        let mut shell = Shell::new();
        let screen = Rc::new(RefCell::new(crate::vt100::Screen::new(6, 40)));
        // In `ensure_promoted`'s order: `promote_if_needed` writes the
        // alternate-screen switch through the sink, and it belongs to the
        // real terminal, not the grid.
        shell.promote_if_needed();
        shell.set_sink_grid(screen.clone());
        assert!(!screen.borrow().using_alternate, "the session's grid is never the one promotion switches");
        shell.run_source_here("sh -c 'echo from-a-background-job' &\n", "<test>");
        // Drain until the output arrives rather than racing the child, as the
        // idle tick does.
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
        // `cmd >file &` runs under a `Builtin` sink, so `sink_grid` must look
        // through it to the grid, or the unredirected stderr would be lost.
        let mut shell = Shell::new();
        let screen = Rc::new(RefCell::new(crate::vt100::Screen::new(6, 40)));
        shell.set_sink_grid(screen.clone());
        assert!(shell.sink_grid().is_some());
        let file = Capture::file(std::fs::File::create("/dev/null").unwrap());
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
        // Redefining replaces in place, keyed on name and language.
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
        // Erasing for a language it isn't defined for is a miss.
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
            // The default language is left off, so an abbreviation without
            // one prints as before.
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
        // Bare `shopt` lists every option without error or change.
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &[]), 0);
        assert!(shell.shopt_options.is_empty());

        // `shopt -s` and `shopt -u` with no names list; they don't toggle.
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &strs(&["-s"])), 0);
        assert_eq!(crate::builtins::shell::run_shopt(&mut shell, &strs(&["-u"])), 0);
        assert!(shell.shopt_options.is_empty(), "-s/-u alone must only list, never mutate");
    }

    // A local registry covering every value kind, independent of the real
    // options.
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
        // `-q` only suppresses the output; the status is the same.
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

    // Plants symlinks at the names an attacker would guess (this pid and a
    // counter from zero, spread since other tests may have advanced it)
    // and checks a here-document doesn't overwrite their targets.
    #[test]
    fn a_here_document_cannot_be_aimed_at_a_file_by_planting_a_symlink() {
        use std::io::Read;
        let dir = std::env::temp_dir();
        let victim = dir.join(format!("bish-herestring-victim-{}", std::process::id()));
        std::fs::write(&victim, "UNTOUCHED").unwrap();
        let planted: Vec<std::path::PathBuf> = (0..16).map(|n| dir.join(format!("bish-herestring-{}-{n}", std::process::id()))).collect();
        for path in &planted {
            std::fs::remove_file(path).ok();
            std::os::unix::fs::symlink(&victim, path).ok();
        }

        for body in ["first body", "second body"] {
            let mut f = here_string_file(body).expect("a here-document needs a descriptor");
            let mut back = String::new();
            f.read_to_string(&mut back).unwrap();
            assert_eq!(back, body, "the command has to read what the here-document said");
        }

        let after = std::fs::read_to_string(&victim).unwrap();
        for path in &planted {
            std::fs::remove_file(path).ok();
        }
        std::fs::remove_file(&victim).ok();
        assert_eq!(after, "UNTOUCHED", "a here-document reached a file through a planted symlink");
    }

    // `set -n` must actually stop execution; accepting and ignoring it
    // would run a script that asked not to run.
    #[test]
    fn noexec_parses_a_script_and_runs_none_of_it() {
        let mut shell = Shell::new();
        let out = Rc::new(RefCell::new(String::new()));
        shell.set_sink_capture(out.clone());

        // Everything before it runs; nothing after, not even an EXIT trap set
        // before it.
        shell.run_source_here("echo before; trap 'echo trapped' EXIT; set -n; echo after; for i in 1 2; do echo $i; done", "<test>");
        assert_eq!(out.borrow().as_str(), "before\n");

        // Recorded, for `$-` and `set -o`.
        assert_eq!(shell.shell_option_enabled("noexec"), Some(true));
        assert!(shell.lookup_var("-").contains('n'), "$- must carry the flag: {:?}", shell.lookup_var("-"));

        // And it can't be undone: the `set +n` above never ran.
        out.borrow_mut().clear();
        shell.run_source_here("set +n; echo back", "<test>");
        assert_eq!(out.borrow().as_str(), "", "nothing can run, including the thing that would turn it off");
        assert_eq!(shell.shell_option_enabled("noexec"), Some(true));
    }

    // An interactive shell ignores `set -n`, as bash does: a prompt that
    // ran nothing could never accept `set +n`. Not even recorded: `set +o`
    // says off and `$-` has no `n`.
    #[test]
    fn an_interactive_shell_discards_noexec_rather_than_storing_it() {
        let mut shell = Shell::new();
        shell.mark_interactive();
        let out = Rc::new(RefCell::new(String::new()));
        shell.set_sink_capture(out.clone());

        shell.run_source_here("set -n; echo still running", "<test>");
        assert_eq!(out.borrow().as_str(), "still running\n");
        assert_eq!(shell.shell_option_enabled("noexec"), Some(false), "not stored, so `set +o` agrees");
        assert!(!shell.lookup_var("-").contains('n'), "{:?}", shell.lookup_var("-"));
        // The long name is the same flag.
        shell.run_source_here("set -o noexec; echo also running", "<test>");
        assert_eq!(shell.shell_option_enabled("noexec"), Some(false));

        // A flag given at invocation is kept, applied before any prompt;
        // bash's `bash -ni -c` runs nothing either.
        let mut invoked = Shell::new();
        invoked.apply_shell_flag('n', true);
        invoked.mark_interactive();
        assert_eq!(invoked.shell_option_enabled("noexec"), Some(true), "the flag survives; honouring it is the loop's decision");
    }

    // `::bish theme list` must name the shipped themes with nothing
    // configured, since that's how one finds the names.
    #[test]
    fn theme_list_names_the_shipped_themes_and_marks_the_active_one() {
        let mut shell = Shell::new();
        let buf = Rc::new(RefCell::new(String::new()));
        shell.set_sink_capture(buf.clone());
        let listing = |sh: &mut Shell| {
            buf.borrow_mut().clear();
            assert_eq!(crate::builtins::bish::run_bish(sh, &strs(&["theme", "list"])).status(), 0);
            buf.borrow().clone()
        };
        let text = listing(&mut shell);
        assert!(text.contains("revontulet"), "{text:?}");
        assert!(text.contains("kaamos"), "{text:?}");
        assert!(!text.contains('*'), "nothing is active yet: {text:?}");

        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "kaamos"]), KNOWN_BISHOPTS), 0);
        let text = listing(&mut shell);
        assert!(text.lines().any(|l| l.starts_with("* kaamos")), "{text:?}");
        assert!(text.lines().any(|l| l.starts_with("  revontulet")), "{text:?}");

        // A declared theme joins the list; one shadowing a shipped name stays
        // one row and says so.
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "begin"])).status(), 0);
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "paper"]), KNOWN_BISHOPTS), 0);
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "end"])).status(), 0);
        let text = listing(&mut shell);
        assert_eq!(text.lines().count(), 3, "{text:?}");
        assert!(text.lines().any(|l| l.starts_with("  paper\tdeclared")), "{text:?}");
    }

    // Shipped themes sit between a declared theme and a registered
    // default, per name, so liking a shipped theme but one colour doesn't
    // mean copying the other forty.
    #[test]
    fn a_shipped_theme_colours_what_a_declared_one_leaves_alone() {
        let mut shell = Shell::new();
        // Nothing active: a shipped theme is inert until named.
        assert_eq!(shell.hl_color_for("keyword", crate::csscolor::ColorSupport::Truecolor), None);
        assert_eq!(shell.bishopt_value(KNOWN_BISHOPTS, "ui_col_directory"), shell.bishopt_value(KNOWN_BISHOPTS, "ui_col_directory"));
        let registered = shell.bishopt_value(KNOWN_BISHOPTS, "ui_col_directory");

        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "kaamos"]), KNOWN_BISHOPTS), 0);
        assert_eq!(shell.hl_color_for("keyword", crate::csscolor::ColorSupport::Truecolor), Some(vt100::Color::Rgb(0x6E, 0x0D, 0x91)));
        // The scheme's own 16-color reduction, on a terminal limited to that.
        assert_eq!(shell.hl_color_for("keyword", crate::csscolor::ColorSupport::Ansi16), Some(vt100::Color::Indexed(5)));
        // A UI option too, no longer the registered default.
        let active = shell.bishopt_value(KNOWN_BISHOPTS, "ui_col_directory");
        assert_ne!(active, registered, "a shipped theme has to beat a registered default");
        assert_eq!(
            active,
            Some(BishOptValue::Color(
                "#0B0DA0, -bish-bright-blue".to_string(),
                crate::csscolor::parse_terminal_list("#0B0DA0, -bish-bright-blue").unwrap()
            ))
        );

        // A declared theme of the same name wins for what it sets.
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "begin"])).status(), 0);
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "kaamos"]), KNOWN_BISHOPTS), 0);
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "keyword", "#010203"])), 0);
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "end"])).status(), 0);
        assert_eq!(shell.hl_color_for("keyword", crate::csscolor::ColorSupport::Truecolor), Some(vt100::Color::Rgb(1, 2, 3)));
        assert_eq!(
            shell.hl_color_for("comment", crate::csscolor::ColorSupport::Truecolor),
            Some(vt100::Color::Rgb(0x14, 0x7B, 0x7C)),
            "a name the declaration never mentioned still comes from the shipped theme"
        );
        // The listing agrees, bottom tier first.
        let colours: std::collections::HashMap<String, String> = shell.hl_colors().into_iter().collect();
        assert_eq!(colours.get("keyword").map(String::as_str), Some("#010203"));
        assert_eq!(colours.get("comment").map(String::as_str), Some("#147B7C, -bish-cyan"));

        // Switching moves every colour not set elsewhere.
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "revontulet"]), KNOWN_BISHOPTS), 0);
        assert_eq!(shell.hl_color_for("comment", crate::csscolor::ColorSupport::Truecolor), Some(vt100::Color::Rgb(0x11, 0x71, 0x72)));
        assert_eq!(
            shell.hl_color_for("keyword", crate::csscolor::ColorSupport::Truecolor),
            Some(vt100::Color::Rgb(0xCB, 0x5E, 0xFB)),
            "the kaamos declaration does not follow"
        );
        // A name nothing mentions is still unset: shipped themes aren't a
        // registry of defaults.
        assert_eq!(shell.hl_color_for("something_no_one_has_named", crate::csscolor::ColorSupport::Truecolor), None);
    }

    #[test]
    fn bish_theme_declares_a_named_theme_without_applying_its_opts_live() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "begin"])).status(), 0);
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "dark"]), KNOWN_BISHOPTS), 0);
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "accent", "blue"]), &test_bishopts()), 0);
        assert_eq!(crate::builtins::bish::run_bish(&mut shell, &strs(&["theme", "end"])).status(), 0);

        // Neither `theme` nor `accent` was applied live.
        assert_eq!(shell.bishopt_value(KNOWN_BISHOPTS, "theme"), Some(BishOptValue::Str(String::new())));
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))]))
        );
        // The theme was registered, without a `theme` entry.
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

        // Registering doesn't activate.
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))]))
        );

        // Activating it outside a declaration makes its options the fallback
        // wherever nothing else was set.
        assert_eq!(crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "dark"]), KNOWN_BISHOPTS), 0);
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("blue".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(0, 0, 255, 255))]))
        );

        // An explicit setting still wins over the active theme.
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
        // The original declaration is intact: a set after the refused nested
        // `begin` still lands in it.
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

    // Every name bish's highlighting produces must be settable at
    // `::bish hl`. Not all it accepts, since the namespace is open, but a
    // typo or duplicate here is a colour nobody can set.
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
        // No registry and no defaults, since the namespace is open: an
        // unmentioned name is unset, and the caller falls back to
        // `highlight::default_style`.
        assert_eq!(shell.hl_color("string"), None);
        assert_eq!(shell.hl_color("something_no_one_has_named"), None);
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "string", "#123456"])), 0);
        assert_eq!(shell.hl_color("string"), Some(vt100::Color::Rgb(0x12, 0x34, 0x56)));
        // A name nothing produces yet is accepted, so a server's semantic
        // token types can be coloured before bish knows them.
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "lsp_type_parameter", "#abcdef"])), 0);
        assert_eq!(shell.hl_color("lsp_type_parameter"), Some(vt100::Color::Rgb(0xab, 0xcd, 0xef)));
        // Unsetting returns it to unset.
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--unset", "string"])), 0);
        assert_eq!(shell.hl_color("string"), None);
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--unset", "string"])), 1, "unsetting what is not set says so");
    }

    // A theme carries the palette along with the options.
    #[test]
    fn a_theme_captures_highlight_colours_alongside_bishopts() {
        let mut shell = Shell::new();
        assert_eq!(crate::builtins::bish::run_bish_theme(&mut shell, &strs(&["begin"])), 0);
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "midnight"]), KNOWN_BISHOPTS);
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "ui_col_directory", "#111111"]), KNOWN_BISHOPTS);
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "string", "#222222"])), 0);
        assert_eq!(crate::builtins::bish::run_bish_theme(&mut shell, &strs(&["end"])), 0);

        // Declaring isn't switching.
        assert_eq!(shell.hl_color("string"), None);
        // Switching brings both.
        crate::builtins::bish::run_bishopt(&mut shell, &strs(&["--set", "theme", "midnight"]), KNOWN_BISHOPTS);
        assert_eq!(shell.bishopt_color("ui_col_directory"), Some(vt100::Color::Rgb(0x11, 0x11, 0x11)));
        assert_eq!(shell.hl_color("string"), Some(vt100::Color::Rgb(0x22, 0x22, 0x22)));

        // A direct setting wins over the theme, as for a bishopt.
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "string", "#333333"])), 0);
        assert_eq!(shell.hl_color("string"), Some(vt100::Color::Rgb(0x33, 0x33, 0x33)));
        // The listing includes the theme's entries: what's in force, not what
        // was typed.
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
        // UI colours are bishopts with defaults: `-bish-blue` is ANSI slot 4,
        // resolved by the terminal.
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

        // `get` echoes the whole list as typed, not the winning candidate.
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
        // Nothing here suits `ColorSupport::None`, so the last, least
        // demanding candidate is used.
        assert_eq!(shell.bishopt_color_for("ui_col_directory", ColorSupport::None), Some(vt100::Color::Indexed(1)));
        // `::bish hl` shares the candidate picking.
        assert_eq!(crate::builtins::bish::run_hl(&mut shell, &strs(&["--set", "string", "#ff0000, -bish-ansi(200), -bish-red"])), 0);
        assert_eq!(shell.hl_color_for("string", ColorSupport::Truecolor), Some(vt100::Color::Rgb(255, 0, 0)));
        assert_eq!(shell.hl_color_for("string", ColorSupport::Ansi16), Some(vt100::Color::Indexed(1)));
    }

    fn capture_output(shell: &mut Shell) -> Rc<RefCell<String>> {
        let buf = Rc::new(RefCell::new(String::new()));
        shell.set_sink_capture(buf.clone());
        buf
    }

    // The `compgen` expectations below were checked against bash.

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
        // No source: status 0, even with a word and no output.
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &[]), 0);
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["zzz"])), 0);
        // A source with no matches: 1.
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-W", "", "--", "x"])), 1);
        assert_eq!(crate::builtins::completion::run_compgen(&mut shell, &strs(&["-W", "abc def", "--", "zzz"])), 1);
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
        // `!` and `time` are reserved words in bish's grammar too, and
        // `type -t` answers "keyword" off this list.
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
        // Not the bare form `SIGNAL_NAMES` stores.
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
        // Capture combines stdout and stderr, so the error lands here.
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
        // `disabled` is a source that can be empty, so this is 1, not the
        // no-source 0.
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
        // Set through the shell: a name put in the process environment from
        // outside isn't a shell variable (see the `globals` field).
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

        // An absolute pattern, so the test doesn't depend on any cwd.
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

    // `-F` and `-C`'s round trip isn't tested here: `compgen::run_external`
    // re-runs `current_exe()`, which under `cargo test` is the test harness.

    // A reserved word where a command can't start is a plain word:
    // `echo function` must parse. The lexer doesn't know command position,
    // so the parser maps such keywords back to words (`keyword_text`).
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
        // Checked against bash: even `do`/`done` are wordlist items, since
        // the list ends at the `;` before the real `do`.
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
        // Checked against bash: `FOO=bar if` runs a command named `if` (127),
        // not an if-block.
        let mut shell = Shell::new();
        shell.run_source_here("FOO=bar if", "<test>");
        assert_eq!(shell.last_status, 127);
    }

    #[test]
    fn a_genuinely_bare_unmatched_marker_keyword_is_still_a_syntax_error() {
        // A `then` where a command starts, with no prefix, stays a syntax
        // error, as in bash (`true; then echo hi`).
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
        // `@Z` isn't a transform, so the whole `${...}` reads as a variable
        // name nothing can set, and `check_param_name` reports a bad
        // substitution, as bash does.
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        let result = shell.run_source_here(r#"v=hi; echo "[${v@Z}]""#, "<test>");
        assert!(matches!(result, ExecResult::Exit(1)), "{result:?}");
        // The capture takes stderr too; the `echo` never ran.
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
        // As in bash: `${arr[0]@A}` is `declare -a arr='1'`.
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
        // A plausible four-digit year; the clock can't be controlled here.
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
        // A plain `declare` in a function is local, like `local`.
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

        // What was printed must redefine the same function.
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
        // With a name: the bare name, as bash prints it.
        crate::builtins::vars::run_declare(&mut shell, "declare", &strs(&["-F", "foo"]), &[]);
        assert_eq!(buf.borrow().as_str(), "foo\n");
        // Without: a declaration line per function, which can be read back.
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
        // Moves the process's directory; see `CWD_LOCK`.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Checked against bash: a function named like a builtin, even a
        // special one (`export`, `return`), wins; `builtin` and `command`
        // bypass it.
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"cd() { echo "fake cd $1"; }; cd /somewhere"#, "<test>");
        assert_eq!(buf.borrow().as_str(), "fake cd /somewhere\n");
    }

    #[test]
    fn builtin_bypasses_a_shadowing_function() {
        // Moves the process's directory; see `CWD_LOCK`.
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
        // Moves the process's directory; see `CWD_LOCK`.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // `restrict_to_builtins` (the colon line) runs only builtins: a
        // function of the same name must not preempt one.
        let mut shell = Shell::new();
        shell.restrict_to_builtins = true;
        let buf = capture_output(&mut shell);
        // Defining the function still works; only running it is gated.
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
        // As in bash: `(1 [5]=x 2)` has indices 0, 5, 6.
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
        // `declare -A m=([a]=1 [b]=2)`: an array literal as a later word of
        // one of these builtins, not only as a leading assignment.
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
        // Moves the process's directory; see `CWD_LOCK`.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let result = shell.run_source_here("cd /tmp", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
    }

    // `let` doesn't expand its argument again: the word stage already did,
    // and a second pass would make `let "n=$untrusted"` run commands. bash
    // doesn't expand `let`'s text either.
    #[test]
    fn let_does_not_expand_what_was_already_expanded() {
        let mut shell = Shell::new();
        // Arithmetic through a variable still works.
        shell.run_source_here("x=5; let 'y=x+1'", "<test>");
        assert_eq!(shell.lookup_var("y"), "6");
        shell.run_source_here("let \"z=$x*2\"", "<test>");
        assert_eq!(shell.lookup_var("z"), "10");

        // A command substitution left after the word stage (quoted, or from a
        // variable) is a syntax error, not a command.
        for form in ["let \"q=$sub+1\"", "let \"$sub\"", "let 'q=$(echo 9)'"] {
            let mut shell = Shell::new();
            shell.run_source_here("sub='$(echo 9)'", "<test>");
            let result = shell.run_source_here(form, "<test>");
            assert!(matches!(result, ExecResult::Status(2)), "{form}: {result:?}");
            assert_eq!(shell.lookup_var("q"), "", "{form}: nothing may have been assigned");
        }
    }

    // A re-exec'd child gets restriction in its state script, since it has
    // no `-o` name for the option replay. The corpus checks behaviour; this
    // checks order: `set -r` after the assignments, since it makes PATH
    // readonly.
    #[test]
    fn the_preamble_restricts_a_child_only_after_telling_it_everything() {
        let mut shell = Shell::new();
        assert!(!shell.functions_preamble().contains("set -r"), "an unrestricted shell must not restrict its children");

        shell.run_source_here("set -r", "<test>");
        let preamble = shell.functions_preamble();
        assert!(preamble.ends_with("set -r\n"), "restriction has to be the last thing said: {preamble:?}");
        // `BASH_ENV` is readonly under restriction, so its assignment must
        // come before `set -r`.
        let mut shell = Shell::new();
        shell.run_source_here("BASH_ENV=/dev/null; set -r", "<test>");
        let preamble = shell.functions_preamble();
        let at = preamble.find("set -r").expect("restricted, so it is there");
        assert!(preamble[..at].contains("BASH_ENV="), "a readonly-under-r name has to be assigned before: {preamble:?}");
    }

    // A language server and a debug adapter are named outside dispatch, so
    // the name is checked where it's given, by the command-name rule.
    #[test]
    fn a_restricted_shell_refuses_to_register_a_program_by_path() {
        let mut shell = Shell::new();
        assert!(!shell.restricted_program_refused("x", "/usr/bin/anything"), "nothing is refused when unrestricted");

        shell.run_source_here("set -r", "<test>");
        assert!(shell.restricted_program_refused("x", "/usr/bin/anything"));
        assert!(shell.restricted_program_refused("x", "./relative"));
        assert!(!shell.restricted_program_refused("x", "bare-name"), "a bare name is allowed, exactly as it is for a command");

        // And through the builtin that registers one.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--lang=x", "/tmp/evil"])), 1);
        assert!(shell.lsp_servers.is_empty(), "a refused registration must not leave a server behind");
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--lang=x", "allowed-name"])), 0);
        assert_eq!(shell.lsp_servers.len(), 1);
    }

    #[test]
    fn restricted_mode_is_a_one_way_latch() {
        // Moves the process's directory; see `CWD_LOCK`.
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
            // Fatal, as in bash: a write to a readonly name stops a
            // non-interactive shell (`bash -r -c 'SHELL=/x; echo after'`).
            let result = shell.run_source_here(&format!("{name}=/tmp"), "<test>");
            assert!(matches!(result, ExecResult::Exit(1)), "{name}: {result:?}");
        }
        // Readable, just not writable.
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
        // Plain `<` isn't a write. Checked by status, since an external's
        // stdout bypasses the capture.
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

    // A `${var op word}` operand may contain a `}` inside a quoted nested
    // expansion (`${x:-"${x}"}`, the `${VAR+"${VAR}"}` idiom mise's
    // activation uses).
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

    // `declare -f`/`-F` on a non-function fails silently, as in bash
    // (unlike `declare -p`); mise's activation guards on it.
    #[test]
    fn declare_f_on_a_nonexistent_function_is_silent() {
        let mut shell = Shell::new();
        // The capture backs both stdout and stderr, so empty means nothing
        // was written.
        let buf = capture_output(&mut shell);
        let result = shell.run_source_here("declare -F not_a_real_function_xyz", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
        assert_eq!(buf.borrow().as_str(), "");
    }

    // Builtins honour their own redirects, checked against bash for each
    // form; mise's activation silences guards with
    // `declare -p foo >/dev/null 2>&1`.
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
        // It follows the live value.
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
        // An id is never reused, so `rm` can't hit the wrong one.
        assert_eq!(crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "editor:file:open", "__again"])), 0);
        assert_eq!(hook_ids(&mut shell), vec![2, 3]);
    }

    fn lsp_ids(shell: &Shell) -> Vec<u64> {
        shell.lsp_servers.iter().map(|s| s.id).collect()
    }

    // The `::bish` completion list must be real. Asked of the dispatcher:
    // it says "unknown subcommand" for anything it doesn't handle.
    #[test]
    fn every_offered_bish_subcommand_is_really_dispatched() {
        let unknown = |args: &[&str]| {
            let mut shell = Shell::new();
            // `Capture` takes stderr, where "unknown subcommand" goes.
            let out = capture_output(&mut shell);
            crate::builtins::bish::run_bish(&mut shell, &strs(args));
            let seen = out.borrow().clone();
            seen.contains("unknown subcommand")
        };
        // The control: an undispatched name does say so.
        assert!(unknown(&["bishNoSuchSubcommand"]), "the probe itself works");

        for sub in bish_subcommands() {
            assert!(!unknown(&[sub]), "::bish {sub} is offered but not dispatched");
            for second in bish_sub_subcommands(sub) {
                assert!(!unknown(&[sub, second]), "::bish {sub} {second} is offered but not dispatched");
            }
        }
    }

    // The same for the one flag with a fixed set of values.
    #[test]
    fn every_offered_apply_edits_value_is_accepted() {
        for value in lsp_apply_edits_values() {
            let mut shell = Shell::new();
            assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", &format!("--apply-edits={value}"), "x"])), 0, "{value}");
            assert_eq!(shell.lsp_servers[0].apply_edits, *value);
        }
    }

    // `KNOWN_BUILTINS` decides whether a pipeline stage re-execs to run a
    // builtin; a dispatched name missing from it is spawned as a program
    // and fails with ENOENT. The dispatcher is a `match`, so this reads
    // the source. An arm shaped differently enough to escape the pattern
    // is missed; a failure means the name is missing from the list.
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
        // Never reused, as with `hook` ids.
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
        // A word with a space in it is quoted.
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
        // `--root` keeps its default: the command is asked first, the markers
        // are the fallback.
        assert_eq!(shell.lsp_servers[0].root_markers, vec![".git".to_string()]);

        // Either order, and the `=` spelling.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--root=go.mod", "--root-cmd=go env GOMOD", "gopls"])), 0);
        assert_eq!(shell.lsp_servers[1].root_cmd, "go env GOMOD");
        assert_eq!(shell.lsp_servers[1].root_markers, vec!["go.mod".to_string()]);

        // A flag with nothing usable after it is a config error.
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
        // A `--setting` before `--lang` isn't taken for the command.
        assert_eq!(server.command, vec!["rust-analyzer".to_string()]);
        assert_eq!(server.lang, "rust");
        assert_eq!(
            server.settings,
            vec![
                ("rust-analyzer.check.command".to_string(), "clippy".to_string()),
                ("rust-analyzer.cargo.features".to_string(), "[\"all\"]".to_string()),
            ]
        );

        // A repeated key keeps the later value, as in `settings_tree`.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--setting", "a.b=1", "--setting", "a.b=2", "srv"])), 0);
        assert_eq!(shell.lsp_servers[1].settings, vec![("a.b".to_string(), "2".to_string())]);

        // A value may contain `=`; a key may not.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--setting", "a=b=c", "srv"])), 0);
        assert_eq!(shell.lsp_servers[2].settings, vec![("a".to_string(), "b=c".to_string())]);

        // Malformed is a config error, not a silently dropped setting.
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
        // Checked against bash.
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
        // After this was written: catches a zero or garbage without pinning a
        // date.
        assert!(secs > 1_700_000_000, "{secs}");

        let real = lines.next().unwrap();
        let (whole, frac) = real.split_once('.').expect("a decimal point, not a locale separator");
        assert!(whole.parse::<u64>().unwrap().abs_diff(secs) <= 1);
        assert_eq!(frac.len(), 6, "microseconds, zero-padded: {real}");
    }

    // Checked against bash; which CDPATH hits are announced (a literal `.`
    // is, an empty component isn't) can't be guessed.
    #[test]
    fn cdpath_searches_and_says_where_it_landed() {
        // Moves the process's directory; see `CWD_LOCK`.
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Put back afterwards, or later tests would run in a deleted
        // directory.
        let restore = std::env::current_dir().expect("a current directory");
        let root = std::env::temp_dir().join(format!("bish-cdpath-{}", std::process::id()));
        let (here, away) = (root.join("here"), root.join("away"));
        std::fs::create_dir_all(here.join("alpha")).unwrap();
        std::fs::create_dir_all(away.join("beta")).unwrap();
        std::fs::create_dir_all(away.join("alpha")).unwrap();

        // `unset CDPATH` first, so an inherited one doesn't interfere.
        let run = |script: &str| {
            let mut shell = Shell::new();
            let out = capture_output(&mut shell);
            shell.run_source_here(&format!("unset CDPATH\n{script}"), "<test>");
            out.borrow().clone()
        };
        let prelude = format!("cd {}; CDPATH={}", here.display(), away.display());

        // The announcement says CDPATH was used and which component won.

        // Found only via CDPATH: goes there and says so.
        assert_eq!(run(&format!("{prelude}; cd beta")), format!("{}\n", away.join("beta").display()));

        // In both: CDPATH wins.
        assert_eq!(run(&format!("{prelude}; cd alpha")), format!("{}\n", away.join("alpha").display()));

        // An empty component means here, and isn't announced.
        assert_eq!(run(&format!("cd {}; CDPATH=:{}; cd alpha", here.display(), away.display())), "");
        assert_eq!(run(&format!("cd {}; CDPATH=.:{}; cd alpha", here.display(), away.display())), format!("{}\n", here.join("alpha").display()));

        // Never searched: `./`, an absolute path, or no CDPATH.
        assert_eq!(run(&format!("{prelude}; cd ./alpha")), "");
        assert_eq!(run(&format!("{prelude}; cd {}", away.join("beta").display())), "");
        assert_eq!(run(&format!("cd {}; cd alpha", here.display())), "");

        // Not found anywhere: reported against the name as typed.
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

    // bash loops forever here. The guard keeps a handler that mistypes
    // something from calling itself until the machine gives up.
    #[test]
    fn a_handler_that_mistypes_something_does_not_call_itself_forever() {
        let mut shell = Shell::new();
        let out = capture_output(&mut shell);
        shell.run_source_here("command_not_found_handle(){ echo H; bish_also_missing; }\nbish_no_such_command\necho \"status=$?\"", "<test>");
        let seen = out.borrow().clone();
        assert_eq!(seen.matches('H').count(), 1, "the handler ran exactly once: {seen}");
        assert!(seen.contains("status=127"));
    }

    // `PROMPT_COMMAND` mustn't change `$?`, or a prompt coloured by status
    // would be wrong after the first line.
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
        // bash 5.1's array form: each element in turn.
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

    // Checked against bash. DEBUG and RETURN are inherited under
    // `functrace`, ERR under `errtrace`.
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

        // ERR has `errexit`'s exemptions.
        assert_eq!(run("trap 'echo E' ERR; if false; then :; fi; echo done"), "done\n");
        assert_eq!(run("trap 'echo E' ERR; false || true; echo done"), "done\n");
        assert_eq!(run("trap 'echo E' ERR; ! false; echo done"), "done\n");

        // And fires with `errexit` off: scripts trap ERR so as not to exit.
        assert_eq!(run("trap 'echo E' ERR; false; echo after"), "E\nafter\n");

        // Not inherited into a function without `errtrace`.
        assert_eq!(run("f(){ false; }; trap 'echo E' ERR; f; echo done"), "E\ndone\n");
        assert_eq!(run("set -E; f(){ false; }; trap 'echo E' ERR; f; echo done"), "E\nE\ndone\n");
        assert_eq!(run("f(){ echo in; }; trap 'echo RET' RETURN; f"), "in\n");
        assert_eq!(run("set -T; f(){ echo in; }; trap 'echo RET' RETURN; f"), "in\nRET\n");

        // `-` clears one, as for a signal.
        assert_eq!(run("trap 'echo D' DEBUG; trap - DEBUG; true; true"), "D\n");

        // A trap doesn't change the `$?` of the command it fired around.
        assert_eq!(run("trap 'false' DEBUG; echo hi; echo $?"), "hi\n0\n");
    }

    // Checked against bash.
    #[test]
    fn pipestatus_reports_every_stage() {
        let run = |script: &str| {
            let mut shell = Shell::new();
            let out = capture_output(&mut shell);
            shell.run_source_here(script, "<test>");
            out.borrow().clone()
        };
        // `sh -c` rather than `true`/`false`: a builtin pipeline stage
        // re-execs `current_exe()`, which under `cargo test` is the test
        // harness.
        assert_eq!(run(r#"sh -c 'exit 0' | sh -c 'exit 1'; echo "${PIPESTATUS[0]} ${PIPESTATUS[1]}""#), "0 1\n");
        assert_eq!(run(r#"sh -c 'exit 1' | sh -c 'exit 0'; echo "${PIPESTATUS[0]} ${PIPESTATUS[1]}""#), "1 0\n");
        // External stages for the same reason; with the real binary,
        // `exit 3 | exit 4 | exit 5` also gives `3 4 5`.
        assert_eq!(run(r#"sh -c 'exit 3' | sh -c 'exit 4' | sh -c 'exit 5'; echo "${PIPESTATUS[@]}""#), "3 4 5\n");
        // A plain command is a one-element pipeline.
        assert_eq!(run(r#"false; echo "${PIPESTATUS[0]}""#), "1\n");
        assert_eq!(run(r#"true; echo "${#PIPESTATUS[@]}""#), "1\n");
        // `$?` is the last stage, so an earlier failure needs this to be
        // seen.
        assert_eq!(run(r#"sh -c 'exit 1' | sh -c 'exit 0'; echo "$? ${PIPESTATUS[0]}""#), "0 1\n");
    }

    // Checked against bash.
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
        // Off again is off.
        assert_eq!(run(r#"shopt -s nocasematch; shopt -u nocasematch; [[ HELLO =~ ^hel ]] && echo yes || echo no"#), "no\n");
        // Captures still land in `BASH_REMATCH`.
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

        // A misspelling is a config error, not a silent fallback to `scoped`.
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--apply-edits=sometimes", "x"])), 2);
        assert_eq!(crate::builtins::bish::run_lsp(&mut shell, &strs(&["add", "--apply-edits"])), 2);
        assert_eq!(shell.lsp_servers.len(), 3);
    }

    // Any order: with four flags, insisting on one would make a misplaced
    // one part of the command.
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
        // A flag with nothing usable after it is a config error.
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

    // A typo'd event is refused rather than becoming a hook that never
    // fires.
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

    // Registration order: a config adding two hooks for one event means
    // that order.
    #[test]
    fn hooks_fire_in_the_order_they_were_added() {
        let mut shell = Shell::new();
        for name in ["__first", "__second", "__third"] {
            crate::builtins::bish::run_hook(&mut shell, &strs(&["add", "editor:file:open", name]));
        }
        assert_eq!(shell.hooks_for("editor:file:open", "bash"), vec!["__first", "__second", "__third"]);
    }

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

    #[test]
    fn every_hook_event_is_described() {
        let mut events: Vec<&str> = HOOK_EVENTS.to_vec();
        let mut described: Vec<&str> = HOOK_EVENT_HELP.iter().map(|(e, _)| *e).collect();
        events.sort_unstable();
        described.sort_unstable();
        assert_eq!(events, described, "HOOK_EVENTS and HOOK_EVENT_HELP disagree");
    }

    // Every internal node is a real prefix, so `editor:file:write` names
    // both write moments.
    #[test]
    fn the_event_hierarchy_is_prefix_matchable() {
        let with_prefix = |prefix: &str| HOOK_EVENTS.iter().filter(|e| e.starts_with(prefix)).count();
        assert_eq!(with_prefix("editor:file:write"), 2, "pre and post are one event's two moments");
        assert_eq!(with_prefix("editor:file"), 4);
        assert_eq!(with_prefix("editor:"), 4);
        assert_eq!(with_prefix("shell:exec"), 2, "and now shell:exec really does name two");
        assert_eq!(with_prefix("shell:"), 3);
        // Which `prewrite`/`postwrite` names would break.
        assert!(HOOK_EVENTS.iter().all(|e| !e.contains("prewrite") && !e.contains("postwrite")));
    }

    // A hook isn't a command the user ran, so it doesn't change `$?`.
    #[test]
    fn last_status_can_be_put_back_after_a_hook_runs() {
        let mut shell = Shell::new();
        shell.set_last_status(3);
        let saved = shell.last_status();
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

#[cfg(test)]
mod promoted_capture_tests {
    use super::*;

    // A shell in a split or tabbed window's state: promoted, writing to a
    // pane's grid.
    fn promoted_shell() -> Shell {
        let mut shell = Shell::new();
        shell.set_sink_grid(Rc::new(RefCell::new(crate::vt100::Screen::new(24, 80))));
        shell.promoted.set(true);
        shell
    }

    fn value_of(shell: &mut Shell, name: &str) -> String {
        shell.lookup_var(name)
    }

    /// Runs `src` as repl.rs runs a typed line, then waits for any job left
    /// in `pending_fg` (`discard_pending_fg`), so a test doesn't race the
    /// last command's process.
    fn run_line(shell: &mut Shell, src: &str) {
        shell.run_source_here(src, "test");
        shell.discard_pending_fg();
    }

    #[test]
    fn a_promoted_session_still_captures_an_external_commands_output() {
        // A promoted session gives a foreground external a pty, but inside
        // `$( )` the output belongs in the capture.
        let mut shell = promoted_shell();
        run_line(&mut shell, "x=$(/bin/echo captured)");
        assert_eq!(value_of(&mut shell, "x"), "captured");
    }

    #[test]
    fn the_same_capture_works_unpromoted_and_promoted_alike() {
        // A plain prompt never reaches the pty.
        let mut plain = Shell::new();
        run_line(&mut plain, "x=$(/bin/echo same)");
        let mut promoted = promoted_shell();
        run_line(&mut promoted, "x=$(/bin/echo same)");
        assert_eq!(value_of(&mut plain, "x"), value_of(&mut promoted, "x"));
        assert_eq!(value_of(&mut promoted, "x"), "same");
    }

    #[test]
    fn a_captured_external_command_in_a_loop_word_list_still_iterates() {
        // `for i in $(seq 1 3)` must run its body three times.
        let mut shell = promoted_shell();
        run_line(&mut shell, "out=; for i in $(/bin/echo a b c); do out=\"$out$i\"; done");
        assert_eq!(value_of(&mut shell, "out"), "abc");
    }

    #[test]
    fn a_chain_after_an_external_command_still_runs() {
        // A promoted session hands a foreground external to repl.rs as
        // `ExecResult::Fg`, a control-flow escape, which must not abandon the
        // rest of a `&&` chain (`cp a b && mv b c`).
        let dir = crate::tempdir::TempDir::new("fg-chain");
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, b"payload").expect("seed");
        let mut shell = promoted_shell();
        let (cp, mv) = (crate::toolpath::require("cp"), crate::toolpath::require("mv"));
        run_line(&mut shell, &format!("{cp} {} {} && {mv} {} {}", a.display(), b.display(), b.display(), dir.join("c").display()));
        assert!(dir.join("c").exists(), "the second half of the chain has to run");
        assert!(!b.exists(), "and it really moved the file");
    }

    #[test]
    fn every_statement_on_a_line_after_an_external_still_runs() {
        let dir = crate::tempdir::TempDir::new("fg-list");
        let seed = dir.join("seed");
        std::fs::write(&seed, b"x").expect("seed");
        let mut shell = promoted_shell();
        // No `>`: a redirect keeps a command off the pty path.
        let cp = crate::toolpath::require("cp");
        let script = format!(
            "{cp} {0} {1}; {cp} {0} {2}; {cp} {0} {3}",
            seed.display(),
            dir.join("1").display(),
            dir.join("2").display(),
            dir.join("3").display()
        );
        run_line(&mut shell, &script);
        for name in ["1", "2", "3"] {
            assert!(dir.join(name).exists(), "statement {name} was dropped");
        }
    }

    #[test]
    fn a_loop_body_with_an_external_runs_every_iteration() {
        // The same, inside a loop.
        let dir = crate::tempdir::TempDir::new("fg-loop");
        let seed = dir.join("seed");
        std::fs::write(&seed, b"x").expect("seed");
        let mut shell = promoted_shell();
        run_line(&mut shell, &format!("for f in a b c; do /bin/cp {} {}/$f; done", seed.display(), dir.path().display()));
        for name in ["a", "b", "c"] {
            assert!(dir.join(name).exists(), "iteration {name} never ran");
        }
    }

    #[test]
    fn a_condition_that_is_an_external_command_still_picks_a_branch() {
        let dir = crate::tempdir::TempDir::new("fg-if");
        let mut shell = promoted_shell();
        run_line(&mut shell, &format!("if /bin/true; then /bin/echo yes > {}; fi", dir.join("taken").display()));
        assert!(dir.join("taken").exists(), "the branch behind the condition never ran");
    }

    #[test]
    fn a_settled_external_reports_its_own_status() {
        // Driving the job here keeps its status for `$?`.
        let mut shell = promoted_shell();
        run_line(&mut shell, &format!("{} ; rc=$?", crate::toolpath::require("false")));
        assert_eq!(value_of(&mut shell, "rc"), "1");
        run_line(&mut shell, &format!("{} ; rc=$?", crate::toolpath::require("true")));
        assert_eq!(value_of(&mut shell, "rc"), "0");
    }

    #[test]
    fn an_enclosing_redirect_still_wins_over_the_pane() {
        // A group redirected to a file must not hand its command a pty
        // either.
        let dir = crate::tempdir::TempDir::new("promoted-redirect");
        let path = dir.path().join("out.txt");
        let mut shell = promoted_shell();
        run_line(&mut shell, &format!("{{ /bin/echo written; }} > {}", path.display()));
        assert_eq!(std::fs::read_to_string(&path).unwrap_or_default().trim(), "written");
    }
}
