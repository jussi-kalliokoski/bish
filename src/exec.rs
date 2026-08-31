use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::rc::Rc;

use crate::arith;
use crate::bishedit::highlight;
use crate::bishedit::snippet::{self, Abbr};
use crate::builtins;
use crate::compgen;
use crate::glob;
use crate::lexer::{Chunk, ReplaceAnchor, TransformKind, VarOp};
use crate::parser::{
    self, AndOr, ArrayLiteralItem, AssignMode, Combinator, ListItem, Pipeline, Program, Redirect, Sep, SimpleCommand, Word,
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
enum OutputSink {
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
    // guards being silent. `stdout`/`stderr` are `None` when that stream
    // has no redirect of its own, falling through to `previous`;
    // `dup_err_to_out`/`dup_out_to_err` make one stream follow wherever
    // the other currently resolves to (an explicit file if that stream has
    // one, `previous`'s own stream otherwise), matching `2>&1`/`1>&2`
    // without needing to know in advance what "wherever it goes" means.
    Builtin {
        previous: Box<OutputSink>,
        stdout: Option<Rc<RefCell<std::fs::File>>>,
        stderr: Option<Rc<RefCell<std::fs::File>>>,
        dup_err_to_out: bool,
        dup_out_to_err: bool,
    },
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
                use std::io::Write;
                let _ = std::io::stdout().write_all(s.as_bytes());
            }
            OutputSink::Grid(screen) => screen.borrow_mut().feed(onlcr(s).as_bytes()),
            OutputSink::Capture(buf) => buf.borrow_mut().push_str(s),
            OutputSink::Builtin { previous, stdout, stderr, dup_err_to_out: _, dup_out_to_err } => {
                use std::io::Write;
                if *dup_out_to_err {
                    // `1>&2`: stdout follows wherever stderr resolves to.
                    match stderr {
                        Some(f) => {
                            let _ = f.borrow_mut().write_all(s.as_bytes());
                        }
                        None => previous.write_err(s),
                    }
                } else {
                    match stdout {
                        Some(f) => {
                            let _ = f.borrow_mut().write_all(s.as_bytes());
                        }
                        None => previous.write_out(s),
                    }
                }
            }
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
            OutputSink::Builtin { previous, stdout, stderr, dup_err_to_out, .. } => {
                use std::io::Write;
                if *dup_err_to_out {
                    match stdout {
                        Some(f) => {
                            let _ = f.borrow_mut().write_all(s.as_bytes());
                        }
                        None => previous.write_out(s),
                    }
                } else {
                    match stderr {
                        Some(f) => {
                            let _ = f.borrow_mut().write_all(s.as_bytes());
                        }
                        None => previous.write_err(s),
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
    dup_err_to_out: bool,
    dup_out_to_err: bool,
}

struct StdioOverride {
    // `Some` => read from here (a real, shared, sequentially-consumed
    // reader) instead of the real stdin -- see SharedReaderState's own
    // doc comment for why this needs more than a bare File.
    stdin: Option<Rc<RefCell<SharedReaderState>>>,
    // `Some` => write here instead of the real stdout.
    stdout: Option<std::fs::File>,
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

impl std::io::BufRead for SharedStdinReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.local.is_empty() {
            let mut state = self.state.borrow_mut();
            if state.pending.is_empty() {
                use std::io::Read;
                let mut tmp = [0u8; 8192];
                let n = state.file.read(&mut tmp)?;
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
    opts: std::collections::HashMap<String, BishOptValue>,
    hl: std::collections::HashMap<String, String>,
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
}

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
    New { name: Option<String> },
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
    Split { horizontal: bool },
    // `window h/left`, `j/below`, `k/above`, `l/right`: move focus to
    // the nearest pane in that direction from the currently focused
    // one, vim Ctrl-w-hjkl style. A no-op if the current window isn't
    // split, or nothing lies in that direction.
    FocusPane(PaneDirection),
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
fn parse_size_spec(arg: &str) -> Option<SizeSpec> {
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

pub struct Shell {
    pub last_status: i32,
    functions: HashMap<String, parser::Command>,
    // Stack of positional-parameter frames; last() is the current scope
    // ($0 is tracked separately since it's never shifted/reassigned by calls).
    arg_frames: Vec<Vec<String>>,
    // Stack of `local` overlays; empty unless we're inside a function call.
    // A name only lives here if `local` explicitly declared it -- plain
    // assignment still targets the global (process-env) variable unless it
    // matches an existing local of the same name, matching bash semantics.
    var_scopes: Vec<HashMap<String, String>>,
    script_name: String,
    // Indexed arrays (`arr=(...)`). A BTreeMap (not Vec) so arrays are
    // genuinely sparse like bash's: `arr[10]=x` doesn't materialize empty
    // strings for indices 0..9, and `${#arr[@]}` counts only what's
    // actually set. Kept as one flat global map (every read/write site
    // just indexes straight into it) rather than a var_scopes-style stack,
    // since only `local -a`/`-A` need scoping at all -- see
    // array_local_stack/assoc_local_stack below for how that's retrofitted
    // via save/restore instead of a parallel lookup chain.
    arrays: HashMap<String, std::collections::BTreeMap<usize, String>>,
    // Associative arrays (`declare -A name`). Kept in a separate map from
    // `arrays` since their keys are arbitrary strings, not indices -- a name
    // in `assoc_names` is looked up here instead of `arrays` everywhere an
    // array is read or written.
    assoc_arrays: HashMap<String, OrderedMap>,
    assoc_names: std::collections::HashSet<String>,
    // `alias name=value`: stored and queryable (alias/unalias both work as
    // a plain table), but NOT expanded when a command runs. Real alias
    // expansion happens at parse time on an already-known table, textually
    // substituting a command-position word before the rest of that line is
    // even tokenized -- fundamentally at odds with this shell parsing an
    // entire script upfront before executing anything (a script-mode
    // architecture, unlike bash's line-at-a-time interactive parsing). It's
    // also off by default in bash for non-interactive shells (needs
    // `shopt -s expand_aliases`), so a script that never touches that
    // option -- the overwhelming majority -- already sees this exact
    // behavior from real bash too. Storing without expanding keeps a
    // defensive `alias foo=... ` preamble from failing the script outright
    // under `set -e`, without risking a half-correct expansion that only
    // works for some control-flow shapes.
    // Vec, not a map -- bash's own `alias` listing (and real bash's own
    // internal table) is in definition order, not sorted, and this list is
    // never large enough for linear lookup to matter.
    aliases: Vec<(String, String)>,
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
    /// `::bish hook`-registered commands, in the order they were added.
    /// Inherited by a virtual child exactly as `abbrs` is: a window you
    /// split off should behave like the one you split it from.
    pub hooks: Vec<Hook>,
    next_hook_id: u64,
    /// Declared language servers -- config, not processes. See
    /// `LspServer`.
    pub lsp_servers: Vec<LspServer>,
    next_lsp_id: u64,
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
    /// Set while a hook is running, so a hook that causes its own event
    /// -- a `shell:cwd:change` hook that `cd`s, most obviously -- fires
    /// once rather than forever. Not shared with a virtual child: a hook
    /// that legitimately starts a subshell should not have that
    /// subshell's own hooks suppressed.
    firing_hooks: bool,
    // One frame per active function call (pushed/popped alongside
    // var_scopes in call_function). `local -a`/`-A name` snapshots the
    // array's pre-local value here (None if it didn't exist) before
    // resetting it to empty, so returning from the function restores
    // whatever the caller had -- a save/restore shadow rather than a real
    // nested scope chain, since `arrays`/`assoc_arrays` themselves stay
    // flat (see the comment on `arrays` above).
    array_local_stack: Vec<Vec<(String, Option<std::collections::BTreeMap<usize, String>>)>>,
    assoc_local_stack: Vec<Vec<(String, Option<OrderedMap>)>>,
    // `declare -n`/`local -n ref=target`: ref's own stored value is the
    // *name* of the target variable, not user data -- lookup_var/assign_var/
    // var_is_set all redirect through resolve_nameref for any name in this
    // set before doing anything else, so reading/writing `ref` transparently
    // reads/writes `target` instead. Scalars only; array-element namerefs
    // (`declare -n ref=arr[0]`) aren't supported, a scoped gap.
    nameref_names: std::collections::HashSet<String>,
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
    dir_stack: Vec<String>,
    // shopt -s/-u NAME: explicit overrides only, keyed by name -- absent
    // means "use that name's own default from KNOWN_SHOPT_OPTIONS", not
    // "off" (several real bash options, e.g. cmdhist/promptvars, default
    // on). Most of these have no actual effect on bish's behavior beyond
    // being trackable/queryable/listable (e.g. extglob is unconditionally
    // on regardless of this map, see glob.rs), but recognizing the names
    // at all means `shopt -s extglob`/`shopt -s nullglob` in a script no
    // longer fails as an unknown command, which would otherwise abort the
    // whole script under `set -e`.
    shopt_options: std::collections::HashMap<String, bool>,
    // `bishopt --set/--unset NAME [VALUE]`: bish's own config surface, a
    // deliberately separate namespace from shopt_options above (shopt
    // exists only for bash-script compatibility -- see KNOWN_BISHOPTS'
    // own doc comment for why the two shouldn't mix). Same override-only
    // shape as shopt_options: absent means "use that option's own
    // registered default", `--unset` removes the entry outright rather
    // than writing the default back, so "explicitly unset" and "never
    // touched" collapse to the same state.
    bishopts: std::collections::HashMap<String, BishOptValue>,
    // `::bish theme begin`/`::bish theme end`'s own registry -- theme
    // name -> the bishopt overrides captured while declaring it (see
    // pending_theme's own doc comment for how a declaration fills this
    // in). Consulted by bishopt_value as a second-tier default, below an
    // explicit self.bishopts override but above KNOWN_BISHOPTS' own
    // hardcoded one, whenever the "theme" bishopt itself (an ordinary
    // Str option, set the normal way -- outside any declaration) names
    // one of these. A shell-wide table, cloned into a forked child the
    // same way bishopts itself is.
    themes: std::collections::HashMap<String, Theme>,
    /// Syntax-highlighting colours, by name -- see `::bish hl`.
    ///
    /// A plain map rather than a registry, because the names are open:
    /// `HighlightKind`'s own are what bish produces today, and a
    /// language server's semantic token types will be more of the same
    /// without any of them needing to be declared first. That is the
    /// whole reason these are not bishopts, which are a closed set with
    /// a default each.
    hl: std::collections::HashMap<String, String>,
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
    pending_theme: Option<Theme>,
    // `complete NAME`: registered completion specs, by command name -- see
    // run_complete's own doc comment. Consulted both by `compgen`-adjacent
    // introspection (`complete -p`/`-r`/`compopt`) and, via a per-prompt
    // snapshot repl.rs builds the same way as cwd/known_functions, by
    // bish's own interactive Tab completion (ShellCompletionProvider).
    completions: std::collections::HashMap<String, compgen::CompgenSpec>,
    // `complete -D`: the fallback spec used when no exact name matches.
    default_completion: Option<compgen::CompgenSpec>,
    // `readonly NAME`. Checked by assign_var, the single write path plain
    // assignment/local/export/declare/arithmetic-assignment/read/getopts
    // all funnel through, so marking a name here blocks writes everywhere
    // at once.
    readonly_names: std::collections::HashSet<String>,
    // `declare -i`/`local -i`: assignments to these names are evaluated as
    // arithmetic expressions instead of stored as literal text (checked in
    // assign_var, the single write path).
    integer_names: std::collections::HashSet<String>,
    // `declare -u`/`-l`: assignments to these names are case-folded
    // (checked in assign_var alongside integer_names).
    upper_names: std::collections::HashSet<String>,
    lower_names: std::collections::HashSet<String>,
    // `declare -x`/`export -x NAME` on a name that's currently a `local`:
    // globals are already unconditionally visible to children (assign_var
    // writes them straight to the process env), so this only matters for a
    // local -- assign_var additionally mirrors the value into the process
    // env for any name in this set, so child processes can see it despite
    // it living in var_scopes rather than env.
    exported_names: std::collections::HashSet<String>,
    // `>(cmd)` substitutions queued by the command currently being built,
    // to run (reading the temp file back) once it finishes; see
    // run_proc_sub_out/drain_proc_subs.
    proc_sub_out_pending: Vec<(String, String)>,
    // Every proc-sub temp file created for the command currently being
    // built, deleted once it finishes (drain_proc_subs).
    proc_sub_cleanup: Vec<String>,
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
    jobs: Rc<RefCell<JobTable>>,
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
    opt_monitor: bool,
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
    sink: OutputSink,
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
    pending_fg: Option<Job>,
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
    env_snapshot: std::collections::HashMap<String, String>,
    umask_snapshot: u32,
}

// A fresh, process/time-derived seed -- used both for a brand-new Shell
// and for new_virtual_child's child (which deliberately does NOT inherit
// the parent's current rng_state, so sibling sessions don't produce
// correlated $RANDOM sequences).
fn fresh_rng_seed() -> u64 {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x2545F4914F6CDD1D)
        ^ (std::process::id() as u64).wrapping_mul(0x9E3779B97F4A7C15);
    if seed == 0 { 0x2545F4914F6CDD1D } else { seed }
}

impl Shell {
    pub fn new() -> Self {
        Shell {
            last_status: 0,
            functions: HashMap::new(),
            arg_frames: vec![Vec::new()],
            var_scopes: Vec::new(),
            script_name: "bish".to_string(),
            arrays: HashMap::new(),
            assoc_arrays: HashMap::new(),
            assoc_names: std::collections::HashSet::new(),
            aliases: Vec::new(),
            abbrs: Vec::new(),
            hooks: Vec::new(),
            next_hook_id: 1,
            lsp_servers: Vec::new(),
            next_lsp_id: 1,
            lsp: Rc::new(RefCell::new(NoServices)),
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
            readonly_names: std::collections::HashSet::new(),
            integer_names: std::collections::HashSet::new(),
            upper_names: std::collections::HashSet::new(),
            lower_names: std::collections::HashSet::new(),
            exported_names: std::collections::HashSet::new(),
            proc_sub_out_pending: Vec::new(),
            proc_sub_cleanup: Vec::new(),
            rng_state: fresh_rng_seed(),
            shell_start: std::time::Instant::now(),
            seconds_offset: 0,
            jobs: Rc::new(RefCell::new(JobTable::new())),
            traps: std::collections::HashMap::new(),
            exit_trap: None,
            coproc_fds: std::collections::HashMap::new(),
            opt_errexit: false,
            opt_nounset: false,
            opt_xtrace: false,
            opt_pipefail: false,
            opt_noglob: false,
            opt_monitor: false,
            opt_restricted: false,
            opt_posix: false,
            suppress_errexit: 0,
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
            stdio_override: None,
            debug_hook: None,
            subshell_depth: 0,
            env_snapshot: std::env::vars().collect(),
            umask_snapshot: current_umask(),
        }
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
            arg_frames: vec![Vec::new()],
            var_scopes: Vec::new(),
            script_name: self.script_name.clone(),
            arrays: self.arrays.clone(),
            assoc_arrays: self.assoc_arrays.clone(),
            assoc_names: self.assoc_names.clone(),
            aliases: self.aliases.clone(),
            abbrs: self.abbrs.clone(),
            hooks: self.hooks.clone(),
            next_hook_id: self.next_hook_id,
            lsp_servers: self.lsp_servers.clone(),
            next_lsp_id: self.next_lsp_id,
            lsp: Rc::clone(&self.lsp),
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
            proc_sub_out_pending: Vec::new(),
            proc_sub_cleanup: Vec::new(),
            rng_state: fresh_rng_seed(),
            shell_start: std::time::Instant::now(),
            seconds_offset: 0,
            jobs: self.jobs.clone(),
            traps: self.traps.clone(),
            exit_trap: self.exit_trap.clone(),
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
            opt_posix: self.opt_posix,
            suppress_errexit: 0,
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
            stdio_override: None,
            debug_hook: self.debug_hook.clone(),
            subshell_depth: self.subshell_depth + 1,
            // Captured fresh from the real process rather than cloning
            // self.env_snapshot/umask_snapshot directly -- equal to it at
            // this exact instant regardless (new_virtual_child only ever
            // runs while `self` is the currently-synced-in session; see
            // sync_real_state_in/out's own doc comment), but this is the
            // more obviously-correct way to express "start identical to
            // whatever the real state actually is right now."
            env_snapshot: std::env::vars().collect(),
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
        if let Some(cmd) = self.exit_trap.take() {
            self.run_source_here(&cmd, "trap");
        }
    }

    // Real bash enables job control (`-m`/monitor) by default for an
    // interactive shell, no explicit `set -m` needed -- only a
    // non-interactive script has to opt in. repl.rs calls this once for
    // the root session at interactive startup, matching that; every
    // `window new` virtual child then inherits it automatically the same
    // way it inherits every other opt_* flag (see new_virtual_child).
    pub fn enable_monitor_mode(&mut self) {
        self.opt_monitor = true;
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

    fn resolve_job_spec(&self, spec: &str) -> Option<usize> {
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

    fn run_jobs(&mut self, _args: &[String]) -> i32 {
        let mut table = self.jobs.borrow_mut();
        let last_idx = table.jobs.len().checked_sub(1);
        let prev_idx = table.jobs.len().checked_sub(2);
        let mut to_remove = Vec::new();
        for (i, job) in table.jobs.iter_mut().enumerate() {
            let mark = if Some(i) == last_idx {
                "+"
            } else if Some(i) == prev_idx {
                "-"
            } else {
                " "
            };
            // Checked before Job::poll -- that only ever wraps Child::
            // try_wait, which never observes a stop (no WUNTRACED), so a
            // job this shell has already recorded as Stopped would
            // otherwise just look Running to it forever (see Job::
            // stopped's own doc comment).
            if job.stopped {
                // No trailing " &": bash only shows that for a job
                // actually launched with `&`, and a job stopped via
                // Ctrl-Z from the foreground wasn't.
                sh_println!(self, "[{}]{}  Stopped                 {}", job.id, mark, job.cmd_text);
                continue;
            }
            match job.poll() {
                Some(_) => {
                    sh_println!(self, "[{}]{}  Done                    {} &", job.id, mark, job.cmd_text);
                    to_remove.push(i);
                }
                None => {
                    sh_println!(self, "[{}]{}  Running                 {} &", job.id, mark, job.cmd_text);
                }
            }
        }
        for i in to_remove.into_iter().rev() {
            table.jobs.remove(i);
        }
        0
    }

    // disown [-a|-r] [%job|pid...]: removes matching jobs from the job
    // table without touching their children at all -- Rust's own
    // `Child::drop` never kills a still-running process, only closes the
    // handle, so simply removing the entry already gives disown's
    // "stop tracking, let it keep running independently" effect. bish
    // has no SIGHUP-on-exit for background jobs to begin with (a
    // separate, pre-existing gap), so the other half of real disown's
    // job -- surviving that signal -- doesn't apply here; this only
    // affects `jobs`/`wait`/`fg`/`bg` no longer seeing the job. Bare
    // `disown` (no flags, no specs) disowns just the current job,
    // matching bash.
    fn run_disown(&mut self, args: &[String]) -> i32 {
        let mut all = false;
        let mut running_only = false;
        let mut specs: Vec<&String> = Vec::new();
        for a in args {
            match a.as_str() {
                "-a" => all = true,
                "-r" => running_only = true,
                _ => specs.push(a),
            }
        }
        if all {
            self.jobs.borrow_mut().jobs.clear();
            return 0;
        }
        if running_only && specs.is_empty() {
            self.jobs.borrow_mut().jobs.retain(|j| j.stopped);
            return 0;
        }
        if specs.is_empty() {
            let mut table = self.jobs.borrow_mut();
            if table.jobs.is_empty() {
                sh_eprintln!(self, "bish: disown: current: no such job");
                return 1;
            }
            table.jobs.pop();
            return 0;
        }
        // resolve_job_spec takes its own immutable borrow of self.jobs,
        // so every spec is resolved to an index before the single
        // borrow_mut below that actually removes them (highest index
        // first, so earlier removals don't shift later indices out from
        // under this same pass).
        let mut status = 0;
        let mut idxs: Vec<usize> = Vec::new();
        for s in specs {
            match self.resolve_job_spec(s) {
                Some(i) => idxs.push(i),
                None => {
                    sh_eprintln!(self, "bish: disown: {}: no such job", s);
                    status = 1;
                }
            }
        }
        idxs.sort_unstable();
        idxs.dedup();
        let mut table = self.jobs.borrow_mut();
        for idx in idxs.into_iter().rev() {
            table.jobs.remove(idx);
        }
        status
    }

    // A job that *was* spawned pty-attached (Job::pty_master.is_some() --
    // only true for a promoted, unredirected background job) bubbles
    // ExecResult::Fg without blocking at all: see that variant's doc
    // comment for why the actual poll loop has to happen through
    // repl.rs's Shell::take_pending_fg + drive_fg_job instead of directly
    // here.
    //
    // A job real job control isolated into its own process group (Job::
    // pgid -- see its own doc comment) gets the real terminal-foregrounding
    // dance: SIGCONT it if it was Stopped, tcsetpgrp the real terminal at
    // it, wait watching for it to stop *again* (not just exit -- see
    // waitpid_untraced) rather than Job::wait's plain blocking wait
    // (which can never observe a stop), then reclaim the terminal for
    // bish either way.
    //
    // Anything else (no pty, no pgid -- a multi-stage pipeline, or a job
    // spawned before this shell ever ran under `set -m`) falls back to
    // the original plain blocking wait: scripts don't distinguish that
    // from real terminal foregrounding since they never interactively
    // signal the job via the keyboard anyway.
    fn run_fg(&mut self, args: &[String]) -> ExecResult {
        if !self.opt_monitor {
            sh_eprintln!(self, "bish: fg: no job control");
            return ExecResult::Status(1);
        }
        let idx = match args.first() {
            Some(spec) => self.resolve_job_spec(spec),
            None => self.jobs.borrow().jobs.len().checked_sub(1),
        };
        match idx {
            Some(i) => {
                let mut job = {
                    let mut table = self.jobs.borrow_mut();
                    sh_println!(self, "{}", table.jobs[i].cmd_text);
                    table.jobs.remove(i)
                };
                if job.pty_master.is_some() {
                    // Real bug, found interactively: if this job was
                    // Stopped (Ctrl-Z while it was pty-driven -- see
                    // FgJob::send_stop), it's sitting there SIGSTOP'd
                    // right now. Bubbling it to repl.rs's drive_fg_job
                    // without resuming it first means that loop just
                    // forwards keystrokes (including further Ctrl-C/
                    // Ctrl-Z) into a pty whose sole reader is frozen and
                    // can't act on any of them -- not even run a signal
                    // handler, since a stopped process runs *no* code at
                    // all until continued -- which looked like the whole
                    // shell hanging, unrecoverable short of the
                    // undiscoverable Ctrl+Space detach. This job's own
                    // pgid always equals its own (single) pid -- see
                    // Job::pgid's doc comment on why pty-attached jobs
                    // don't separately store one -- so send_signal_to_pgrp
                    // on that pid reaches the same process SIGCONT would
                    // via a real pgid.
                    if job.stopped {
                        if let Some(&pid) = job.pids.first() {
                            send_signal_to_pgrp(pid, SIGCONT);
                        }
                        job.stopped = false;
                    }
                    self.pending_fg = Some(job);
                    return ExecResult::Fg;
                }
                if let Some(pgid) = job.pgid {
                    if job.stopped {
                        send_signal_to_pgrp(pgid, SIGCONT);
                        job.stopped = false;
                    }
                    pty::tcsetpgrp(0, pgid as i32).ok();
                    let outcome = waitpid_untraced(job.pids[0]);
                    unsafe {
                        pty::tcsetpgrp(0, getpgrp()).ok();
                    }
                    return match outcome {
                        JobWaitOutcome::Exited(status) => ExecResult::Status(status),
                        JobWaitOutcome::Stopped(_sig) => {
                            job.stopped = true;
                            let id = job.id;
                            let cmd_text = job.cmd_text.clone();
                            self.jobs.borrow_mut().jobs.push(job);
                            sh_println!(self, "\n[{}]+  Stopped                 {}", id, cmd_text);
                            ExecResult::Status(148)
                        }
                    };
                }
                ExecResult::Status(job.wait())
            }
            None => {
                sh_eprintln!(self, "bish: fg: no current job");
                ExecResult::Status(1)
            }
        }
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

    // Resumes a Stopped job (Job::stopped) in place, without reclaiming
    // the real terminal for it -- SIGCONT to its process group is enough;
    // it keeps running with whatever stdio it already had (inherited from
    // the real terminal, same as any backgrounded command), it just isn't
    // the terminal's foreground process group, so it won't receive
    // further Ctrl-C/Ctrl-Z (and will itself stop again on SIGTTIN/
    // SIGTTOU if it ever tries to read from the terminal -- ordinary
    // kernel behavior, nothing this shell needs to implement).
    // A job that's *already* running (no pgid at all, or pgid but not
    // stopped) has nothing to resume -- matches real bash, confirmed:
    // `bg` on an already-running job just reports "already in background"
    // and returns 0.
    fn run_bg(&mut self, args: &[String]) -> i32 {
        if !self.opt_monitor {
            sh_eprintln!(self, "bish: bg: no job control");
            return 1;
        }
        let idx = match args.first() {
            Some(spec) => self.resolve_job_spec(spec),
            None => self.jobs.borrow().jobs.len().checked_sub(1),
        };
        match idx {
            Some(i) => {
                let mut table = self.jobs.borrow_mut();
                let job = &mut table.jobs[i];
                if job.stopped {
                    // Same fix as run_fg's own pty_master.is_some()
                    // branch: a pty-attached job doesn't store a
                    // separate pgid (see Job::pgid's doc comment), but
                    // its own pid always equals its own pgid (setsid),
                    // so send_signal_to_pgrp on that pid resumes it the
                    // same way.
                    if let Some(pid) = job.pgid.or_else(|| job.pids.first().copied()) {
                        send_signal_to_pgrp(pid, SIGCONT);
                    }
                    job.stopped = false;
                    sh_println!(self, "[{}]+  {} &", job.id, job.cmd_text);
                    return 0;
                }
                sh_eprintln!(self, "bish: bg: job {} already in background", job.id);
                0
            }
            None => {
                sh_eprintln!(self, "bish: bg: no current job");
                1
            }
        }
    }

    // `wait` with no operands waits for every active job and always
    // returns 0 (POSIX-specified, confirmed against real bash); with
    // operands, waits for just those and returns the *last* one's status.
    // Stopped jobs (Job::stopped) are skipped rather than waited on --
    // Job::wait is a plain Child::wait, which (no WUNTRACED) would just
    // block forever on a job that's merely stopped, not exited.
    fn run_wait(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            loop {
                let idx = self.jobs.borrow().jobs.iter().position(|j| !j.stopped);
                let Some(idx) = idx else { break };
                let mut job = self.jobs.borrow_mut().jobs.remove(idx);
                job.wait();
            }
            return 0;
        }
        let mut status = 0;
        for a in args {
            if let Some(idx) = self.resolve_job_spec(a) {
                if self.jobs.borrow().jobs[idx].stopped {
                    sh_eprintln!(self, "bish: wait: job {} is stopped", self.jobs.borrow().jobs[idx].id);
                    status = 127;
                    continue;
                }
                let mut job = self.jobs.borrow_mut().jobs.remove(idx);
                status = job.wait();
                continue;
            }
            match a.parse::<u32>() {
                Ok(pid) => {
                    let idx = self.jobs.borrow().jobs.iter().position(|j| j.pids.contains(&pid));
                    match idx {
                        Some(idx) => {
                            let mut job = self.jobs.borrow_mut().jobs.remove(idx);
                            status = job.wait();
                        }
                        None => {
                            sh_eprintln!(self, "bish: wait: pid {} is not a child of this shell", pid);
                            status = 127;
                        }
                    }
                }
                Err(_) => {
                    sh_eprintln!(self, "bish: wait: {}: no such job", a);
                    status = 127;
                }
            }
        }
        status
    }

    // kill [-SIGNAME|-N] pid|%job ... Negative PIDs (process-group kill)
    // aren't specially handled -- see the `jobs` field comment on why real
    // process-group management is out of scope here.
    fn run_kill(&mut self, args: &[String]) -> i32 {
        let mut sig = 15; // SIGTERM
        let mut targets: Vec<&String> = Vec::new();
        for a in args {
            if let Some(rest) = a.strip_prefix('-') {
                if rest == "l" {
                    for (name, num) in SIGNAL_NAMES {
                        sh_println!(self, "{}) SIG{}", num, name);
                    }
                    return 0;
                }
                if let Some(n) = signal_number(rest) {
                    sig = n;
                    continue;
                }
            }
            targets.push(a);
        }
        let mut status = 0;
        for t in targets {
            if let Some(idx) = t.strip_prefix('%').and_then(|_| self.resolve_job_spec(t)) {
                let pids = self.jobs.borrow().jobs[idx].pids.clone();
                for pid in pids {
                    send_signal(pid, sig);
                }
            } else if let Ok(pid) = t.parse::<i32>() {
                if !send_signal(pid as u32, sig) {
                    sh_eprintln!(self, "bish: kill: ({}) - No such process", pid);
                    status = 1;
                }
            } else {
                sh_eprintln!(self, "bish: kill: {}: arguments must be process or job IDs", t);
                status = 1;
            }
        }
        status
    }

    // getopts optstring name [args...]. Options requiring an argument are
    // marked with a trailing ':' in optstring (e.g. "ab:c"); a leading ':'
    // switches to "silent" error mode (custom handling via OPTARG/'?'/':'
    // instead of a printed message), matching bash.
    fn run_getopts(&mut self, args: &[String]) -> ExecResult {
        let optstring = args.first().cloned().unwrap_or_default();
        let varname = match args.get(1) {
            Some(v) => v.clone(),
            None => {
                sh_eprintln!(self, "bish: getopts: usage: getopts optstring name [args]");
                return ExecResult::Status(2);
            }
        };
        let positional: Vec<String> =
            if args.len() > 2 { args[2..].to_vec() } else { self.arg_frames.last().cloned().unwrap_or_default() };

        let optind: usize = self.lookup_var("OPTIND").trim().parse().unwrap_or(1);
        let idx = optind.saturating_sub(1);

        if idx >= positional.len() {
            return ExecResult::Status(1);
        }
        let cur = positional[idx].clone();
        if !cur.starts_with('-') || cur == "-" {
            return ExecResult::Status(1);
        }
        if cur == "--" {
            self.assign_var("OPTIND", (optind + 1).to_string());
            return ExecResult::Status(1);
        }

        let opt_char = cur.chars().nth(1).unwrap_or('?');
        let silent = optstring.starts_with(':');
        let spec = optstring.trim_start_matches(':');

        let Some(pos) = spec.find(opt_char) else {
            if silent {
                self.assign_var(&varname, "?".to_string());
                self.assign_var("OPTARG", opt_char.to_string());
            } else {
                sh_eprintln!(self, "bish: getopts: illegal option -- '{}'", opt_char);
                self.assign_var(&varname, "?".to_string());
            }
            self.assign_var("OPTIND", (optind + 1).to_string());
            return ExecResult::Status(0);
        };

        let needs_arg = spec.as_bytes().get(pos + 1) == Some(&b':');
        if needs_arg {
            let rest: String = cur.chars().skip(2).collect();
            if !rest.is_empty() {
                self.assign_var("OPTARG", rest);
                self.assign_var("OPTIND", (optind + 1).to_string());
            } else if idx + 1 < positional.len() {
                self.assign_var("OPTARG", positional[idx + 1].clone());
                self.assign_var("OPTIND", (optind + 2).to_string());
            } else {
                if silent {
                    self.assign_var(&varname, ":".to_string());
                    self.assign_var("OPTARG", opt_char.to_string());
                } else {
                    sh_eprintln!(self, "bish: getopts: option requires an argument -- '{}'", opt_char);
                    self.assign_var(&varname, "?".to_string());
                }
                self.assign_var("OPTIND", (optind + 1).to_string());
                return ExecResult::Status(0);
            }
        } else {
            self.assign_var("OPTIND", (optind + 1).to_string());
        }
        self.assign_var(&varname, opt_char.to_string());
        ExecResult::Status(0)
    }

    // unset [-f|-v] NAME... Also accepts `arr[i]` to remove one element
    // without touching the rest of the array. `stderr_target` mirrors real
    // bash routing this error through the command's own `2>` (confirmed via
    // a clean bash probe) -- unlike nounset/plain-assignment errors, which
    // always go to real stderr since they happen before any redirect setup.
    fn run_unset(&mut self, args: &[String], stderr_target: &Option<String>) -> i32 {
        let mut only_funcs = false;
        let mut only_vars = false;
        let mut names: Vec<&String> = Vec::new();
        for a in args {
            match a.as_str() {
                "-f" => only_funcs = true,
                "-v" => only_vars = true,
                _ => names.push(a),
            }
        }
        for n in names {
            if only_funcs {
                self.functions.remove(n.as_str());
                continue;
            }
            if let Some(bracket) = n.find('[') {
                if let Some(idx_expr) = n.strip_suffix(']').map(|s| &s[bracket + 1..]) {
                    let arr_name = n[..bracket].to_string();
                    if self.assoc_names.contains(&arr_name) {
                        let key = self.expand_index_as_string(idx_expr);
                        if let Some(map) = self.assoc_arrays.get_mut(&arr_name) {
                            map.remove(&key);
                        }
                    } else if let Ok(i) = arith::eval(idx_expr, self) {
                        if let Some(idx) = self.resolve_array_index(&arr_name, i) {
                            if let Some(map) = self.arrays.get_mut(&arr_name) {
                                map.remove(&idx);
                            }
                        }
                    }
                    continue;
                }
            }
            if self.readonly_names.contains(n.as_str()) || self.is_restricted_readonly_name(n) {
                write_diagnostic(stderr_target, &format!("bish: unset: {}: cannot unset: readonly variable", n), self.sink.clone());
                continue;
            }
            self.arrays.remove(n.as_str());
            self.assoc_arrays.remove(n.as_str());
            self.assoc_names.remove(n.as_str());
            let mut removed_local = false;
            for scope in self.var_scopes.iter_mut().rev() {
                if scope.remove(n.as_str()).is_some() {
                    removed_local = true;
                    break;
                }
            }
            if !removed_local {
                unsafe {
                    std::env::remove_var(n);
                }
            }
            if !only_vars {
                self.functions.remove(n.as_str());
            }
        }
        0
    }

    // declare/typeset [-A|-a|-i|-r|-g] [NAME|NAME=value]... `-x` isn't
    // tracked separately since every variable already lives in the
    // process env here; other real bash flags (-u/-l/-n/...) are
    // accepted but not enforced. `-p`/`-f`/`-F` are a different mode
    // entirely (print instead of declare, see print_declared/
    // print_functions) -- checked first, same as bash effectively
    // treating them as a separate subcommand.
    fn run_declare(&mut self, args: &[String], array_literals: &[(usize, String, AssignMode, Vec<ArrayLiteralItem>)]) -> i32 {
        if args.iter().any(|a| a == "-f" || a == "-F") {
            let names_only = args.iter().any(|a| a == "-F");
            let names: Vec<String> = args.iter().filter(|a| !a.starts_with('-')).cloned().collect();
            return self.print_functions(&names, names_only);
        }
        if args.iter().any(|a| a == "-p") {
            let names: Vec<String> = args.iter().filter(|a| !a.starts_with('-')).cloned().collect();
            return self.print_declared(&names);
        }
        // `-g`: force the write to the true global scope even when
        // called from inside a function -- without it, a plain
        // declare/typeset inside a function auto-localizes exactly like
        // `local` does (see the scalar assignment branch below), matching
        // real bash (confirmed: `f() { declare z=5; }; f; echo "$z"`
        // prints nothing in bash, but bish used to leak z to the global
        // scope here before this fix).
        let mut global_flag = false;
        let mut array_mode: Option<bool> = None; // Some(true)=-A, Some(false)=-a
        let mut readonly_flag = false;
        let mut integer_flag = false;
        let mut nameref_flag = false;
        let mut upper_flag = false;
        let mut lower_flag = false;
        let mut export_flag = false;
        for (i, a) in args.iter().enumerate() {
            match a.as_str() {
                "-A" => {
                    array_mode = Some(true);
                    continue;
                }
                "-a" => {
                    array_mode = Some(false);
                    continue;
                }
                "-r" => {
                    readonly_flag = true;
                    continue;
                }
                "-i" => {
                    integer_flag = true;
                    continue;
                }
                "-n" => {
                    nameref_flag = true;
                    continue;
                }
                "-u" => {
                    upper_flag = true;
                    continue;
                }
                "-l" => {
                    lower_flag = true;
                    continue;
                }
                "-x" => {
                    export_flag = true;
                    continue;
                }
                "-g" => {
                    global_flag = true;
                    continue;
                }
                _ => {}
            }
            // `declare -A m=([a]=1 [b]=2)` -- this position is actually
            // an array literal, not a plain `NAME`/`NAME=value` string
            // (`a` here is just its xtrace-only display text, see
            // array_literal_display's own doc comment). `-A`/`-a` seen
            // so far decides which table it's declared into, matching
            // the plain-name case just below; no flag at all falls back
            // to whatever `name` already is (bash's own behavior:
            // without `-A`, a bracketed key is just an arithmetic index
            // into a plain indexed array).
            if let Some((_, name, mode, items)) = array_literals.iter().find(|(pos, ..)| *pos == i) {
                match array_mode {
                    Some(true) => {
                        self.assoc_names.insert(name.clone());
                        self.assoc_arrays.entry(name.clone()).or_default();
                    }
                    Some(false) => {
                        self.arrays.entry(name.clone()).or_default();
                    }
                    None => {}
                }
                self.apply_array_literal(name, *mode, items);
                if readonly_flag {
                    self.readonly_names.insert(name.clone());
                }
                continue;
            }
            if a.starts_with('-') {
                continue;
            }
            let (name, val) = match a.find('=') {
                Some(eq) => (a[..eq].to_string(), Some(a[eq + 1..].to_string())),
                None => (a.clone(), None),
            };
            if integer_flag {
                self.integer_names.insert(name.clone());
            }
            if upper_flag {
                self.upper_names.insert(name.clone());
            }
            if lower_flag {
                self.lower_names.insert(name.clone());
            }
            if export_flag {
                self.exported_names.insert(name.clone());
            }
            if nameref_flag {
                self.nameref_names.insert(name.clone());
                if let Some(v) = val {
                    self.raw_var_write(&name, v);
                }
                if readonly_flag {
                    self.readonly_names.insert(name);
                }
                continue;
            }
            match array_mode {
                Some(true) => {
                    self.assoc_names.insert(name.clone());
                    self.assoc_arrays.entry(name.clone()).or_default();
                }
                Some(false) => {
                    self.arrays.entry(name.clone()).or_default();
                }
                None => {
                    // Auto-localize, matching `local`: a plain (non-`-g`)
                    // declare/typeset inside a function creates a new
                    // local shadow rather than falling through to the
                    // global env. Pre-inserting an (empty, for now) entry
                    // into the current scope makes assign_var's own
                    // existing "write into whichever scope already
                    // shadows this name" logic (raw_var_write) do the
                    // right thing without needing a separate write path.
                    if !global_flag && !self.var_scopes.is_empty() {
                        self.var_scopes.last_mut().unwrap().entry(name.clone()).or_default();
                    }
                    if let Some(v) = val {
                        if global_flag { self.assign_var_global(&name, v) } else { self.assign_var(&name, v) }
                    } else if export_flag {
                        // Bare `declare -x NAME`/`export NAME` on an
                        // already-set variable (commonly a local: `local
                        // Z=inner; export Z`) -- re-assign its current
                        // value through assign_var so exported_names'
                        // mirror-to-env logic fires immediately, instead
                        // of only on the variable's *next* write. The
                        // empty-fallback branch below wouldn't reach this
                        // case since it only fires for a name with no
                        // value at all yet.
                        let cur = self.lookup_var(&name);
                        if global_flag { self.assign_var_global(&name, cur) } else { self.assign_var(&name, cur) }
                    } else if self.lookup_var(&name).is_empty() && std::env::var(&name).is_err() {
                        if global_flag {
                            self.assign_var_global(&name, String::new())
                        } else {
                            self.assign_var(&name, String::new())
                        }
                    }
                }
            }
            if readonly_flag {
                self.readonly_names.insert(name);
            }
        }
        0
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
    fn print_functions(&mut self, names: &[String], names_only: bool) -> i32 {
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
                        sh_println!(self, "declare -f {}", name);
                    } else {
                        let def = parser::Command::FuncDef { name: name.clone(), body: Box::new(body) };
                        let src = crate::serialize::serialize_program(&[ListItem {
                            and_or: AndOr { first: Pipeline { commands: vec![def], negate: false }, rest: Vec::new() },
                            sep: Sep::Seq,
                            line: 0,
                        }]);
                        sh_println!(self, "{}", src.trim_end());
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
    fn print_declared(&mut self, names: &[String]) -> i32 {
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
        } else if self.arrays.contains_key(name) {
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
        let is_array = self.assoc_names.contains(name) || self.arrays.contains_key(name);
        if is_array && single_element.is_none() {
            return self.declare_p_line(name).unwrap_or_default();
        }
        let flags = self.attribute_flags_string(name);
        let value = match single_element {
            Some(v) => v.to_string(),
            None => self.lookup_var(name),
        };
        let quoted = crate::serialize::quote_literal(&value);
        if flags.is_empty() {
            format!("{name}={quoted}")
        } else {
            format!("declare -{flags} {name}={quoted}")
        }
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
                'd' => out.push_str(&prompt_date()),
                't' => out.push_str(&strftime("%H:%M:%S", &local_time_now())),
                'T' => out.push_str(&strftime("%I:%M:%S", &local_time_now())),
                '@' => out.push_str(&strftime("%I:%M %p", &local_time_now())),
                'A' => out.push_str(&strftime("%H:%M", &local_time_now())),
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
                    out.push_str(&strftime(fmt, &local_time_now()));
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

        if self.assoc_names.contains(name) {
            let map = self.assoc_arrays.get(name)?;
            let mut body = String::new();
            for (k, v) in map.iter() {
                body.push('[');
                body.push_str(k);
                body.push_str("]=");
                body.push_str(&declare_p_quote(v));
                body.push(' ');
            }
            return Some(format!("declare {} {}=({})", flag_str, name, body.trim_end()));
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
            return None;
        }
        let value = self.lookup_var(name);
        Some(format!("declare {} {}={}", flag_str, name, declare_p_quote(&value)))
    }

    // readonly NAME[=value]... Marks each name so assign_var refuses future
    // writes. The initializing assignment (if any) happens before the name
    // is added to readonly_names, so it isn't rejected by its own call.
    fn run_readonly(&mut self, args: &[String]) -> i32 {
        for a in args {
            if a.starts_with('-') {
                continue;
            }
            let (name, val) = match a.find('=') {
                Some(eq) => (a[..eq].to_string(), Some(a[eq + 1..].to_string())),
                None => (a.clone(), None),
            };
            if let Some(v) = val {
                self.assign_var(&name, v);
            }
            self.readonly_names.insert(name);
        }
        0
    }

    // Effective on/off state for a known shopt option name: an explicit
    // `-s`/`-u` override if there's been one this session, else that
    // name's own default from KNOWN_SHOPT_OPTIONS. `extglob` is special-
    // cased to always report "on" regardless of either, since bish's
    // extglob support is unconditional (see glob.rs) rather than actually
    // gated by this flag.
    fn shopt_is_on(&self, name: &str) -> bool {
        if name == "extglob" {
            return true;
        }
        self.shopt_options.get(name).copied().unwrap_or_else(|| shopt_default_on(name).unwrap_or(false))
    }

    fn print_shopt_line(&mut self, name: &str, reusable: bool) {
        let on = self.shopt_is_on(name);
        if reusable {
            sh_println!(self, "shopt -{} {}", if on { "s" } else { "u" }, name);
        } else {
            sh_println!(self, "{:<15}\t{}", name, if on { "on" } else { "off" });
        }
    }

    // shopt [-su] [-q] [-p] [NAME ...]. Bare `shopt` lists every known
    // option's on/off state; `shopt -s`/`shopt -u` alone list only the
    // ones currently on/off (respectively); either with NAMEs given
    // toggles just those. `-p` prints in the same `shopt -s/-u NAME` form
    // that can be fed back in, instead of the plain "NAME\ton/off" table.
    // A NAME not in KNOWN_SHOPT_OPTIONS is rejected up front, matching
    // real bash's own "invalid shell option name" error -- see that
    // list's own doc comment for what most of these names actually do (or
    // don't do) in bish.
    fn run_shopt(&mut self, args: &[String]) -> i32 {
        let mut mode: Option<bool> = None; // Some(true)=-s, Some(false)=-u
        let mut quiet = false;
        let mut reusable = false;
        let mut names: Vec<&str> = Vec::new();
        for a in args {
            match a.as_str() {
                "-s" => mode = Some(true),
                "-u" => mode = Some(false),
                "-q" => quiet = true,
                "-p" => reusable = true,
                _ if a.starts_with('-') => {}
                other => names.push(other),
            }
        }
        for n in &names {
            if shopt_default_on(n).is_none() {
                sh_eprintln!(self, "bish: shopt: {n}: invalid shell option name");
                return 1;
            }
        }
        match mode {
            Some(on) if names.is_empty() => {
                let matching: Vec<&str> = KNOWN_SHOPT_OPTIONS.iter().map(|(n, _)| *n).filter(|n| self.shopt_is_on(n) == on).collect();
                for n in matching {
                    self.print_shopt_line(n, reusable);
                }
                0
            }
            Some(on) => {
                for n in &names {
                    self.shopt_options.insert(n.to_string(), on);
                }
                0
            }
            None if quiet => {
                if names.iter().all(|n| self.shopt_is_on(n)) {
                    0
                } else {
                    1
                }
            }
            None => {
                let targets: Vec<&str> = if names.is_empty() { KNOWN_SHOPT_OPTIONS.iter().map(|(n, _)| *n).collect() } else { names };
                for n in targets {
                    self.print_shopt_line(n, reusable);
                }
                0
            }
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
    fn bishopt_value(&self, registry: &[(&str, BishOptDefault)], name: &str) -> Option<BishOptValue> {
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
                let c = crate::csscolor::parse_terminal_list(s).unwrap_or_else(|e| panic!("KNOWN_BISHOPTS: {name}: default color {s:?} doesn't parse: {e}"));
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
    fn store_hl(&mut self, name: &str, value: String) {
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

    fn store_bishopt(&mut self, name: &str, value: BishOptValue) {
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

    fn describe_bishopts(&self, registry: &[(&str, BishOptDefault)], which: Option<&str>) -> Vec<String> {
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

    fn run_bishopt(&mut self, args: &[String], registry: &[(&str, BishOptDefault)]) -> i32 {
        enum Mode<'a> {
            List,
            Get(&'a str, bool), // bool: quiet
            // `--describe [NAME]`: what an option is for, what it
            // accepts, and what it is set to. Everything `bishopt` could
            // already tell you was the *value*; this is the half that
            // makes an option findable rather than merely settable.
            Describe(Option<&'a str>),
            Set(&'a str, Option<&'a str>),
            Unset(&'a str),
        }
        let mode = match args {
            [] => Mode::List,
            [flag] if flag == "--describe" || flag == "-d" => Mode::Describe(None),
            [flag, name] if flag == "--describe" || flag == "-d" => Mode::Describe(Some(name.as_str())),
            [flag, name] if flag == "--set" || flag == "-s" => Mode::Set(name, None),
            [flag, name, value] if flag == "--set" || flag == "-s" => Mode::Set(name, Some(value)),
            [flag, name] if flag == "--unset" || flag == "-u" => Mode::Unset(name),
            [flag, name] if flag == "--quiet" || flag == "-q" => Mode::Get(name, true),
            [name] => Mode::Get(name, false),
            _ => {
                sh_eprintln!(self, "bish: bishopt: usage: bishopt [--quiet|-q NAME | --set|-s NAME [VALUE] | --unset|-u NAME | --describe|-d [NAME] | NAME]");
                return 2;
            }
        };
        match mode {
            Mode::List => {
                for (name, _) in registry {
                    sh_println!(self, "{name}");
                }
                0
            }
            Mode::Describe(which) => {
                if let Some(name) = which
                    && !registry.iter().any(|(n, _)| *n == name)
                {
                    sh_eprintln!(self, "bish: bishopt: unknown option '{name}'");
                    return 1;
                }
                for line in self.describe_bishopts(registry, which) {
                    sh_println!(self, "{line}");
                }
                0
            }
            Mode::Get(name, quiet) => match self.bishopt_value(registry, name) {
                Some(BishOptValue::Bool(on)) => {
                    if !quiet {
                        sh_println!(self, "{}", if on { "on" } else { "off" });
                    }
                    if on {
                        0
                    } else {
                        1
                    }
                }
                Some(BishOptValue::Int(n)) => {
                    if !quiet {
                        sh_println!(self, "{n}");
                    }
                    0
                }
                Some(BishOptValue::Str(s)) => {
                    if !quiet {
                        sh_println!(self, "{s}");
                    }
                    0
                }
                Some(BishOptValue::Color(text, _)) => {
                    if !quiet {
                        sh_println!(self, "{text}");
                    }
                    0
                }
                None => {
                    sh_eprintln!(self, "bish: bishopt: {name}: no such option");
                    1
                }
            },
            Mode::Set(name, value) => match (registry.iter().find(|(n, _)| *n == name).map(|(_, d)| d.clone()), value) {
                (None, _) => {
                    sh_eprintln!(self, "bish: bishopt: {name}: no such option");
                    1
                }
                (Some(BishOptDefault::Bool(_)), None | Some("on")) => {
                    self.store_bishopt(name, BishOptValue::Bool(true));
                    0
                }
                (Some(BishOptDefault::Bool(_)), Some("off")) => {
                    self.store_bishopt(name, BishOptValue::Bool(false));
                    0
                }
                (Some(BishOptDefault::Bool(_)), Some(_)) => {
                    sh_eprintln!(self, "bish: bishopt: --set: {name}: a boolean option only accepts 'on' or 'off'");
                    2
                }
                (Some(BishOptDefault::Int(_, _)), None) => {
                    sh_eprintln!(self, "bish: bishopt: --set: {name}: requires a VALUE");
                    2
                }
                (Some(BishOptDefault::Int(_, range)), Some(v)) => match v.parse::<i64>() {
                    Ok(n) if range.contains(&n) => {
                        self.store_bishopt(name, BishOptValue::Int(n));
                        0
                    }
                    Ok(n) => {
                        sh_eprintln!(self, "bish: bishopt: --set: {name}: {n} is outside {}..{}", range.start(), range.end());
                        2
                    }
                    Err(_) => {
                        sh_eprintln!(self, "bish: bishopt: --set: {name}: {v:?} is not a whole number");
                        2
                    }
                },
                (Some(BishOptDefault::Str(_)), None) => {
                    sh_eprintln!(self, "bish: bishopt: --set: {name}: requires a VALUE");
                    2
                }
                (Some(BishOptDefault::Str(_)), Some(v)) => {
                    self.store_bishopt(name, BishOptValue::Str(v.to_string()));
                    0
                }
                (Some(BishOptDefault::Color(_)), None) => {
                    sh_eprintln!(self, "bish: bishopt: --set: {name}: requires a VALUE");
                    2
                }
                (Some(BishOptDefault::Color(_)), Some(v)) => match crate::csscolor::parse_terminal_list(v) {
                    Ok(c) => {
                        self.store_bishopt(name, BishOptValue::Color(v.to_string(), c));
                        0
                    }
                    Err(e) => {
                        sh_eprintln!(self, "bish: bishopt: --set: {name}: invalid color '{v}': {e}");
                        2
                    }
                },
            },
            Mode::Unset(name) => {
                if !registry.iter().any(|(n, _)| *n == name) {
                    sh_eprintln!(self, "bish: bishopt: {name}: no such option");
                    return 1;
                }
                self.bishopts.remove(name);
                0
            }
        }
    }

    // `::bish SUBCOMMAND...`: a small namespace of its own for bish-
    // specific commands (`theme begin`/`theme end` today) that don't
    // read naturally as an ordinary top-level builtin name -- `theme` on
    // its own would either collide with a real bash script's own
    // variable/function of that name, or need its own awkward "begin"/
    // "end" builtins polluting the global command namespace for
    // something this narrow. `::` is never a valid start of an ordinary
    // bash command word in practice, so `::bish` reads unambiguously as
    // "this is bish's own thing," the same spirit as `set -o` bundling
    // bash's own less-common toggles under one name instead of each
    // getting its own builtin.
    fn run_bish(&mut self, args: &[String]) -> ExecResult {

        match args {
            [sub, rest @ ..] if sub == "theme" => ExecResult::Status(self.run_bish_theme(rest)),
            // The canonical spelling of the window manager. `window`/
            // `win` survive only as *command-mode* aliases (see
            // run_single's own arm): a top-level builtin called `window`
            // shadows any real `window` on `$PATH` for every script that
            // runs under bish, and this namespace exists precisely for
            // bish-specific commands that shouldn't spend a common word.
            [sub, rest @ ..] if sub == "window" || sub == "win" => self.run_window(rest),
            [sub, rest @ ..] if sub == "hook" => ExecResult::Status(self.run_hook(rest)),
            [sub, rest @ ..] if sub == "hl" => ExecResult::Status(self.run_hl(rest)),
            [sub, rest @ ..] if sub == "lsp" => ExecResult::Status(self.run_lsp(rest)),
            [] => {
                sh_eprintln!(self, "bish: ::bish: missing subcommand (expected: theme, window, hook, hl, lsp)");
                ExecResult::Status(2)
            }
            [other, ..] => {
                sh_eprintln!(self, "bish: ::bish: unknown subcommand '{other}' (expected: theme, window, hook, hl, lsp)");
                ExecResult::Status(2)
            }
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
    fn hook_lang_flag<'a>(&mut self, subcommand: &str, args: &'a [String]) -> Result<(Option<String>, &'a [String]), i32> {
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

    fn run_hook(&mut self, args: &[String]) -> i32 {
        match args.first().map(String::as_str) {
            Some("ls") | Some("list") | None => {
                let lang = match self.hook_lang_flag("ls", &args[1.min(args.len())..]) {
                    Ok((lang, [])) => lang,
                    Ok(_) => {
                        sh_eprintln!(self, "bish: ::bish hook: ls: usage: ::bish hook ls [--lang=GLOB]");
                        return 2;
                    }
                    Err(status) => return status,
                };
                for hook in self.hooks.clone() {
                    // Listing by language asks "what would fire for a
                    // file of this language", so it matches the *glob*
                    // against the language given, exactly as firing
                    // does -- not the two globs against each other.
                    if let Some(lang) = lang.as_deref()
                        && !crate::glob::matches(&hook.lang, lang)
                    {
                        continue;
                    }
                    sh_println!(self, "{}\t{}\t{}\t{}", hook.id, hook.event, hook.lang, hook.command);
                }
                0
            }
            Some("add") => {
                let (lang, rest) = match self.hook_lang_flag("add", &args[1..]) {
                    Ok(parsed) => parsed,
                    Err(status) => return status,
                };
                let [event, command @ ..] = rest else {
                    sh_eprintln!(self, "bish: ::bish hook: add: usage: ::bish hook add [--lang=GLOB] EVENT COMMAND...");
                    return 2;
                };
                if !HOOK_EVENTS.contains(&event.as_str()) {
                    sh_eprintln!(self, "bish: ::bish hook: add: unknown event '{event}' (try `::bish hook help`)");
                    return 2;
                }
                if command.is_empty() {
                    sh_eprintln!(self, "bish: ::bish hook: add: no command given");
                    return 2;
                }
                let id = self.next_hook_id;
                self.next_hook_id += 1;
                self.hooks.push(Hook {
                    id,
                    event: event.clone(),
                    lang: lang.unwrap_or_else(|| "*".to_string()),
                    command: command.join(" "),
                });
                // The id is the return value: a config that adds a hook
                // is usually the thing that will want to remove it.
                sh_println!(self, "{id}");
                0
            }
            Some("rm") | Some("remove") => {
                let Some(id) = args.get(1).and_then(|a| a.parse::<u64>().ok()) else {
                    sh_eprintln!(self, "bish: ::bish hook: rm: usage: ::bish hook rm <id>");
                    return 2;
                };
                let before = self.hooks.len();
                self.hooks.retain(|h| h.id != id);
                if self.hooks.len() == before {
                    sh_eprintln!(self, "bish: ::bish hook: rm: no hook with id {id}");
                    return 1;
                }
                0
            }
            Some("help") | Some("--help") | Some("-h") | Some("events") => {
                for line in hook_help() {
                    sh_println!(self, "{line}");
                }
                0
            }
            Some(other) => {
                sh_eprintln!(self, "bish: ::bish hook: unknown subcommand '{other}' (expected: ls, add, rm, help)");
                2
            }
        }
    }

    // `::bish lsp ls|add|rm|status` -- which language servers exist and
    // which are running. Deliberately the same shape as `::bish hook`
    // right above: a per-shell counter for ids, `--lang` as a glob, `rm`
    // by the id `add` printed. Two registries that worked differently
    // would be two things to learn.
    //
    // Canonical under `::bish` rather than a bare `lsp` builtin, for the
    // reason `window` was moved there: this namespace exists so
    // bish-specific commands don't shadow real ones in scripts.
    fn run_lsp(&mut self, args: &[String]) -> i32 {
        match args.first().map(String::as_str) {
            Some("ls") | Some("list") | None => {
                let rest = &args[1.min(args.len())..];
                let lang = match self.lsp_lang_flag("ls", rest) {
                    Ok((lang, [])) => lang,
                    Ok(_) => {
                        sh_eprintln!(self, "bish: ::bish lsp: ls: usage: ::bish lsp ls [--lang=GLOB]");
                        return 2;
                    }
                    Err(status) => return status,
                };
                for server in self.lsp_servers.clone() {
                    // Same question `hook ls --lang` answers: "what
                    // would be used for a file of this language" -- so
                    // the glob is matched against the language given,
                    // not the two globs against each other.
                    if let Some(lang) = lang.as_deref()
                        && !crate::glob::matches(&server.lang, lang)
                    {
                        continue;
                    }
                    let root = if server.root_cmd.is_empty() { server.root_markers.join(",") } else { server.root_cmd.clone() };
                    sh_println!(self, "{}\t{}\t{}\t{}", server.id, server.lang, root, server.command_line());
                }
                0
            }
            Some("add") => {
                // Round-robin rather than a fixed order: with four
                // flags, an order that only reads correctly one way
                // means `--apply-edits=always --lang=rust rust-analyzer`
                // silently tries to *run* `--apply-edits=always`. Each
                // helper leaves the slice alone when its flag isn't at
                // the front, so a pass that consumes nothing is the
                // signal that what remains is the command.
                let mut rest = &args[1..];
                let mut lang = None;
                let mut root_markers = None;
                let mut root_cmd = String::new();
                let mut apply_edits = "scoped".to_string();
                loop {
                    let before = rest.len();
                    match self.lsp_lang_flag("add", rest) {
                        Ok((found, after)) => {
                            if found.is_some() {
                                lang = found;
                            }
                            rest = after;
                        }
                        Err(status) => return status,
                    }
                    let mark = rest.len();
                    match self.lsp_root_flag(rest) {
                        Ok((found, after)) => {
                            if after.len() != mark {
                                root_markers = Some(found);
                            }
                            rest = after;
                        }
                        Err(status) => return status,
                    }
                    match self.lsp_root_cmd_flag(rest) {
                        Ok((found, after)) => {
                            if !found.is_empty() {
                                root_cmd = found;
                            }
                            rest = after;
                        }
                        Err(status) => return status,
                    }
                    let mark = rest.len();
                    match self.lsp_apply_edits_flag(rest) {
                        Ok((found, after)) => {
                            if after.len() != mark {
                                apply_edits = found;
                            }
                            rest = after;
                        }
                        Err(status) => return status,
                    }
                    if rest.len() == before {
                        break;
                    }
                }
                let root_markers = root_markers.unwrap_or_else(|| vec![".git".to_string()]);
                if rest.is_empty() {
                    sh_eprintln!(self, "bish: ::bish lsp: add: usage: ::bish lsp add [--lang=GLOB] [--root=NAME,...] COMMAND...");
                    return 2;
                }
                let id = self.next_lsp_id;
                self.next_lsp_id += 1;
                self.lsp_servers.push(LspServer {
                    id,
                    lang: lang.unwrap_or_else(|| "*".to_string()),
                    command: rest.to_vec(),
                    root_markers,
                    root_cmd,
                    apply_edits,
                });
                // The id is the return value, same as `hook add`: a
                // config that registers something usually wants to be
                // able to take it back.
                sh_println!(self, "{id}");
                0
            }
            Some("rm") | Some("remove") => {
                let Some(id) = args.get(1).and_then(|a| a.parse::<u64>().ok()) else {
                    sh_eprintln!(self, "bish: ::bish lsp: rm: usage: ::bish lsp rm <id>");
                    return 2;
                };
                let before = self.lsp_servers.len();
                self.lsp_servers.retain(|s| s.id != id);
                if self.lsp_servers.len() == before {
                    sh_eprintln!(self, "bish: ::bish lsp: rm: no language server with id {id}");
                    return 1;
                }
                0
            }
            Some("status") => {
                // Collected before printing: `sh_println!` needs the
                // shell mutably, and the table is reached through it.
                let rows: Vec<String> = self.lsp.borrow().rows().iter().map(|fields| fields.join("\t")).collect();
                for row in rows {
                    sh_println!(self, "{row}");
                }
                0
            }
            Some("log") => {
                // The whole reason a server's stderr is captured rather
                // than discarded: when one fails to start, what it said
                // on the way out is the only explanation there is.
                let Some(id) = args.get(1).and_then(|a| a.parse::<u64>().ok()) else {
                    sh_eprintln!(self, "bish: ::bish lsp: log: usage: ::bish lsp log <id>");
                    return 2;
                };
                let lines = self.lsp.borrow().logs(id);
                if lines.is_empty() {
                    sh_eprintln!(self, "bish: ::bish lsp: log: nothing recorded for id {id}");
                    return 1;
                }
                for line in lines {
                    sh_println!(self, "{line}");
                }
                0
            }
            Some("restart") => {
                // A server that died, or never started, stays that way
                // on purpose -- retrying on every file open would turn
                // one bad line of config into a spawn per keystroke of
                // navigation. This is how someone who has *fixed* that
                // line says so.
                let Some(id) = args.get(1).and_then(|a| a.parse::<u64>().ok()) else {
                    sh_eprintln!(self, "bish: ::bish lsp: restart: usage: ::bish lsp restart <id>");
                    return 2;
                };
                let dropped = self.lsp.borrow_mut().forget(id);
                if dropped == 0 {
                    sh_eprintln!(self, "bish: ::bish lsp: restart: nothing running or failed for id {id}");
                    return 1;
                }
                0
            }
            Some("help") | Some("--help") | Some("-h") => {
                for line in lsp_help() {
                    sh_println!(self, "{line}");
                }
                0
            }
            Some(other) => {
                sh_eprintln!(self, "bish: ::bish lsp: unknown subcommand '{other}' (expected: ls, add, rm, status, log, restart, help)");
                2
            }
        }
    }

    // `--root-cmd=COMMAND`/`--root-cmd COMMAND`. Its own helper rather
    // than folding into `lsp_root_flag`, so `--root` and `--root-cmd`
    // can be given in either order and neither is positional.
    fn lsp_root_cmd_flag<'a>(&mut self, args: &'a [String]) -> Result<(String, &'a [String]), i32> {
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

    // `--apply-edits=scoped|never|always`, defaulting to `scoped`.
    fn lsp_apply_edits_flag<'a>(&mut self, args: &'a [String]) -> Result<(String, &'a [String]), i32> {
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
    fn lsp_lang_flag<'a>(&mut self, subcommand: &str, args: &'a [String]) -> Result<(Option<String>, &'a [String]), i32> {
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
    fn lsp_root_flag<'a>(&mut self, args: &'a [String]) -> Result<(Vec<String>, &'a [String]), i32> {
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
        self.hooks
            .iter()
            .filter(|h| h.event == event && crate::glob::matches(&h.lang, language))
            .map(|h| h.command.clone())
            .collect()
    }

    /// Brackets a run of hooks, so anything they do can't fire more.
    pub fn set_firing_hooks(&mut self, firing: bool) {
        self.firing_hooks = firing;
    }

    // `::bish hl` -- the syntax-highlighting palette.
    //
    // Shaped like `bishopt` (`--set`, `--unset`, a bare name to read,
    // nothing to list) because it does the same job, and two commands
    // that behave differently for no reason are two things to learn.
    // It is a *separate* command because the names are open: bishopt is
    // a closed registry with a default and a description for each
    // entry, and a highlight colour cannot be, since a language
    // server's semantic token types are not knowable in advance.
    //
    // Only colours. The chrome colours (`ui_col_*`) stay bishopts --
    // those really are a fixed set of things bish draws.
    fn run_hl(&mut self, args: &[String]) -> i32 {
        match args {
            [] => {
                for (name, value) in self.hl_colors() {
                    sh_println!(self, "{name}\t{value}");
                }
                0
            }
            [flag, name] if flag == "--unset" || flag == "-u" => {
                // Live state even mid-declaration, exactly as
                // `bishopt --unset` is: unsetting is not declaring.
                if self.hl.remove(name.as_str()).is_none() {
                    sh_eprintln!(self, "bish: ::bish hl: {name} is not set");
                    return 1;
                }
                0
            }
            [flag, name, value] if flag == "--set" || flag == "-s" => {
                if let Err(e) = crate::csscolor::parse_terminal_list(value) {
                    sh_eprintln!(self, "bish: ::bish hl: {name}: {e}");
                    return 2;
                }
                self.store_hl(name, value.clone());
                0
            }
            [name] if !name.starts_with('-') => {
                match self.hl_colors().into_iter().find(|(n, _)| n == name) {
                    Some((_, value)) => {
                        sh_println!(self, "{value}");
                        0
                    }
                    // Nothing said about this name, which is not an
                    // error: an open namespace has no unknown names,
                    // only unset ones.
                    None => 1,
                }
            }
            _ => {
                sh_eprintln!(self, "bish: ::bish hl: usage: ::bish hl [NAME | --set|-s NAME COLOUR | --unset|-u NAME]");
                2
            }
        }
    }

    fn run_bish_theme(&mut self, args: &[String]) -> i32 {
        match args {
            [sub] if sub == "begin" => self.run_bish_theme_begin(),
            [sub] if sub == "end" => self.run_bish_theme_end(),
            [] => {
                sh_eprintln!(self, "bish: ::bish theme: missing subcommand (expected: begin, end)");
                2
            }
            [other, ..] => {
                sh_eprintln!(self, "bish: ::bish theme: unknown subcommand '{other}' (expected: begin, end)");
                2
            }
        }
    }

    // Starts a new theme declaration -- every `bishopt --set` from here
    // until the matching `::bish theme end` is captured into
    // `pending_theme` instead of applying live (see store_bishopt's own
    // doc comment). Refuses to nest: a `begin` while one is already in
    // progress would otherwise silently discard whatever the outer one
    // had captured so far the moment `end` ran, with no way back --
    // there's no real use for nesting this anyway (a theme is a flat set
    // of opts, not something that composes from an inner declaration).
    fn run_bish_theme_begin(&mut self) -> i32 {
        if self.pending_theme.is_some() {
            sh_eprintln!(self, "bish: ::bish theme: a theme declaration is already in progress -- `::bish theme end` it first");
            return 1;
        }
        self.pending_theme = Some(Theme::default());
        0
    }

    // Ends the current theme declaration. The captured "theme" entry (if
    // any -- set via an ordinary `bishopt --set theme NAME` *inside* the
    // declaration, which store_bishopt diverted here instead of applying
    // live) names which entry of `self.themes` the rest of the captured
    // opts get registered under; it's removed from that captured map
    // first so a theme's own opts never include a "theme" entry pointing
    // at itself. If "theme" was never set during the declaration, there's
    // no name to register anything under -- the whole batch is just
    // discarded, matching "theme behaves unset until explicitly declared
    // inside a theme declaration" (declaring opts with no name doesn't
    // retroactively give them one). Registering a theme here never
    // switches to it -- that still needs its own ordinary `bishopt --set
    // theme NAME` afterward, outside any declaration, the same way
    // defining a theme and activating one are two separate, deliberate
    // steps.
    fn run_bish_theme_end(&mut self) -> i32 {
        let Some(mut pending) = self.pending_theme.take() else {
            sh_eprintln!(self, "bish: ::bish theme: no theme declaration in progress");
            return 1;
        };
        let Some(BishOptValue::Str(name)) = pending.opts.remove("theme") else {
            return 0;
        };
        self.themes.insert(name, pending);
        0
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
        let mut names: std::collections::BTreeSet<String> = std::env::vars().map(|(k, _)| k).collect();
        for scope in &self.var_scopes {
            names.extend(scope.keys().cloned());
        }
        names.extend(self.arrays.keys().cloned());
        names.extend(self.assoc_arrays.keys().cloned());
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
        let mut variables: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
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
    fn report_compgen_parse_error(&mut self, who: &str, err: &compgen::ParseError) -> i32 {
        match err {
            compgen::ParseError::UnknownAction(name) => sh_eprintln!(self, "bish: {who}: {name}: invalid action name"),
            compgen::ParseError::UnknownOption(c) => sh_eprintln!(self, "bish: {who}: -{c}: invalid option"),
            compgen::ParseError::UnknownOptName(name) => sh_eprintln!(self, "bish: {who}: {name}: invalid option name"),
            compgen::ParseError::MissingArg(flag) => sh_eprintln!(self, "bish: {who}: {flag}: option requires an argument"),
        }
        2
    }

    // compgen [-V varname] [-abcdefgjksuv] [-o option] [-A action]
    // [-G globpat] [-W wordlist] [-F function] [-C command] [-X filterpat]
    // [-P prefix] [-S suffix] [--] [word] -- bash's own completion-
    // generator builtin, built on compgen.rs's shared spec parser/resolver
    // (see that module's own doc comment for the design and every
    // reverse-engineered semantic detail: which sources get filtered by
    // `word` and which don't, the exact -F/-C calling convention, `-o`'s
    // names being validated but otherwise inert).
    //
    // `-V varname` is compgen's own option (not part of the shared spec
    // grammar, since `complete` has no equivalent) -- stripped out before
    // handing the rest to compgen::parse_spec_args.
    //
    // Exit status is success unless a source was actually requested and
    // it produced nothing
    // (confirmed against real bash: bare `compgen`/a lone trailing word
    // with no -A/-G/-W/-F/-C at all always exits 0 even though it prints
    // nothing, but e.g. `compgen -W "" -- x` -- a real source, zero
    // matches -- exits 1). Applies the same way whether or not -V
    // redirected the output into an array.
    fn run_compgen(&mut self, args: &[String]) -> i32 {
        let mut varname: Option<String> = None;
        let mut rest: Vec<String> = Vec::new();
        let mut idx = 0;
        while idx < args.len() {
            if args[idx] == "-V" {
                let Some(v) = args.get(idx + 1) else {
                    sh_eprintln!(self, "bish: compgen: -V: option requires an argument");
                    return 2;
                };
                varname = Some(v.clone());
                idx += 2;
            } else {
                rest.push(args[idx].clone());
                idx += 1;
            }
        }
        let (spec, positionals) = match compgen::parse_spec_args(&rest) {
            Ok(v) => v,
            Err(e) => return self.report_compgen_parse_error("compgen", &e),
        };
        let word = positionals.last().cloned().unwrap_or_default();
        let had_source = spec.has_any_source();
        // A nicer, specific diagnostic for the overwhelmingly common
        // mistake (a typo'd function name) than the silent "just no
        // candidates" compgen::run_external falls back to for any
        // subprocess failure -- that tolerance exists for the interactive
        // Tab-completion path, where a hard error would be disruptive, but
        // this standalone builtin can and should still say what went
        // wrong (confirmed against real bash: `compgen -F nosuchfunc`
        // prints "function not found" and exits 1).
        if let Some(name) = &spec.function
            && !self.functions.contains_key(name)
        {
            sh_eprintln!(self, "bish: compgen: {name}: function not found");
            return 1;
        }
        let ctx = self.action_context();
        let preamble = self.functions_preamble();
        let candidates = compgen::resolve_spec(&spec, &word, &ctx, &self.cwd, &preamble);

        let empty = candidates.is_empty();
        if let Some(var) = varname {
            self.assoc_names.remove(&var);
            self.arrays.insert(var, candidates.into_iter().enumerate().collect());
        } else {
            for c in &candidates {
                sh_println!(self, "{c}");
            }
        }
        if empty && had_source {
            1
        } else {
            0
        }
    }

    // complete [-p|-r] [options] name... | complete -D [options] --
    // registers/lists/removes the completion specs bish's own interactive
    // Tab completion consults (see ShellCompletionProvider's own doc
    // comment on how) for a given command name, or the `-D` default spec
    // used when no exact name matches. Shares its entire option grammar
    // with `compgen` (compgen.rs's parse_spec_args) -- only `-p`/`-r`/`-D`
    // themselves, and taking one-or-more trailing NAMEs instead of a
    // single trailing word, are complete's own.
    //
    // `-p`/`-r` are detected as the very first argument (confirmed against
    // real bash: always used alone, never mixed with the rest of the
    // option grammar in practice) and take every remaining argument as a
    // literal NAME to print/remove -- no names at all means "every
    // registered spec" for both. The literal name "-D" in either list
    // targets the default spec instead of a real command name (confirmed:
    // `complete -p -D` prints the default spec's own line).
    //
    // Registration always fully replaces whatever spec a name already had
    // (confirmed: re-registering `cmd1` with just `-W x` drops its
    // previous -X/-P/-S/-o entirely) -- a plain HashMap::insert overwrite,
    // never a merge.
    fn run_complete(&mut self, args: &[String]) -> i32 {
        match args.first().map(String::as_str) {
            None => {
                self.print_all_completions();
                return 0;
            }
            Some("-p") => return self.print_completions(&args[1..]),
            Some("-r") => return self.remove_completions(&args[1..]),
            _ => {}
        }
        let is_default = args.iter().any(|a| a == "-D");
        let filtered: Vec<String> = args.iter().filter(|a| *a != "-D").cloned().collect();
        let (spec, names) = match compgen::parse_spec_args(&filtered) {
            Ok(v) => v,
            Err(e) => return self.report_compgen_parse_error("complete", &e),
        };
        if is_default {
            self.default_completion = Some(spec);
            return 0;
        }
        if names.is_empty() {
            sh_eprintln!(self, "bish: complete: usage: complete [-p|-r] [name ...] | complete -D [options] | complete [options] name [name ...]");
            return 2;
        }
        for name in names {
            self.completions.insert(name, spec.clone());
        }
        0
    }

    fn print_all_completions(&mut self) {
        let mut names: Vec<&String> = self.completions.keys().collect();
        names.sort();
        for name in names {
            sh_println!(self, "{}", compgen::format_spec(&self.completions[name], name));
        }
        if let Some(default) = &self.default_completion {
            sh_println!(self, "{}", compgen::format_spec(default, "-D"));
        }
    }

    fn print_completions(&mut self, names: &[String]) -> i32 {
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

    fn remove_completions(&mut self, names: &[String]) -> i32 {
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

    // compopt [-o option] [+o option] [name] -- adjusts a registered
    // spec's own stored `-o` list in place (add for `-o`, remove for
    // `+o`), leaving every other field untouched. Real bash's own compopt
    // with no `name` at all only makes sense called from inside an
    // in-progress completion function (adjusting *that* completion's
    // options); bish's completion generation never calls back into a
    // running compopt invocation that way, so -- matching real bash's own
    // behavior outside that context -- this always errors when no name is
    // given (confirmed: `compopt -o nospace` outside a completion function
    // prints "not currently executing completion function" and exits 1).
    fn run_compopt(&mut self, args: &[String]) -> i32 {
        let mut adds: Vec<String> = Vec::new();
        let mut removes: Vec<String> = Vec::new();
        let mut name: Option<&str> = None;
        let mut idx = 0;
        while idx < args.len() {
            match args[idx].as_str() {
                "-o" => {
                    let Some(v) = args.get(idx + 1) else {
                        sh_eprintln!(self, "bish: compopt: -o: option requires an argument");
                        return 2;
                    };
                    if !compgen::O_OPTIONS.contains(&v.as_str()) {
                        sh_eprintln!(self, "bish: compopt: {v}: invalid option name");
                        return 2;
                    }
                    adds.push(v.clone());
                    idx += 2;
                }
                "+o" => {
                    let Some(v) = args.get(idx + 1) else {
                        sh_eprintln!(self, "bish: compopt: +o: option requires an argument");
                        return 2;
                    };
                    removes.push(v.clone());
                    idx += 2;
                }
                other => {
                    name = Some(other);
                    idx += 1;
                }
            }
        }
        let Some(name) = name else {
            sh_eprintln!(self, "bish: compopt: not currently executing completion function");
            return 1;
        };
        let Some(spec) = self.completions.get_mut(name) else {
            sh_eprintln!(self, "bish: compopt: {name}: no completion specification");
            return 1;
        };
        for o in adds {
            if !spec.opts.contains(&o) {
                spec.opts.push(o);
            }
        }
        spec.opts.retain(|o| !removes.contains(o));
        0
    }

    // Fish-style abbreviations: `self.abbrs`'s own doc comment covers the
    // storage/trigger split (this builtin only ever stores/queries/lists;
    // the actual expansion happens in editor.rs's read_line). Deliberately
    // scoped down from real fish's own `abbr`: no `--rename`, no
    // `--position anywhere` (always command position, fish's own default),
    // no regex/function-backed abbreviations, no scope flags (`-U`/`-g`,
    // meaningless here -- bish has no fish-variable-style scoping at all)
    // -- just add/erase/list/show/query, the part of `abbr` people
    // actually reach for day to day. An expansion *can* carry `%s`
    // placeholders, which makes it a snippet rather than plain text (see
    // bishedit::snippet, and `snippet::parse_order` for how a trailing
    // `2 1` is told apart from two more words of expansion).
    // `--lang=GLOB` scopes an abbreviation to the languages it's for
    // (default `bash`, which is what the shell prompt itself counts as),
    // so an abbreviation's identity here is `(name, lang)` and the same
    // short name can mean one thing at a prompt and another in a Rust
    // file. See `take_lang_flag` for why it's only recognized among the
    // leading options.
    // `-a`/`--add` is optional (`abbr NAME EXPANSION` alone means add, `abbr`
    // with a recognized name misparsed as NAME would just mean "add an
    // abbreviation literally named `-x`" -- an accepted, unvalidated edge
    // case, same spirit as `alias`'s own lack of name validation above).
    // Bare `abbr` (no args at all) shows everything, matching this
    // codebase's own `alias`'s bare-listing convention rather than real
    // fish's (which errors) -- consistency with the sibling builtin wins
    // here since nothing else in bish already commits to fish's own
    // no-args-is-an-error behavior.
    fn run_abbr(&mut self, args: &[String]) -> i32 {
        enum Mode {
            Add,
            Erase,
            List,
            Show,
            Query,
        }
        let (args, lang) = snippet::take_lang_flag(args);
        let args: &[String] = &args;
        let (mode, rest) = match args.first().map(String::as_str) {
            Some("-a") | Some("--add") => (Mode::Add, &args[1..]),
            Some("-e") | Some("--erase") => (Mode::Erase, &args[1..]),
            Some("-l") | Some("--list") => (Mode::List, &args[1..]),
            Some("-s") | Some("--show") => (Mode::Show, &args[1..]),
            Some("-q") | Some("--query") => (Mode::Query, &args[1..]),
            None => (Mode::Show, args),
            Some(_) => (Mode::Add, args),
        };
        match mode {
            Mode::Add => {
                let Some((name, expansion_words)) = rest.split_first() else {
                    sh_eprintln!(self, "bish: abbr: -a: requires a NAME and an EXPANSION");
                    return 2;
                };
                if expansion_words.is_empty() {
                    sh_eprintln!(self, "bish: abbr: -a: requires an EXPANSION for '{name}'");
                    return 2;
                }
                let expansion = expansion_words.join(" ");
                let lang = lang.unwrap_or_else(|| snippet::DEFAULT_LANG.to_string());
                // Redefinition is keyed on both name *and* language: the
                // same name under a different `--lang=` is a different
                // abbreviation, not a replacement for this one.
                match self.abbrs.iter_mut().find(|a| a.name == *name && a.lang == lang) {
                    Some(existing) => existing.expansion = expansion,
                    None => self.abbrs.push(Abbr { name: name.clone(), expansion, lang }),
                }
                0
            }
            Mode::Erase => {
                if rest.is_empty() {
                    sh_eprintln!(self, "bish: abbr: -e: requires a NAME");
                    return 2;
                }
                let mut status = 0;
                for name in rest {
                    // With no `--lang=`, erasing a name erases it in
                    // every language it was defined for -- "get rid of
                    // `foo`" is what someone typing that means. With one,
                    // only the exact `(name, lang)` entry goes.
                    let before = self.abbrs.len();
                    self.abbrs.retain(|a| a.name != *name || lang.as_ref().is_some_and(|l| a.lang != *l));
                    if self.abbrs.len() == before {
                        sh_eprintln!(self, "bish: abbr: -e: {}: no such abbreviation", name);
                        status = 1;
                    }
                }
                status
            }
            Mode::List => {
                for abbr in &self.abbrs {
                    sh_println!(self, "{}", abbr.name);
                }
                0
            }
            Mode::Show => {
                for abbr in &self.abbrs {
                    // A non-default language is printed back as the same
                    // `--lang=`, so `abbr -s` stays something you can
                    // paste straight back in.
                    let lang = if abbr.lang == snippet::DEFAULT_LANG {
                        String::new()
                    } else {
                        format!("--lang={} ", crate::serialize::quote_literal(&abbr.lang))
                    };
                    sh_println!(
                        self,
                        "abbr -a {}{} {}",
                        lang,
                        crate::serialize::quote_literal(&abbr.name),
                        crate::serialize::quote_literal(&abbr.expansion)
                    );
                }
                0
            }
            Mode::Query => {
                if rest.is_empty() {
                    sh_eprintln!(self, "bish: abbr: -q: requires at least one NAME");
                    return 2;
                }
                // `--lang=` narrows the question to that one language;
                // without it, any language counts.
                let hit = |a: &Abbr, name: &String| a.name == *name && lang.as_ref().is_none_or(|l| a.lang == *l);
                if rest.iter().all(|name| self.abbrs.iter().any(|a| hit(a, name))) { 0 } else { 1 }
            }
        }
    }

    // `window`/`w`/`win` -- the window-manager builtin. Only validates the
    // subcommand and triggers promotion; the actual session/window
    // mutation happens in repl.rs, reached via the bubbled-up
    // ExecResult::Window signal (see that variant's doc comment for why
    // this can't just mutate shared state directly from here).
    // `::bish window ...` (and command mode's `window` alias). The
    // read-only subcommands answer from `Shell::windows` right here; the
    // rest bubble an action up to repl.rs -- and only where something is
    // actually there to act on it, since `ExecResult::Window` is a
    // signal `run_program` stops on and letting one escape from `bish
    // script.sh` would end the script for no reason.
    fn run_window(&mut self, args: &[String]) -> ExecResult {
        let result = self.run_window_inner(args);
        if matches!(result, ExecResult::Window(_)) && !self.windows_available && !self.restrict_to_builtins {
            sh_eprintln!(self, "bish: ::bish window: no window manager here (this needs an interactive bish)");
            return ExecResult::Status(1);
        }
        result
    }

    fn run_window_inner(&mut self, args: &[String]) -> ExecResult {
        fn parse_window_name(shell: &mut Shell, subcommand: &str, rest: &[String]) -> Result<Option<String>, i32> {
            match rest.first().map(String::as_str) {
                None => Ok(None),
                Some("--name") | Some("-n") => match rest.len() {
                    1 => {
                        sh_eprintln!(shell, "bish: window: {subcommand}: --name needs a name");
                        Err(2)
                    }
                    _ => Ok(Some(rest[1..].join(" "))),
                },
                Some(other) => {
                    sh_eprintln!(shell, "bish: window: {subcommand}: unexpected argument '{other}' (expected --name NAME)");
                    Err(2)
                }
            }
        }

        self.promote_if_needed();
        match args.first().map(String::as_str) {
            Some("next") | Some("n") => ExecResult::Window(WindowAction::Next),
            Some("previous") | Some("prev") | Some("p") => ExecResult::Window(WindowAction::Previous),
            Some("new") | Some("c") | Some("create") => match parse_window_name(self, "create", &args[1..]) {
                Ok(name) => ExecResult::Window(WindowAction::New { name }),
                Err(status) => ExecResult::Status(status),
            },
            Some("rename") | Some("ren") => match args.get(1) {
                // A bare `window rename` clears the name; anything else
                // is the new one, joined so `window rename my project`
                // means what it looks like.
                None => ExecResult::Window(WindowAction::Rename(None)),
                Some(_) => ExecResult::Window(WindowAction::Rename(Some(args[1..].join(" ")))),
            },
            // The two that only *read*. Both answer from
            // `Shell::windows`, which means both behave like any other
            // builtin: `ls` writes to whatever sink it has (so
            // `$(window ls)` captures it) and `select` fails
            // synchronously (so `select || create` works in a function,
            // a subshell or an `if`).
            Some("ls") | Some("list") => {
                for w in &self.windows.clone() {
                    let name = w.name.clone().unwrap_or_default();
                    let current = if w.current { "*" } else { "" };
                    sh_println!(self, "{}\t{name}\t{}\t{}\t{current}", w.id, w.cwd, w.panes);
                }
                ExecResult::Status(0)
            }
            Some("select") | Some("sel") => match args.get(1) {
                Some(target) => {
                    // A name first, an id second: a name is what a config
                    // function knows, an id is what it falls back to.
                    let found = self
                        .windows
                        .iter()
                        .position(|w| w.name.as_deref() == Some(target.as_str()))
                        .or_else(|| self.windows.iter().position(|w| w.id.to_string() == *target));
                    match found {
                        Some(index) => ExecResult::Window(WindowAction::Select(index)),
                        None => {
                            sh_eprintln!(self, "bish: window: select: no window named '{target}'");
                            ExecResult::Status(1)
                        }
                    }
                }
                None => {
                    sh_eprintln!(self, "bish: window: select: usage: window select <name>|<id>");
                    ExecResult::Status(2)
                }
            },
            Some("close") | Some("q") | Some("quit") => ExecResult::Window(WindowAction::Close),
            // WindowAction::Split's own `horizontal` names the divider
            // LINE's orientation (true = a horizontal dividing line,
            // panes stacked top/bottom), matching vim's :split/:vsplit
            // convention. Users read "vertical"/"horizontal" by the
            // panes' own arrangement axis instead (stacked = panes
            // arranged *vertically*, side by side = arranged
            // *horizontally*) -- the opposite pairing -- so `split`/`s`
            // maps to horizontal:false (side by side) and `vsplit`/`v`
            // to horizontal:true (stacked), even though that looks
            // inverted next to the field's own name.
            Some("split") | Some("s") => ExecResult::Window(WindowAction::Split { horizontal: false }),
            Some("vsplit") | Some("v") => ExecResult::Window(WindowAction::Split { horizontal: true }),
            Some("h") | Some("left") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Left)),
            Some("j") | Some("below") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Down)),
            Some("k") | Some("above") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Up)),
            Some("l") | Some("right") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Right)),
            Some("=") | Some("balance") => ExecResult::Window(WindowAction::Balance),
            Some("+") | Some("sizeup") => ExecResult::Window(WindowAction::SizeUp),
            Some("-") | Some("sizedown") => ExecResult::Window(WindowAction::SizeDown),
            Some("size") => match args.get(1).and_then(|a| parse_size_spec(a)) {
                Some(spec) => ExecResult::Window(WindowAction::SetSize(spec)),
                None => {
                    sh_eprintln!(self, "bish: window: size: usage: window size <N>|<N>%|<N>/<M>");
                    ExecResult::Status(2)
                }
            },
            Some("fg") => match args.get(1).and_then(|a| a.parse::<u32>().ok()) {
                Some(id) => ExecResult::Window(WindowAction::FgSession(id)),
                None => {
                    sh_eprintln!(self, "bish: window: fg: usage: window fg <window-id>");
                    ExecResult::Status(2)
                }
            },
            Some(other) => {
                sh_eprintln!(self, "bish: window: unknown subcommand: {}", other);
                ExecResult::Status(2)
            }
            None => {
                sh_eprintln!(
                    self,
                    "bish: window: missing subcommand (next(n)/previous/new(c,create)/close(q,quit)/split(s)/vsplit(v)/h(left)/j(below)/k(above)/l(right)/=(balance)/+(sizeup)/-(sizedown)/size <N|N%,N/M>/fg <id>)"
                );
                ExecResult::Status(2)
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

    // ulimit [-HS] [-a] [-cdefilmnqrstuvx [limit]]. `-a` doesn't attempt to
    // byte-match bash's exact column alignment (its internal padding rules
    // aren't a fixed width across all entries) -- purely cosmetic output
    // that scripts don't parse, unlike the single-limit query/set forms
    // below, which do match exactly. Moved here from a builtins.rs free
    // function (M6) so its output goes through self.sink_out/sink_err
    // like every other builtin's, instead of always writing straight to
    // the real stdout/stderr regardless of which session ran it.
    fn run_ulimit(&mut self, args: &[String]) -> i32 {
        unsafe extern "C" {
            fn getrlimit(resource: i32, rlim: *mut RLimit) -> i32;
            fn setrlimit(resource: i32, rlim: *const RLimit) -> i32;
        }
        let mut hard = false;
        let mut soft = false;
        let mut show_all = false;
        let mut flag: Option<char> = None;
        let mut value: Option<String> = None;
        for a in args {
            if let Some(rest) = a.strip_prefix('-').filter(|r| !r.is_empty()) {
                for c in rest.chars() {
                    match c {
                        'H' => hard = true,
                        'S' => soft = true,
                        'a' => show_all = true,
                        other => flag = Some(other),
                    }
                }
            } else {
                value = Some(a.clone());
            }
        }
        if show_all {
            for spec in LIMIT_SPECS {
                let mut rl = RLimit { cur: 0, max: 0 };
                unsafe {
                    getrlimit(spec.resource, &mut rl);
                }
                let v = if hard { rl.max } else { rl.cur };
                let unit_part = if spec.unit.is_empty() { String::new() } else { format!("{}, ", spec.unit) };
                sh_println!(self, "{:<24}({}-{}) {}", spec.label, unit_part, spec.flag, fmt_limit(v, spec.div));
            }
            return 0;
        }
        let f = flag.unwrap_or('f');
        let spec = match LIMIT_SPECS.iter().find(|s| s.flag == f) {
            Some(s) => s,
            None => {
                sh_eprintln!(self, "bish: ulimit: -{}: invalid option", f);
                return 1;
            }
        };
        let mut rl = RLimit { cur: 0, max: 0 };
        unsafe {
            getrlimit(spec.resource, &mut rl);
        }
        match value {
            None => {
                let v = if hard { rl.max } else { rl.cur };
                sh_println!(self, "{}", fmt_limit(v, spec.div));
                0
            }
            Some(v) => {
                let new_val: u64 = if v == "unlimited" {
                    RLIM_INFINITY
                } else {
                    match v.parse::<u64>() {
                        Ok(n) => n * spec.div,
                        Err(_) => {
                            sh_eprintln!(self, "bish: ulimit: {}: invalid number", v);
                            return 1;
                        }
                    }
                };
                if !soft && !hard {
                    rl.cur = new_val;
                    rl.max = new_val;
                } else {
                    if soft {
                        rl.cur = new_val;
                    }
                    if hard {
                        rl.max = new_val;
                    }
                }
                if unsafe { setrlimit(spec.resource, &rl) } != 0 {
                    sh_eprintln!(self, "bish: ulimit: cannot modify limit: {}", std::io::Error::last_os_error());
                    return 1;
                }
                0
            }
        }
    }

    // POSIX has no query-only umask read -- `umask(new) -> previous` is the
    // only syscall shape, so reading the current value means setting a
    // throwaway mask and immediately restoring what was there. Moved here
    // alongside run_ulimit for the same M6 sink reason.
    fn run_umask(&mut self, args: &[String]) -> i32 {
        let symbolic = args.iter().any(|a| a == "-S");
        match args.iter().find(|a| !a.starts_with('-')) {
            Some(s) => match u32::from_str_radix(s, 8) {
                Ok(m) => {
                    unsafe {
                        umask(m);
                    }
                    // Keep this session's own remembered umask in lockstep
                    // -- see sync_real_state_in/out's own doc comment for
                    // why a mutation of this real, process-wide syscall
                    // needs a Shell-owned mirror at all.
                    self.umask_snapshot = m;
                    0
                }
                Err(_) => {
                    sh_eprintln!(self, "bish: umask: {}: invalid octal number", s);
                    1
                }
            },
            None => {
                let cur = current_umask();
                if symbolic {
                    sh_println!(self, "{}", umask_symbolic(cur));
                } else {
                    sh_println!(self, "{:04o}", cur);
                }
                0
            }
        }
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
        self.run_cd(&[path.to_string_lossy().into_owned()])
    }

    fn run_cd(&mut self, args: &[String]) -> i32 {
        let old = self.cwd.to_string_lossy().into_owned();
        let target = if let Some(dir) = args.first() {
            if dir == "-" {
                match std::env::var("OLDPWD") {
                    Ok(p) => {
                        sh_println!(self, "{}", p);
                        p
                    }
                    Err(_) => {
                        sh_eprintln!(self, "cd: OLDPWD not set");
                        return 1;
                    }
                }
            } else {
                dir.clone()
            }
        } else {
            match std::env::var("HOME") {
                Ok(h) => h,
                Err(_) => {
                    sh_eprintln!(self, "cd: HOME not set");
                    return 1;
                }
            }
        };
        let _ = old;
        match self.change_directory(std::path::Path::new(&target)) {
            Ok(()) => 0,
            Err(e) if e == RESTRICTED => {
                sh_eprintln!(self, "bish: cd: restricted");
                1
            }
            Err(e) => {
                sh_eprintln!(self, "cd: {}: {}", target, e);
                1
            }
        }
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
        std::env::set_current_dir(target).map_err(|e| e.to_string())?;
        self.cwd = std::env::current_dir().unwrap_or_else(|_| target.to_path_buf());
        let new = self.cwd.to_string_lossy().into_owned();
        unsafe {
            std::env::set_var("OLDPWD", &old);
            std::env::set_var("PWD", &new);
        }
        // ...and into this session's own remembered environment, not
        // just the real one. `sync_real_state_in` reapplies that
        // snapshot before every command, so a raw `set_var` made outside
        // a command -- which is exactly what the file browser's Ctrl-Y
        // does -- survives until the next command and is then silently
        // reverted. (`cd` itself never saw this: it runs inside a
        // command, and `sync_real_state_out` captures the real
        // environment right afterwards.) Same trap, and the same fix, as
        // `set_terminal_capability_env` below.
        self.env_snapshot.insert("OLDPWD".to_string(), old);
        self.env_snapshot.insert("PWD".to_string(), new);
        Ok(())
    }

    // `echo [-neE] [arg...]`: writes each arg separated by a single
    // space, then a trailing newline unless -n was given. Flags must
    // come first and be one of exactly n/e/E (bundled, e.g. "-ne") --
    // the first argument that doesn't fit that shape ends flag parsing,
    // matching bash's own echo (no long options, no "--" special case).
    // -e enables the same backslash escapes real bash's echo recognizes
    // (see echo_expand_escapes); -E (the default) leaves them untouched.
    fn run_echo(&self, args: &[String]) -> i32 {
        let mut interpret_escapes = false;
        let mut trailing_newline = true;
        let mut i = 0;
        while let Some(a) = args.get(i) {
            if a.len() < 2 || !a.starts_with('-') || !a[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E')) {
                break;
            }
            for c in a[1..].chars() {
                match c {
                    'n' => trailing_newline = false,
                    'e' => interpret_escapes = true,
                    'E' => interpret_escapes = false,
                    _ => unreachable!(),
                }
            }
            i += 1;
        }

        let mut out = String::new();
        let mut stopped_early = false;
        for (pos, a) in args[i..].iter().enumerate() {
            if pos > 0 {
                out.push(' ');
            }
            if interpret_escapes {
                let (expanded, hit_c) = echo_expand_escapes(a);
                out.push_str(&expanded);
                if hit_c {
                    stopped_early = true;
                    break;
                }
            } else {
                out.push_str(a);
            }
        }
        if trailing_newline && !stopped_early {
            out.push('\n');
        }
        sh_print!(self, "{}", out);
        0
    }

    // `printf FORMAT [ARGS...]` / `printf -v NAME FORMAT [ARGS...]`:
    // bash-compatible subset -- %s %d %i %o %u %x %X %c %b %q %%
    // conversions, with "-" (left-align) and "0" (zero-pad) flags, a
    // numeric width, and a ".precision" (applied to %s only -- a
    // truncation length; other conversions ignore it, a minor
    // simplification against real printf's %d minimum-digit-count
    // behavior). FORMAT's own backslash escapes (\n, \t, ...) are
    // always interpreted -- unlike echo, there's no -e/-E switch, POSIX
    // printf's format string is escape-interpreted unconditionally.
    // FORMAT is cycled -- reused from the start -- for as long as
    // there's still at least one unconsumed argument left, matching
    // real printf (`printf "%s\n" a b c` prints three lines); a numeric
    // conversion given a missing or non-numeric argument is treated as
    // 0, a string conversion given a missing one as "". -v NAME assigns
    // the formatted result to a shell variable instead of printing it.
    fn run_printf(&mut self, args: &[String]) -> i32 {
        let (var_name, rest) = if args.first().map(String::as_str) == Some("-v") {
            match args.get(1) {
                Some(name) => (Some(name.clone()), &args[2..]),
                None => {
                    sh_eprintln!(self, "bish: printf: -v: option requires an argument");
                    return 1;
                }
            }
        } else {
            (None, &args[..])
        };
        let Some(format) = rest.first() else {
            sh_eprintln!(self, "bish: printf: usage: printf format [arguments]");
            return 1;
        };
        let values = &rest[1..];

        let mut out = String::new();
        let mut idx = 0;
        loop {
            let before = idx;
            printf_format_once(format, values, &mut idx, &mut out);
            if idx >= values.len() || idx == before {
                break;
            }
        }

        match var_name {
            Some(name) => self.assign_var(&name, out),
            None => sh_print!(self, "{}", out),
        }
        0
    }

    fn run_pushd(&mut self, args: &[String]) -> i32 {
        let target = match args.iter().find(|a| !a.starts_with('-')) {
            Some(d) => d.clone(),
            None => match self.dir_stack.first() {
                Some(d) => d.clone(),
                None => {
                    sh_eprintln!(self, "bish: pushd: no other directory");
                    return 1;
                }
            },
        };
        if !args.iter().any(|a| !a.starts_with('-')) {
            // Bare `pushd`: rotate -- cd into the current top-of-stack,
            // then push the old cwd back onto the front, net-swapping them.
            self.dir_stack.remove(0);
        }
        let old_cwd = self.cwd.to_string_lossy().into_owned();
        if self.run_cd(&[target]) != 0 {
            return 1;
        }
        self.dir_stack.insert(0, old_cwd);
        self.print_dirs(false);
        0
    }

    fn run_popd(&mut self, _args: &[String]) -> i32 {
        let target = match self.dir_stack.first() {
            Some(d) => d.clone(),
            None => {
                sh_eprintln!(self, "bish: popd: directory stack empty");
                return 1;
            }
        };
        if self.run_cd(&[target]) != 0 {
            return 1;
        }
        self.dir_stack.remove(0);
        self.print_dirs(false);
        0
    }

    fn run_dirs(&mut self, args: &[String]) -> i32 {
        if args.iter().any(|a| a == "-c") {
            self.dir_stack.clear();
            return 0;
        }
        self.print_dirs(args.iter().any(|a| a == "-v"));
        0
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

    fn print_dirs(&self, vertical: bool) {
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

    // set [-euxo pipefail] [--] [args...]. Combined single-char flags
    // (-eu, -ex, -eux) work; `-o name` must be its own token (not combined
    // into a cluster with other short flags) -- matches real bash, which
    // also rejects e.g. `-euo pipefail` (it consumes `-o` with no argument
    // of its own, then tries to parse "pipefail"'s remaining letters as
    // further short flags and errors on the first invalid one).
    fn run_set(&mut self, args: &[String]) -> i32 {
        let mut idx = 0;
        let mut saw_dashdash = false;
        while idx < args.len() {
            let a = &args[idx];
            if a == "--" {
                saw_dashdash = true;
                idx += 1;
                break;
            }
            if let Some(rest) = a.strip_prefix('-').filter(|r| !r.is_empty()) {
                if rest == "o" {
                    if let Some(optname) = args.get(idx + 1) {
                        self.apply_shell_option(optname, true);
                        idx += 2;
                        continue;
                    }
                }
                for c in rest.chars() {
                    self.apply_shell_flag(c, true);
                }
                idx += 1;
                continue;
            }
            if let Some(rest) = a.strip_prefix('+').filter(|r| !r.is_empty()) {
                if rest == "o" {
                    if let Some(optname) = args.get(idx + 1) {
                        self.apply_shell_option(optname, false);
                        idx += 2;
                        continue;
                    }
                }
                for c in rest.chars() {
                    self.apply_shell_flag(c, false);
                }
                idx += 1;
                continue;
            }
            break;
        }
        if saw_dashdash || idx < args.len() {
            let new_args = args[idx..].to_vec();
            if let Some(frame) = self.arg_frames.last_mut() {
                *frame = new_args;
            }
        }
        0
    }

    fn apply_shell_flag(&mut self, c: char, on: bool) {
        match c {
            'e' => self.opt_errexit = on,
            'u' => self.opt_nounset = on,
            'x' => self.opt_xtrace = on,
            'f' => self.opt_noglob = on,
            'm' => self.opt_monitor = on,
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

    fn apply_shell_option(&mut self, name: &str, on: bool) {
        match name {
            "pipefail" => self.opt_pipefail = on,
            "errexit" => self.opt_errexit = on,
            "nounset" => self.opt_nounset = on,
            "xtrace" => self.opt_xtrace = on,
            "noglob" => self.opt_noglob = on,
            "monitor" => self.opt_monitor = on,
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
            self.check_pending_signals();
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
            if self.opt_errexit && self.suppress_errexit == 0 && result.status() != 0 {
                self.run_exit_trap();
                return ExecResult::Exit(result.status());
            }
        }
        result
    }

    fn run_and_or(&mut self, and_or: &AndOr, background: bool) -> ExecResult {
        let mut result = self.run_pipeline(&and_or.first, background);
        self.last_status = result.status();
        if result.is_signal() {
            return result;
        }
        let mut status = result.status();
        for (comb, pipeline) in &and_or.rest {
            let should_run = match comb {
                Combinator::And => status == 0,
                Combinator::Or => status != 0,
            };
            if should_run {
                result = self.run_pipeline(pipeline, background);
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
            return self.run_command(&pipeline.commands[0], background);
        }
        ExecResult::Status(self.run_multi(&pipeline.commands, background))
    }

    fn run_command(&mut self, cmd: &parser::Command, background: bool) -> ExecResult {
        let redirects: &[Redirect] = command_own_redirects(cmd);
        if !redirects.is_empty() {
            return self.run_compound_redirected(cmd, redirects, background);
        }
        self.run_command_body(cmd, background)
    }

    // The part of run_command that actually dispatches on `cmd`'s own
    // variant, *after* its own redirects (if any) have already been
    // handled -- split out so run_in_child_shell's ChildSource::Parsed
    // case (run_compound_redirected's own in-process conversion) can run
    // `cmd`'s content directly without re-checking command_own_redirects,
    // which would just see the same still-attached redirects and call
    // run_compound_redirected right back into itself, forever.
    fn run_command_body(&mut self, cmd: &parser::Command, background: bool) -> ExecResult {
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
                self.functions.insert(name.clone(), (**body).clone());
                ExecResult::Status(0)
            }
            parser::Command::Subshell(raw, _redirects) => ExecResult::Status(self.run_subshell(raw, background)),
            parser::Command::Arith(raw, _redirects) => match arith::eval(raw, self) {
                Ok(v) => ExecResult::Status(if v != 0 { 0 } else { 1 }),
                Err(e) => {
                    sh_eprintln!(self, "bish: (({})): {}", raw, e);
                    ExecResult::Status(1)
                }
            },
            parser::Command::Test(atoms, _redirects) => self.run_test(atoms),
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
                        Ok(f) => Box::new(std::io::BufReader::new(f)),
                        Err(e) => {
                            sh_eprintln!(self, "bish: {}", e);
                            Box::new(std::io::Cursor::new(Vec::new()))
                        }
                    };
                }
                _ => continue,
            }
        }
        // A converted construct's own stdin (see StdioOverride's doc
        // comment) takes precedence over the real process stdin -- e.g. a
        // `while read` loop's `< file` redirect sits on the *enclosing*
        // compound command, not on this individual `read`, so this
        // command's own `cmd.redirects` above is empty even though stdin
        // very much isn't the real terminal.
        if let Some(o) = &self.stdio_override {
            if let Some(state) = &o.borrow().stdin {
                return Box::new(SharedStdinReader { state: state.clone(), local: Vec::new() });
            }
        }
        // NOT `BufReader::new(stdin())` -- that wraps stdin in a fresh,
        // throwaway read-ahead buffer on every `read` call. A single
        // read_line() can pull far more than one line into that buffer in
        // one syscall; whatever it read past the first line is then lost
        // when the wrapper is dropped at the end of this call, so a `while
        // read` loop would silently only ever see its first line. Stdin's
        // own lock reuses the shared, persistent buffer behind
        // std::io::stdin() instead, so nothing already-read is discarded
        // between calls.
        Box::new(std::io::stdin().lock())
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
        match crate::lexer::Lexer::new(src).tokenize() {
            Ok(toks) => match crate::parser::Parser::new(toks).parse_program() {
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
        for scope in &self.var_scopes {
            for (k, v) in scope {
                flattened.insert(k.as_str(), v.as_str());
            }
        }
        for (k, v) in &flattened {
            s.push_str(k);
            s.push('=');
            s.push_str(&crate::serialize::quote_literal(v));
            s.push('\n');
        }
        for (name, items) in &self.arrays {
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
                and_or: AndOr {
                    first: Pipeline { commands: vec![def], negate: false },
                    rest: Vec::new(),
                },
                sep: Sep::Seq,
                line: 0,
            }]));
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
        let mut child = self.new_virtual_child();

        let effective_stdin: Option<Rc<RefCell<SharedReaderState>>> = match stdio.stdin {
            Some(f) => Some(Rc::new(RefCell::new(SharedReaderState { file: f, pending: Vec::new() }))),
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdin.clone()),
        };
        let effective_stdout: Option<std::fs::File> = match &stdio.stdout {
            Some(f) => f.try_clone().ok(),
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdout.as_ref().and_then(|f| f.try_clone().ok())),
        };
        child.stdio_override = if effective_stdin.is_some() || effective_stdout.is_some() {
            Some(Rc::new(RefCell::new(StdioOverride { stdin: effective_stdin, stdout: effective_stdout })))
        } else {
            None
        };
        // No explicit stdout/stderr override => this construct has no
        // redirect of its own, so its sink should be *exactly* the
        // parent's current one (already fully resolved -- Real/Grid/
        // Capture/Builtin, whatever it is), not a fresh wrapper around it.
        child.sink = if stdio.stdout.is_some() || stdio.stderr.is_some() || stdio.dup_err_to_out || stdio.dup_out_to_err {
            OutputSink::Builtin {
                previous: Box::new(self.sink.clone()),
                stdout: stdio.stdout.as_ref().and_then(|f| f.try_clone().ok()).map(|f| Rc::new(RefCell::new(f))),
                stderr: stdio.stderr.as_ref().and_then(|f| f.try_clone().ok()).map(|f| Rc::new(RefCell::new(f))),
                dup_err_to_out: stdio.dup_err_to_out,
                dup_out_to_err: stdio.dup_out_to_err,
            }
        } else {
            self.sink.clone()
        };

        // The real OS cwd is process-wide, shared with the real parent,
        // even though `child` is otherwise a fully independent Shell -- a
        // `cd` inside this construct (`$(cd /tmp && pwd)`) must not leak
        // back out to the real shell once this call returns.
        let real_cwd_before = std::env::current_dir().ok();
        // A plain (non-`local`) variable assignment isn't Shell-owned
        // state at all -- raw_var_write's own fallback writes straight to
        // the real process environment (see its own doc comment), which
        // is how bish gives builtins/scripts free interop with spawned
        // external commands. That means it's process-wide, exactly like
        // cwd: `new_virtual_child`'s "the two sessions evolve
        // independently" doc comment is only true for arrays/assoc-
        // arrays/functions/etc, which really are their own Rust-owned
        // fields -- a bare `x=2` inside this construct would otherwise
        // permanently clobber the real parent's own `x` the moment this
        // runs in-process instead of as a separate OS process. Snapshot
        // and restore exactly, same technique as cwd just above: any var
        // the child added gets removed, anything it changed gets put
        // back, matching real bash's fork isolation for env/variables.
        let env_before: Vec<(String, String)> = std::env::vars().collect();
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

        let result = child.run_source_here(raw, "subshell");
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
        let before_keys: std::collections::HashSet<&str> = env_before.iter().map(|(k, _)| k.as_str()).collect();
        let after_keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
        for k in after_keys {
            if !before_keys.contains(k.as_str()) {
                unsafe { std::env::remove_var(&k) };
            }
        }
        for (k, v) in &env_before {
            unsafe { std::env::set_var(k, v) };
        }

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
        let saved_sink = self.sink.clone();
        let saved_stdio_override = self.stdio_override.clone();

        let effective_stdin: Option<Rc<RefCell<SharedReaderState>>> = match stdio.stdin {
            Some(f) => Some(Rc::new(RefCell::new(SharedReaderState { file: f, pending: Vec::new() }))),
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdin.clone()),
        };
        let effective_stdout: Option<std::fs::File> = match &stdio.stdout {
            Some(f) => f.try_clone().ok(),
            None => self.stdio_override.as_ref().and_then(|o| o.borrow().stdout.as_ref().and_then(|f| f.try_clone().ok())),
        };
        self.stdio_override = if effective_stdin.is_some() || effective_stdout.is_some() {
            Some(Rc::new(RefCell::new(StdioOverride { stdin: effective_stdin, stdout: effective_stdout })))
        } else {
            None
        };
        if stdio.stdout.is_some() || stdio.stderr.is_some() || stdio.dup_err_to_out || stdio.dup_out_to_err {
            self.sink = OutputSink::Builtin {
                previous: Box::new(saved_sink.clone()),
                stdout: stdio.stdout.as_ref().and_then(|f| f.try_clone().ok()).map(|f| Rc::new(RefCell::new(f))),
                stderr: stdio.stderr.as_ref().and_then(|f| f.try_clone().ok()).map(|f| Rc::new(RefCell::new(f))),
                dup_err_to_out: stdio.dup_err_to_out,
                dup_out_to_err: stdio.dup_out_to_err,
            };
        }

        let result = self.run_command_body(cmd, false);

        self.sink = saved_sink;
        self.stdio_override = saved_stdio_override;
        result
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
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                sh_eprintln!(self, "bish: subshell: {}", e);
                return 1;
            }
        };
        let script = self.functions_preamble() + raw;
        let mut command = Command::new(exe);
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
                self.push_job_with_pty(vec![child], format!("({})", raw), master);
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
        let mut command = Command::new(exe);
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
    fn run_compound_redirected(&mut self, cmd: &parser::Command, redirects: &[Redirect], background: bool) -> ExecResult {
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
        let mut command = Command::new(exe);
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
            command.stdin(redirs.stdin.or_else(|| bg_slave(&bg_pty)).unwrap_or_else(Stdio::inherit));
            command.stdout(redirs.stdout.or_else(|| bg_slave(&bg_pty)).unwrap_or_else(Stdio::inherit));
        } else {
            command.stdin(redirs.stdin.unwrap_or_else(|| self.spawn_stdin_stdio()));
            command.stdout(redirs.stdout.unwrap_or_else(|| self.spawn_stdout_stdio()));
        }
        command.stderr(redirs.stderr.or_else(|| bg_slave(&bg_pty)).unwrap_or_else(Stdio::inherit));
        apply_fd_redirects(&mut command, redirs.dup_stderr_to_stdout, redirs.extra_fds);
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

    fn run_command_substitution(&mut self, raw: &str) -> String {
        let path = proc_sub_temp_path();
        let file = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(_) => return String::new(),
        };
        self.run_in_child_shell(raw, ChildStdio { stdout: Some(file), ..Default::default() });
        let mut s = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        while s.ends_with('\n') {
            s.pop();
        }
        s
    }

    // `<(cmd)`: runs cmd to completion now, capturing its stdout into a
    // temp file, and substitutes that file's path. Real bash streams this
    // concurrently through a FIFO; see the ProcSubIn/ProcSubOut doc comment
    // in lexer.rs for why this shell uses a temp file instead. The path is
    // queued for cleanup (self.proc_sub_cleanup) once the enclosing command
    // has finished reading it.
    fn run_proc_sub_in(&mut self, raw: &str) -> String {
        let path = proc_sub_temp_path();
        let file = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                sh_eprintln!(self, "bish: process substitution: {}", e);
                return String::new();
            }
        };
        self.run_in_child_shell(raw, ChildStdio { stdout: Some(file), ..Default::default() });
        let path_str = path.to_string_lossy().into_owned();
        self.proc_sub_cleanup.push(path_str.clone());
        path_str
    }

    // `>(cmd)`: substitutes a temp file path immediately (so the enclosing
    // command can write to it like any other file), and queues cmd to run
    // reading that file back once the enclosing command finishes -- correct
    // data flow, but sequential rather than concurrent (see lexer.rs).
    fn run_proc_sub_out(&mut self, raw: &str) -> String {
        let path = proc_sub_temp_path();
        if let Err(e) = std::fs::File::create(&path) {
            sh_eprintln!(self, "bish: process substitution: {}", e);
            return String::new();
        }
        let path_str = path.to_string_lossy().into_owned();
        self.proc_sub_out_pending.push((path_str.clone(), raw.to_string()));
        path_str
    }

    // Runs any `>(cmd)` substitutions queued by the command that just
    // finished, then deletes every proc-sub temp file used this round.
    fn drain_proc_subs(&mut self) {
        if !self.proc_sub_out_pending.is_empty() {
            let pending = std::mem::take(&mut self.proc_sub_out_pending);
            for (path, raw) in pending {
                if let Ok(file) = std::fs::File::open(&path) {
                    self.run_in_child_shell(&raw, ChildStdio { stdin: Some(file), ..Default::default() });
                }
                self.proc_sub_cleanup.push(path);
            }
        }
        for path in self.proc_sub_cleanup.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn call_function(&mut self, body: &parser::Command, call_args: Vec<String>) -> ExecResult {
        self.arg_frames.push(call_args);
        self.var_scopes.push(HashMap::new());
        self.array_local_stack.push(Vec::new());
        self.assoc_local_stack.push(Vec::new());
        self.nameref_local_stack.push(Vec::new());
        let result = self.run_command(body, false);
        self.var_scopes.pop();
        if let Some(frame) = self.nameref_local_stack.pop() {
            for (name, was_nameref) in frame.into_iter().rev() {
                if !was_nameref {
                    self.nameref_names.remove(&name);
                }
            }
        }
        if let Some(frame) = self.array_local_stack.pop() {
            for (name, prev) in frame.into_iter().rev() {
                match prev {
                    Some(v) => {
                        self.arrays.insert(name, v);
                    }
                    None => {
                        self.arrays.remove(&name);
                    }
                }
            }
        }
        if let Some(frame) = self.assoc_local_stack.pop() {
            for (name, prev) in frame.into_iter().rev() {
                match prev {
                    Some(v) => {
                        self.assoc_arrays.insert(name, v);
                    }
                    None => {
                        self.assoc_arrays.remove(&name);
                        self.assoc_names.remove(&name);
                    }
                }
            }
        }
        self.arg_frames.pop();
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
        if ran_body {
            ExecResult::Status(self.last_status)
        } else {
            ExecResult::Status(0)
        }
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
        print_menu(self, &items);
        loop {
            let ps3 = {
                let v = self.lookup_var("PS3");
                if v.is_empty() { "#? ".to_string() } else { v }
            };
            sh_eprint!(self, "{}", ps3);
            let _ = std::io::Write::flush(&mut std::io::stderr());
            let mut line = String::new();
            match std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line) {
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
        if ran_body {
            ExecResult::Status(self.last_status)
        } else {
            ExecResult::Status(0)
        }
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
            let should_run = force_run
                || patterns.iter().any(|p| {
                    let pat = self.expand_word(p);
                    glob::matches(&pat, &val)
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

    // `[[ expr ]]`. Real recursive-descent precedence over the flat
    // TestAtom stream the parser built: NOT binds tightest, then simple
    // tests (unary/binary), then AND, then OR (loosest) -- matching bash.
    fn run_test(&mut self, atoms: &[parser::TestAtom]) -> ExecResult {
        let mut pos = 0;
        match self.eval_test_or(atoms, &mut pos) {
            Ok(b) => ExecResult::Status(if b { 0 } else { 1 }),
            Err(e) => {
                sh_eprintln!(self, "bish: [[: {}", e);
                ExecResult::Status(2)
            }
        }
    }

    fn eval_test_or(&mut self, atoms: &[parser::TestAtom], pos: &mut usize) -> Result<bool, String> {
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
                Ok(self.eval_simple_test(&word_atoms))
            }
            other => Err(format!("syntax error near {:?}", other)),
        }
    }

    fn eval_simple_test(&mut self, words: &[&Word]) -> bool {
        match words {
            [] => false,
            [s] => !self.expand_word(s).is_empty(),
            [op, a] => {
                let op = self.expand_word(op);
                let a = self.expand_word(a);
                builtins::unary(&op, &a)
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
                            let map: std::collections::BTreeMap<usize, String> =
                                groups.into_iter().enumerate().collect();
                            self.arrays.insert("BASH_REMATCH".to_string(), map);
                            true
                        }
                        None => {
                            self.arrays.remove("BASH_REMATCH");
                            false
                        }
                    }
                } else {
                    let b = self.expand_word(b);
                    builtins::binary(&a, &op, &b, true)
                }
            }
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
                Chunk::Arith { raw, quoted } => match arith::eval(raw, self) {
                    Ok(v) => {
                        let v = v.to_string();
                        out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                    }
                    Err(e) => sh_eprintln!(self, "bish: (({})): {}", raw, e),
                },
                Chunk::VarExpand { name, op, quoted } => {
                    let name = name.clone();
                    let op = op.clone();
                    let v = self.eval_var_op(&name, &op);
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
                    let v = self.eval_array_var_op(&name, &index, &op);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::Indirect { name, quoted } => {
                    let target = self.lookup_var(name);
                    let v = self.lookup_var(&target);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::ArrayKeys { name, quoted } => {
                    let name = name.clone();
                    let v = self.array_keys(&name).join(" ");
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::VarNamesMatchingPrefix { prefix, quoted, .. } => {
                    let prefix = prefix.clone();
                    let v = self.var_names_with_prefix(&prefix).join(" ");
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

    fn run_single(&mut self, cmd: &SimpleCommand, background: bool) -> ExecResult {
        if cmd.words.is_empty() {
            for (name, mode, val) in &cmd.assigns {
                let v = self.expand_word(val);
                match mode {
                    AssignMode::Set => self.assign_var(name, v),
                    AssignMode::Append => {
                        let mut cur = self.lookup_var(name);
                        cur.push_str(&v);
                        self.assign_var(name, cur);
                    }
                }
            }
            for (name, mode, items) in &cmd.array_assigns {
                self.apply_array_literal(name, *mode, items);
            }
            for (name, index, val) in &cmd.index_assigns {
                let v = self.expand_word(val);
                self.array_set_index(name, index, v);
            }
            if !cmd.redirects.is_empty() {
                // side effect only: create/truncate/append the target files
                let _ = self.resolve_redirects(cmd);
            }
            if let Some(exit) = self.take_pending_exit() {
                return exit;
            }
            return ExecResult::Status(0);
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
        let argv: Vec<String> = if matches!(
            first_word_literal,
            Some("local") | Some("export") | Some("declare") | Some("typeset") | Some("readonly")
        ) {
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
        if let Some(exit) = self.take_pending_exit() {
            return exit;
        }
        if argv.is_empty() {
            // Every word vanished (e.g. the command was just an unquoted
            // empty/unset variable) -- matches bash: nothing runs.
            return ExecResult::Status(0);
        }
        if self.opt_xtrace {
            sh_eprintln!(self, "+ {}", argv.join(" "));
        }
        let name = argv[0].clone();

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
            return self.call_function(&body, argv[1..].to_vec());
        }
        self.dispatch_builtin_or_external(&argv, name, cmd, background, false, &array_literal_args)
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
        let pushed = match self.push_builtin_output_sink(&cmd.redirects) {
            Ok(pushed) => pushed,
            Err(e) => {
                sh_eprintln!(self, "bish: {}", e);
                return ExecResult::Status(1);
            }
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
        match name.as_str() {
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
                let mut ext = Command::new(&argv[i]);
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
                        sh_eprintln!(self, "bish: command: {}: {}", argv[i], e);
                        ExecResult::Status(127)
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
            "type" => return ExecResult::Status(self.run_type(&argv[1..])),
            // No command-path cache exists to manage -- every exec
            // re-resolves PATH via Command::status() itself -- so this is
            // a documented no-op rather than a real cache: `hash -r`
            // (clear) and bare `hash cmd` (remember) both just succeed.
            // Bare `hash` with no args normally lists the cache; ours is
            // always empty, so that's the one output bash-compat requires.
            "hash" => {
                if argv.len() == 1 {
                    sh_println!(self, "hash: hash table empty");
                }
                return ExecResult::Status(0);
            }
            "cd" => return ExecResult::Status(self.run_cd(&argv[1..])),
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
            "echo" => return ExecResult::Status(self.run_echo(&argv[1..])),
            "printf" => return ExecResult::Status(self.run_printf(&argv[1..])),
            "umask" => return ExecResult::Status(self.run_umask(&argv[1..])),
            "ulimit" => return ExecResult::Status(self.run_ulimit(&argv[1..])),
            // alias/unalias: store and query only, no expansion when a
            // command runs -- see the comment on the `aliases` field for
            // why.
            "alias" => {
                if argv.len() == 1 {
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
            "abbr" => return ExecResult::Status(self.run_abbr(&argv[1..])),
            "shopt" => return ExecResult::Status(self.run_shopt(&argv[1..])),
            "bishopt" => return ExecResult::Status(self.run_bishopt(&argv[1..], KNOWN_BISHOPTS)),
            // `::bish SUBCOMMAND...`: a dedicated namespace for bish-
            // specific commands that don't belong as an ordinary top-
            // level builtin name (see run_bish's own doc comment for
            // why) -- `theme` (begin/end a theme declaration) is the
            // first subcommand.
            "::bish" => return self.run_bish(&argv[1..]),
            "compgen" => return ExecResult::Status(self.run_compgen(&argv[1..])),
            "complete" => return ExecResult::Status(self.run_complete(&argv[1..])),
            "compopt" => return ExecResult::Status(self.run_compopt(&argv[1..])),
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
                return self.run_window(&argv[1..]);
            }
            "pushd" => return ExecResult::Status(self.run_pushd(&argv[1..])),
            "popd" => return ExecResult::Status(self.run_popd(&argv[1..])),
            "dirs" => return ExecResult::Status(self.run_dirs(&argv[1..])),
            // `export` is equivalent to `declare -x` (real bash documents
            // it that way) -- routing through run_declare means it shares
            // -x's local-variable-mirroring behavior instead of the
            // simpler, env-only handling this had before.
            "export" => {
                let mut declare_args = vec!["-x".to_string()];
                declare_args.extend(argv[1..].iter().cloned());
                // Dropping argv[0] ("export") shifts every recorded
                // position back by one; prepending "-x" here shifts them
                // forward by one again -- net zero, so array_literal_args
                // (itself indexed into the *original* argv) already lines
                // up with declare_args unchanged.
                return ExecResult::Status(self.run_declare(&declare_args, array_literal_args));
            }
            "let" => {
                let mut last = 0i64;
                for a in &argv[1..] {
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
            "test" => return ExecResult::Status(builtins::test(&argv[1..], false)),
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
                return ExecResult::Status(builtins::test(&a, false));
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
                let n = argv.get(1).and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                if let Some(frame) = self.arg_frames.last_mut() {
                    let drain = n.min(frame.len());
                    frame.drain(0..drain);
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
                let shifted_array_literals: Vec<_> = array_literal_args
                    .iter()
                    .filter_map(|(p, n, m, i)| p.checked_sub(1).map(|p2| (p2, n.clone(), *m, i.clone())))
                    .collect();
                for (i, a) in argv[1..].iter().enumerate() {
                    match a.as_str() {
                        "-a" => {
                            array_mode = Some(false);
                            continue;
                        }
                        "-A" => {
                            array_mode = Some(true);
                            continue;
                        }
                        "-i" => {
                            integer_flag = true;
                            continue;
                        }
                        "-n" => {
                            nameref_flag = true;
                            continue;
                        }
                        "-u" => {
                            upper_flag = true;
                            continue;
                        }
                        "-l" => {
                            lower_flag = true;
                            continue;
                        }
                        "-x" => {
                            export_flag = true;
                            continue;
                        }
                        "-g" => {
                            global_flag = true;
                            continue;
                        }
                        _ if a.starts_with('-') => continue,
                        _ => {}
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
                                self.assoc_local_stack.last_mut().unwrap().push((name.clone(), prev));
                                self.assoc_names.insert(name.clone());
                                self.assoc_arrays.insert(name.clone(), OrderedMap::default());
                            }
                            Some(false) => {
                                let prev = self.arrays.remove(name);
                                self.array_local_stack.last_mut().unwrap().push((name.clone(), prev));
                                self.arrays.insert(name.clone(), std::collections::BTreeMap::new());
                            }
                            None => {}
                        }
                        self.apply_array_literal(name, *mode, items);
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
                        self.var_scopes.last_mut().unwrap().insert(n.clone(), v.unwrap_or_default());
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
                        Some(true) => {
                            let prev = self.assoc_arrays.remove(&n);
                            self.assoc_local_stack.last_mut().unwrap().push((n.clone(), prev));
                            self.assoc_names.insert(n.clone());
                            self.assoc_arrays.insert(n, OrderedMap::default());
                        }
                        Some(false) => {
                            let prev = self.arrays.remove(&n);
                            self.array_local_stack.last_mut().unwrap().push((n.clone(), prev));
                            self.arrays.insert(n, std::collections::BTreeMap::new());
                        }
                        None => {
                            if global_flag {
                                self.assign_var_global(&n, v.unwrap_or_default());
                                continue;
                            }
                            let v = v.unwrap_or_default();
                            let v = if integer_flag { arith::eval(&v, self).unwrap_or(0).to_string() } else { v };
                            let v = if upper_flag {
                                v.to_uppercase()
                            } else if lower_flag {
                                v.to_lowercase()
                            } else {
                                v
                            };
                            if export_flag {
                                unsafe {
                                    std::env::set_var(&n, &v);
                                }
                            }
                            self.var_scopes.last_mut().unwrap().insert(n, v);
                        }
                    }
                }
                return ExecResult::Status(0);
            }
            "exit" => {
                let code = argv
                    .get(1)
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(self.last_status);
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
                let mut i = 1;
                while i < argv.len() {
                    match argv[i].as_str() {
                        "-r" => i += 1,
                        "-s" => {
                            silent = true;
                            i += 1;
                        }
                        "-a" => {
                            array_name = argv.get(i + 1).map(|s| s.as_str());
                            i += 2;
                        }
                        "-p" => {
                            prompt = argv.get(i + 1).map(|s| s.as_str());
                            i += 2;
                        }
                        "-n" | "-N" => {
                            nchars = argv.get(i + 1).and_then(|s| s.parse::<usize>().ok());
                            i += 2;
                        }
                        "-d" => {
                            delim = argv.get(i + 1).and_then(|s| s.bytes().next()).unwrap_or(b'\n');
                            i += 2;
                        }
                        "-t" => {
                            // Parsed and consumed for arg-shape compatibility;
                            // actual timeout enforcement happens below, but
                            // only against real stdin (see is_real_stdin).
                            i += 2;
                        }
                        "-u" => {
                            read_u_flag = argv.get(i + 1).map(|s| s.as_str());
                            i += 2;
                        }
                        other => {
                            names.push(other);
                            i += 1;
                        }
                    }
                }
                let timeout_secs = argv.iter().position(|a| a == "-t").and_then(|p| argv.get(p + 1)).and_then(|s| s.parse::<f64>().ok());
                let is_real_stdin = !cmd
                    .redirects
                    .iter()
                    .any(|r| matches!(r, Redirect::In(_) | Redirect::HereString(_) | Redirect::HereDoc(_)));
                if let Some(p) = prompt {
                    if is_real_stdin && stdin_is_tty() {
                        sh_eprint!(self, "{}", p);
                        let _ = std::io::Write::flush(&mut std::io::stderr());
                    }
                }
                if let Some(secs) = timeout_secs {
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
                        Some(KeptFd::Read(r)) => read_line_or_chars(r, nchars, delim),
                        _ => {
                            sh_eprintln!(self, "bish: read: {}: invalid file descriptor", fd);
                            return ExecResult::Status(1);
                        }
                    }
                } else {
                    let mut reader = self.read_input_source(cmd);
                    read_line_or_chars(&mut *reader, nchars, delim)
                };

                return match got {
                    None => ExecResult::Status(1),
                    Some(line) => {
                        let line = line.as_str();
                        let ifs = self.get_ifs();
                        if let Some(arr) = array_name {
                            let (parts, ..) = ifs_tokenize(line, &ifs);
                            let map: std::collections::BTreeMap<usize, String> =
                                parts.into_iter().enumerate().collect();
                            self.arrays.insert(arr.to_string(), map);
                        } else if names.is_empty() {
                            self.assign_var("REPLY", line.to_string());
                        } else {
                            let is_ifs_ws = |c: char| c.is_whitespace() && ifs.contains(c);
                            let mut rest = line.trim_start_matches(is_ifs_ws).to_string();
                            for (i, n) in names.iter().enumerate() {
                                if i == names.len() - 1 {
                                    self.assign_var(n, rest.trim_end_matches(is_ifs_ws).to_string());
                                } else {
                                    match ifs_next_field(&rest, &ifs) {
                                        Some((field, remainder)) => {
                                            self.assign_var(n, field);
                                            rest = remainder;
                                        }
                                        None => {
                                            self.assign_var(n, rest.clone());
                                            rest = String::new();
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
                for a in &argv[1..] {
                    match a.as_str() {
                        "-t" => strip_newline = true,
                        other if !other.starts_with('-') => array_name = other.to_string(),
                        _ => {}
                    }
                }
                let mut reader = self.read_input_source(cmd);
                let mut map = std::collections::BTreeMap::new();
                let mut idx = 0usize;
                loop {
                    let mut line = String::new();
                    match std::io::BufRead::read_line(&mut *reader, &mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if strip_newline {
                                line = line.trim_end_matches(['\n', '\r']).to_string();
                            }
                            map.insert(idx, line);
                            idx += 1;
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
                match std::fs::read_to_string(&path) {
                    Ok(src) => return self.run_source_here(&src, &path),
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}: {}", path, e);
                        return ExecResult::Status(1);
                    }
                }
            }
            "trap" => {
                if argv.len() == 1 || argv.get(1).map(|s| s == "-p").unwrap_or(false) {
                    if let Some(code) = &self.exit_trap {
                        sh_println!(self, "trap -- {} EXIT", crate::serialize::quote_literal(code));
                    }
                    let mut entries: Vec<(i32, TrapAction)> =
                        self.traps.iter().map(|(k, v)| (*k, v.clone())).collect();
                    entries.sort_by_key(|(n, _)| *n);
                    for (n, action) in entries {
                        match action {
                            TrapAction::Run(code) => {
                                sh_println!(self, "trap -- {} SIG{}", crate::serialize::quote_literal(&code), signal_name(n))
                            }
                            TrapAction::Ignore => sh_println!(self, "trap -- '' SIG{}", signal_name(n)),
                        }
                    }
                    return ExecResult::Status(0);
                }
                if argv.len() < 3 {
                    return ExecResult::Status(0);
                }
                let cmd_str = argv[1].clone();
                for sig in &argv[2..] {
                    if sig == "EXIT" || sig == "0" {
                        self.exit_trap = if cmd_str == "-" { None } else { Some(cmd_str.clone()) };
                        continue;
                    }
                    let num = match signal_number(sig) {
                        Some(n) => n,
                        None => {
                            sh_eprintln!(self, "bish: trap: {}: invalid signal specification", sig);
                            continue;
                        }
                    };
                    if num == 9 || num == 19 {
                        sh_eprintln!(self, "bish: trap: {}: cannot trap", sig);
                        continue;
                    }
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
                return ExecResult::Status(0);
            }
            "jobs" => return ExecResult::Status(self.run_jobs(&argv[1..])),
            "disown" => return ExecResult::Status(self.run_disown(&argv[1..])),
            "fg" => return self.run_fg(&argv[1..]),
            "bg" => return ExecResult::Status(self.run_bg(&argv[1..])),
            "wait" => return ExecResult::Status(self.run_wait(&argv[1..])),
            "kill" => return ExecResult::Status(self.run_kill(&argv[1..])),
            "getopts" => return self.run_getopts(&argv[1..]),
            "unset" => {
                let target = self.peek_stderr_target(&cmd.redirects);
                return ExecResult::Status(self.run_unset(&argv[1..], &target));
            }
            "set" => return ExecResult::Status(self.run_set(&argv[1..])),
            "declare" | "typeset" => {
                // array_literal_args is indexed into the *original* argv
                // (which still has argv[0] == "declare"/"typeset"), so
                // every position shifts back by one to line up with
                // argv[1..].
                let shifted: Vec<_> = array_literal_args
                    .iter()
                    .filter_map(|(p, n, m, i)| p.checked_sub(1).map(|p2| (p2, n.clone(), *m, i.clone())))
                    .collect();
                return ExecResult::Status(self.run_declare(&argv[1..], &shifted));
            }
            "readonly" => return ExecResult::Status(self.run_readonly(&argv[1..])),
            // exec CMD [args...] replaces this process image entirely (no
            // fork, no return on success) -- exactly what real bash does,
            // and available here as safe std (CommandExt::exec wraps
            // execvp, distinct from the fork() this shell avoids).
            "exec" if argv.len() > 1 => {
                if self.opt_restricted {
                    sh_eprintln!(self, "bish: exec: restricted");
                    return ExecResult::Status(1);
                }
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
                    let mut command = Command::new(&argv[1]);
                    command.args(&argv[2..]);
                    command.current_dir(&self.cwd);
                    command.stdin(redirs.stdin.unwrap_or_else(|| self.spawn_stdin_stdio()));
                    command.stdout(redirs.stdout.unwrap_or_else(|| self.spawn_stdout_stdio()));
                    command.stderr(redirs.stderr.unwrap_or_else(Stdio::inherit));
                    apply_fd_redirects(&mut command, redirs.dup_stderr_to_stdout, redirs.extra_fds);
                    self.note_external_spawn();
                    let status = match command.status() {
                        Ok(status) => exit_code_from_status(status),
                        // Same "leave $? at whatever it already was"
                        // behavior as the real top-level case just below.
                        Err(e) => {
                            sh_eprintln!(self, "bish: exec: {}: {}", argv[1], e);
                            self.last_status
                        }
                    };
                    self.run_exit_trap();
                    return ExecResult::Exit(status);
                }
                let mut command = Command::new(&argv[1]);
                command.args(&argv[2..]);
                if let Some(s) = redirs.stdin {
                    command.stdin(s);
                }
                if let Some(s) = redirs.stdout {
                    command.stdout(s);
                }
                if let Some(s) = redirs.stderr {
                    command.stderr(s);
                }
                // Numbered-fd forms (`exec cmd 3>file`) can't go through
                // apply_fd_redirects' pre_exec hook -- CommandExt::exec
                // below is a direct execve of *this* process, no fork, so
                // there's no child to install a pre_exec closure into.
                if let Err(e) = apply_fds_to_self(redirs.dup_stderr_to_stdout, redirs.extra_fds) {
                    sh_eprintln!(self, "bish: exec: {}", e);
                    return ExecResult::Status(1);
                }
                let err = command.exec();
                sh_eprintln!(self, "bish: exec: {}: {}", argv[1], err);
                // Real bash: a non-interactive shell exits immediately when
                // exec fails to find/run the command, and (confirmed via a
                // clean probe against bash 5.0.17) leaves $? at whatever it
                // already was rather than setting 127 -- surprising, but
                // that's what it actually does.
                self.run_exit_trap();
                return ExecResult::Exit(self.last_status);
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
                let (stdin, stdout, stderr) = match self.resolve_plain_fd012(&cmd.redirects) {
                    Ok(f) => f,
                    Err(e) => {
                        sh_eprintln!(self, "bish: {}", e);
                        return ExecResult::Status(1);
                    }
                };
                if let Err(e) = apply_fd012_to_self(stdin, stdout, stderr) {
                    sh_eprintln!(self, "bish: exec: {}", e);
                    return ExecResult::Status(1);
                }
                if let Err(e) = apply_fds_to_self(redirs.dup_stderr_to_stdout, redirs.extra_fds) {
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

        let mut command = Command::new(&name);
        command.args(&argv[1..]);
        command.current_dir(&self.cwd);
        for (k, mode, val) in &cmd.assigns {
            let v = self.expand_word(val);
            let v = match mode {
                AssignMode::Set => v,
                AssignMode::Append => self.lookup_var(k) + &v,
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
        let use_pty = self.is_promoted()
            && redirs.stdin.is_none()
            && redirs.stdout.is_none()
            && redirs.stderr.is_none()
            && redirs.extra_fds.is_empty();

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
            command.stdin(redirs.stdin.or_else(|| bg_slave(&bg_pty)).unwrap_or_else(Stdio::inherit));
            command.stdout(redirs.stdout.or_else(|| bg_slave(&bg_pty)).unwrap_or_else(Stdio::inherit));
        } else {
            command.stdin(redirs.stdin.unwrap_or_else(|| self.spawn_stdin_stdio()));
            command.stdout(redirs.stdout.unwrap_or_else(|| self.spawn_stdout_stdio()));
        }
        command.stderr(redirs.stderr.or_else(|| bg_slave(&bg_pty)).unwrap_or_else(Stdio::inherit));
        apply_fd_redirects(&mut command, redirs.dup_stderr_to_stdout, redirs.extra_fds);

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
                    let result = match child.wait() {
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
            Err(e) => {
                let msg = format!("bish: {}: {}", name, e);
                self.write_command_error(cmd, &msg);
                self.drain_proc_subs();
                ExecResult::Status(127)
            }
        }
    }

    // Every pipeline stage is a separate process by necessity (that's what
    // makes piping possible at all), so compound-command stages self-exec
    // just like Subshell already does -- this is actually the *correct*
    // bash semantics too: piped stages always fork, even in real bash.
    fn run_multi(&mut self, commands: &[parser::Command], background: bool) -> i32 {
        let n = commands.len();
        let mut children: Vec<std::process::Child> = Vec::with_capacity(n);
        let mut prev_stdout: Option<Stdio> = None;
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

        // A backgrounded pipeline's own pty (see background_pty): the
        // first stage reads from it, the last writes to it, and every
        // stage's stderr goes there too -- inherited, any of that would
        // land straight on the real screen for the compositor to wipe.
        // Wired as plain fds rather than via spawn_attached, whose setsid
        // would undo the process-group isolation set up just below (and
        // which `kill %N`/`bg` need to reach every stage at once).
        let bg_pty = if background { self.background_pty() } else { None };
        let bg_slave = |pty: &Option<pty::Pty>| -> Option<Stdio> {
            let p = pty.as_ref()?;
            std::fs::OpenOptions::new().read(true).write(true).open(&p.slave_path).ok().map(Stdio::from)
        };

        for (i, cmd) in commands.iter().enumerate() {
            let is_last = i == n - 1;
            // Only the first stage takes the pty as stdin -- every other
            // one reads the previous stage's pipe, exactly as before.
            let default_stdin = match prev_stdout.take() {
                Some(prev) => prev,
                None => bg_slave(&bg_pty).unwrap_or_else(|| self.spawn_stdin_stdio()),
            };
            let default_stdout = if is_last {
                bg_slave(&bg_pty).unwrap_or_else(|| self.spawn_stdout_stdio())
            } else {
                Stdio::piped()
            };
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
                    let mut command = if is_known_builtin(&argv[0]) || self.functions.contains_key(&argv[0]) {
                        let exe = match std::env::current_exe() {
                            Ok(p) => p,
                            Err(e) => {
                                sh_eprintln!(self, "bish: {}", e);
                                kill_all(children);
                                return 1;
                            }
                        };
                        let script_line: String = argv
                            .iter()
                            .map(|a| crate::serialize::quote_literal(a))
                            .collect::<Vec<_>>()
                            .join(" ");
                        let script = self.functions_preamble() + &script_line;
                        let mut command = Command::new(exe);
                        command.arg("-c").arg(script);
                        command
                    } else {
                        let mut command = Command::new(&argv[0]);
                        command.args(&argv[1..]);
                        command
                    };
                    for (k, mode, val) in &sc.assigns {
                        let v = self.expand_word(val);
                        let v = match mode {
                            AssignMode::Set => v,
                            AssignMode::Append => self.lookup_var(k) + &v,
                        };
                        command.env(k, v);
                    }
                    command.stdin(redirs.stdin.unwrap_or(default_stdin));
                    command.stdout(redirs.stdout.unwrap_or(default_stdout));
                    command.stderr(redirs.stderr.or_else(|| default_stderr.take()).unwrap_or_else(Stdio::inherit));
                    apply_fd_redirects(&mut command, redirs.dup_stderr_to_stdout, redirs.extra_fds);
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
                        ResolvedRedirs {
                            stdin: None,
                            stdout: None,
                            stderr: None,
                            dup_stderr_to_stdout: false,
                            extra_fds: Vec::new(),
                        }
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
                    let mut command = Command::new(exe);
                    command.arg("-c").arg(script);
                    command.stdin(redirs.stdin.unwrap_or(default_stdin));
                    command.stdout(redirs.stdout.unwrap_or(default_stdout));
                    command.stderr(redirs.stderr.or_else(|| default_stderr.take()).unwrap_or_else(Stdio::inherit));
                    apply_fd_redirects(&mut command, redirs.dup_stderr_to_stdout, redirs.extra_fds);
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
                        prev_stdout = child.stdout.take().map(Stdio::from);
                    }
                    children.push(child);
                }
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    kill_all(children);
                    return 127;
                }
            }
        }

        if background {
            let cmd_text =
                commands.iter().map(crate::serialize::serialize_command).collect::<Vec<_>>().join(" | ");
            // Both the pgid (so `kill %N`/`bg` reach every stage at once)
            // and the output pty, which push_job_full needs to record for
            // the drain to find later.
            self.push_job_full(children, cmd_text, bg_pty.map(|p| p.master), pgid.map(|p| p as u32));
            return 0;
        }

        let mut status = 0;
        let mut pipefail_status = 0;
        for mut c in children {
            let code = match c.wait() {
                Ok(s) => exit_code_from_status(s),
                Err(e) => {
                    sh_eprintln!(self, "bish: {}", e);
                    1
                }
            };
            status = code;
            if code != 0 {
                pipefail_status = code;
            }
        }
        if self.opt_pipefail {
            pipefail_status
        } else {
            status
        }
    }

    fn expand_word(&mut self, w: &Word) -> String {
        let mut s = String::new();
        for c in &w.chunks {
            match c {
                Chunk::Str(t) | Chunk::LiteralStr(t) => s.push_str(t),
                Chunk::Var { name, .. } => {
                    let name = name.clone();
                    self.check_nounset(&name);
                    s.push_str(&self.lookup_var(&name));
                }
                Chunk::Sub { raw, .. } => s.push_str(&self.run_command_substitution(raw)),
                Chunk::Arith { raw, .. } => match arith::eval(raw, self) {
                    Ok(v) => s.push_str(&v.to_string()),
                    Err(e) => sh_eprintln!(self, "bish: (({})): {}", raw, e),
                },
                Chunk::VarExpand { name, op, .. } => {
                    let name = name.clone();
                    let op = op.clone();
                    s.push_str(&self.eval_var_op(&name, &op));
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
                    s.push_str(&self.eval_array_var_op(&name, &index, &op));
                }
                Chunk::Indirect { name, .. } => {
                    let target = self.lookup_var(name);
                    s.push_str(&self.lookup_var(&target));
                }
                Chunk::ArrayKeys { name, .. } => {
                    let name = name.clone();
                    s.push_str(&self.array_keys(&name).join(" "));
                }
                Chunk::VarNamesMatchingPrefix { prefix, .. } => {
                    let prefix = prefix.clone();
                    s.push_str(&self.var_names_with_prefix(&prefix).join(" "));
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
    fn expand_index_as_string(&mut self, index: &str) -> String {
        self.expand_raw(index)
    }

    // Negative indices count back from the end: bash defines them as
    // relative to one greater than the array's maximum set index, so -1 is
    // the last (highest-index) element. Only meaningful for indexed arrays
    // -- associative-array indices are plain string keys, never resolved
    // here.
    fn resolve_array_index(&self, name: &str, i: i64) -> Option<usize> {
        if i >= 0 {
            return Some(i as usize);
        }
        let max = *self.arrays.get(name)?.keys().next_back()?;
        let resolved = max as i64 + 1 + i;
        if resolved >= 0 {
            Some(resolved as usize)
        } else {
            None
        }
    }

    fn array_element(&mut self, name: &str, index: &str) -> String {
        if index == "@" || index == "*" {
            return self.array_all(name).join(" ");
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
        if let Some(m) = self.assoc_arrays.get(name) {
            return m.keys().cloned().collect();
        }
        self.arrays.get(name).map(|m| m.keys().map(|k| k.to_string()).collect()).unwrap_or_default()
    }

    fn array_all(&self, name: &str) -> Vec<String> {
        if let Some(m) = self.assoc_arrays.get(name) {
            return m.values().cloned().collect();
        }
        self.arrays.get(name).map(|m| m.values().cloned().collect()).unwrap_or_default()
    }

    // "@"/"*" counts only set elements (real bash arrays are sparse --
    // `arr[10]=x` alone gives a length of 1, not 11).
    fn array_length(&mut self, name: &str, index: &str) -> usize {
        if index == "@" || index == "*" {
            if let Some(m) = self.assoc_arrays.get(name) {
                return m.len();
            }
            return self.arrays.get(name).map(|m| m.len()).unwrap_or(0);
        }
        if self.assoc_names.contains(name) {
            let key = self.expand_index_as_string(index);
            return self
                .assoc_arrays
                .get(name)
                .and_then(|m| m.get(&key))
                .map(|s| s.chars().count())
                .unwrap_or(0);
        }
        match arith::eval(index, self) {
            Ok(i) => match self.resolve_array_index(name, i) {
                Some(idx) => self
                    .arrays
                    .get(name)
                    .and_then(|m| m.get(&idx))
                    .map(|s| s.chars().count())
                    .unwrap_or(0),
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
    fn array_set_index(&mut self, name: &str, index: &str, value: String) -> Option<usize> {
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
    fn apply_array_literal(&mut self, name: &str, mode: AssignMode, items: &[ArrayLiteralItem]) {
        let is_assoc = self.assoc_names.contains(name);
        if mode == AssignMode::Set {
            if is_assoc {
                self.assoc_arrays.insert(name.to_string(), OrderedMap::default());
            } else {
                self.arrays.insert(name.to_string(), std::collections::BTreeMap::new());
            }
        }
        let mut next_index: usize = match mode {
            AssignMode::Append if !is_assoc => {
                self.arrays.get(name).and_then(|m| m.keys().next_back()).map(|k| k + 1).unwrap_or(0)
            }
            _ => 0,
        };
        for item in items {
            match item {
                ArrayLiteralItem::Positional(w) => {
                    let v = self.expand_word(w);
                    if is_assoc {
                        self.assoc_arrays.entry(name.to_string()).or_default().insert(next_index.to_string(), v);
                    } else {
                        self.arrays.entry(name.to_string()).or_default().insert(next_index, v);
                    }
                    next_index += 1;
                }
                ArrayLiteralItem::Keyed(index, w) => {
                    let v = self.expand_word(w);
                    if let Some(idx) = self.array_set_index(name, index, v) {
                        next_index = idx + 1;
                    }
                }
            }
        }
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
                ArrayLiteralItem::Positional(w) => crate::serialize::quote_literal(&self.expand_word(w)),
                ArrayLiteralItem::Keyed(index, w) => format!("[{}]={}", index, crate::serialize::quote_literal(&self.expand_word(w))),
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
                    let v = match arith::eval(raw, self) {
                        Ok(n) => n.to_string(),
                        Err(e) => {
                            sh_eprintln!(self, "bish: (({})): {}", raw, e);
                            String::new()
                        }
                    };
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::VarExpand { name, op, quoted } => {
                    let name = name.clone();
                    let op = op.clone();
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
                        let joined = self.array_all(name).join(" ");
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
                    let v = self.eval_array_var_op(&name, &index, &op);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::Indirect { name, quoted } => {
                    let target = self.lookup_var(name);
                    let v = self.lookup_var(&target);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::ArrayKeys { name, quoted } => {
                    // Same @-vs-* / quoted-vs-not splitting rules as
                    // ${arr[@]}: "@" quoted is one field per key.
                    if *quoted {
                        let items = self.array_keys(name);
                        append_parts_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &items);
                    } else {
                        let joined = self.array_keys(name).join(" ");
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
    fn get_ifs(&mut self) -> String {
        if self.var_is_set("IFS") {
            self.lookup_var("IFS")
        } else {
            " \t\n".to_string()
        }
    }

    // Re-lexes and expands a captured raw operand (the "word"/"pattern"
    // half of a ${...} expansion), so it can itself contain further $
    // expansions. Parsed as a single word (see parse_expansion_word) --
    // unlike a command line, unquoted whitespace inside it is literal
    // content, not a field separator.
    fn expand_raw(&mut self, raw: &str) -> String {
        let chunks = crate::lexer::parse_expansion_word(raw);
        self.expand_word(&Word { chunks, globbable: false })
    }

    fn eval_var_op(&mut self, name: &str, op: &VarOp) -> String {
        let cur = self.lookup_var(name);
        match op {
            VarOp::Length => cur.chars().count().to_string(),
            VarOp::Default { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.var_is_set(name) };
                if trigger {
                    self.expand_raw(word)
                } else {
                    cur
                }
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
                    let msg = self.expand_raw(word);
                    sh_eprintln!(self, "bish: {}: {}", name, msg);
                    String::new()
                } else {
                    cur
                }
            }
            VarOp::AltIfSet { word, colon } => {
                let set_enough = if *colon { !cur.is_empty() } else { self.var_is_set(name) };
                if set_enough {
                    self.expand_raw(word)
                } else {
                    String::new()
                }
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
                let off = arith::eval(offset, self).unwrap_or(0);
                let len = length.as_ref().and_then(|l| arith::eval(l, self).ok());
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
                TransformKind::Quote | TransformKind::Upper | TransformKind::Lower | TransformKind::Escape => {
                    apply_transform(&cur, *kind)
                }
            },
        }
    }

    // Same operators as eval_var_op, but reading (and, for :=, writing)
    // one array element instead of a scalar variable. "@"/"*" indices are
    // treated as the joined-all-elements string, matching how they behave
    // as a plain (non-splitting-aware) expansion elsewhere.
    fn eval_array_var_op(&mut self, name: &str, index: &str, op: &VarOp) -> String {
        let cur = self.array_element(name, index);
        match op {
            VarOp::Length => cur.chars().count().to_string(),
            VarOp::Default { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.array_element_is_set(name, index) };
                if trigger {
                    self.expand_raw(word)
                } else {
                    cur
                }
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
                    let msg = self.expand_raw(word);
                    sh_eprintln!(self, "bish: {}[{}]: {}", name, index, msg);
                    String::new()
                } else {
                    cur
                }
            }
            VarOp::AltIfSet { word, colon } => {
                let set_enough = if *colon { !cur.is_empty() } else { self.array_element_is_set(name, index) };
                if set_enough {
                    self.expand_raw(word)
                } else {
                    String::new()
                }
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
                let off = arith::eval(offset, self).unwrap_or(0);
                let len = length.as_ref().and_then(|l| arith::eval(l, self).ok());
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
                    if index == "@" || index == "*" {
                        self.transform_attributes(name, None)
                    } else {
                        self.transform_attributes(name, Some(&cur))
                    }
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
                TransformKind::Quote | TransformKind::Upper | TransformKind::Lower | TransformKind::Escape => {
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
    fn expand_words(&mut self, words: &[Word]) -> Vec<String> {
        let mut out = Vec::new();
        for w in words {
            if w.globbable {
                // globbable implies no quoting/expansion at all in the word
                // (see Word::globbable), so splitting can't apply here --
                // glob-check the single literal value as before.
                let s = self.expand_word(w);
                if !self.opt_noglob {
                    if let Some(matches) = glob::expand(&s) {
                        out.extend(matches);
                        continue;
                    }
                }
                out.push(s);
            } else {
                let (fields, patterns) = self.expand_word_split(w);
                if self.opt_noglob {
                    out.extend(fields);
                } else {
                    for (f, p) in fields.into_iter().zip(patterns.into_iter()) {
                        match glob::expand(&p) {
                            Some(matches) => out.extend(matches),
                            None => out.push(f),
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
    fn raw_var_lookup(&self, name: &str) -> String {
        for scope in self.var_scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return v.clone();
            }
        }
        std::env::var(name).unwrap_or_default()
    }

    // Writes a name's own raw value, bypassing nameref redirection -- used
    // to set a nameref's target-name string itself (assign_var, by
    // contrast, is what a nameref's *reads/writes* get redirected through).
    fn raw_var_write(&mut self, name: &str, value: String) {
        for scope in self.var_scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), value);
                return;
            }
        }
        unsafe {
            std::env::set_var(name, value);
        }
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

    fn lookup_var(&mut self, name: &str) -> String {
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
            "@" | "*" => self.arg_frames.last().map(|a| a.join(" ")).unwrap_or_default(),
            // $$/$!/$RANDOM/$SECONDS/$- are always computed live, never
            // read back from var_scopes/env, matching how bash treats them
            // as effectively magic rather than ordinary settable
            // variables (SECONDS is the one partial exception -- see
            // assign_var's special-case for `SECONDS=n`).
            "$" => std::process::id().to_string(),
            "!" => self.jobs.borrow().last_bg_pid.map(|p| p.to_string()).unwrap_or_default(),
            "RANDOM" => self.next_random().to_string(),
            "SECONDS" => (self.shell_start.elapsed().as_secs() as i64 + self.seconds_offset).to_string(),
            "-" => {
                let mut s = String::new();
                if self.opt_errexit {
                    s.push('e');
                }
                if self.opt_noglob {
                    s.push('f');
                }
                if self.opt_nounset {
                    s.push('u');
                }
                if self.opt_xtrace {
                    s.push('x');
                }
                if self.opt_restricted {
                    s.push('r');
                }
                s
            }
            _ if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) => {
                let idx: usize = name.parse().unwrap_or(0);
                idx.checked_sub(1)
                    .and_then(|i| self.arg_frames.last().and_then(|a| a.get(i)))
                    .cloned()
                    .unwrap_or_default()
            }
            _ => {
                for scope in self.var_scopes.iter().rev() {
                    if let Some(v) = scope.get(name) {
                        return v.clone();
                    }
                }
                if let Ok(v) = std::env::var(name) {
                    return v;
                }
                // Startup-populated-in-real-bash variables: computed on
                // demand here instead, but still overridable by a normal
                // assignment (checked above) since that's the common bash
                // behavior for all of these once actually set.
                match name {
                    "BASH_VERSION" => "5.2.21(1)-release".to_string(),
                    "PPID" => unsafe { getppid() }.to_string(),
                    "UID" => unsafe { getuid() }.to_string(),
                    "EUID" => unsafe { geteuid() }.to_string(),
                    "HOSTNAME" => get_hostname(),
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
                return Some(v.clone());
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
        std::env::var(name).ok()
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
        for k in &current {
            if !self.env_snapshot.contains_key(k) {
                unsafe { std::env::remove_var(k) };
            }
        }
        for (k, v) in &self.env_snapshot {
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
        self.env_snapshot = std::env::vars().collect();
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
        if !term.is_empty() {
            self.env_snapshot.insert("TERM".to_string(), term.to_string());
        }
        if colorterm.is_empty() {
            self.env_snapshot.remove("COLORTERM");
        } else {
            self.env_snapshot.insert("COLORTERM".to_string(), colorterm.to_string());
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
        if is_known_builtin(name) {
            sh_println!(self, "{}", if verbose { format!("{} is a shell builtin", name) } else { name.to_string() });
            return 0;
        }
        match resolve_in_path(name) {
            Some(p) => {
                sh_println!(self, "{}", if verbose { format!("{} is {}", name, p) } else { p });
                0
            }
            None => 1,
        }
    }

    // type [-p] name... A scoped subset of real bash's `type`: reports
    // function/builtin/PATH-resolved-executable, or "not found" (status
    // 1). `-a`/`-t` are accepted but not distinguished from the default.
    fn run_type(&mut self, args: &[String]) -> i32 {
        let mut path_only = false;
        let mut names: Vec<&String> = Vec::new();
        for a in args {
            match a.as_str() {
                "-p" | "-P" => path_only = true,
                "-a" | "-t" => {}
                _ => names.push(a),
            }
        }
        let mut status = 0;
        for name in names {
            if self.functions.contains_key(name.as_str()) {
                if !path_only {
                    sh_println!(self, "{} is a function", name);
                }
                continue;
            }
            if is_known_builtin(name) {
                if !path_only {
                    sh_println!(self, "{} is a shell builtin", name);
                }
                continue;
            }
            match resolve_in_path(name) {
                Some(p) => sh_println!(self, "{}", if path_only { p } else { format!("{} is {}", name, p) }),
                None => {
                    sh_eprintln!(self, "bish: type: {}: not found", name);
                    status = 1;
                }
            }
        }
        status
    }

    fn next_random(&mut self) -> u32 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        ((x >> 33) % 32768) as u32
    }

    fn check_nounset(&mut self, name: &str) {
        if !self.opt_nounset {
            return;
        }
        if self.var_is_set(name) {
            return;
        }
        sh_eprintln!(self, "bish: {}: unbound variable", name);
        self.pending_exit = Some(1);
    }

    // Whether `name` is a variable that's actually been assigned, as
    // opposed to merely evaluating to an empty string -- the distinction
    // `${V-x}`/`${V+x}` (unset only) need vs. `${V:-x}`/`${V:+x}` (unset OR
    // empty). Special/positional parameters always count as set.
    fn var_is_set(&self, name: &str) -> bool {
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
                | "SECONDS"
                | "BASH_VERSION"
                | "PPID"
                | "UID"
                | "EUID"
                | "HOSTNAME"
        ) || (!name.is_empty() && name.chars().all(|c| c.is_ascii_digit()));
        if is_special {
            return true;
        }
        for scope in &self.var_scopes {
            if scope.contains_key(name) {
                return true;
            }
        }
        std::env::var(name).is_ok()
    }

    // Plain assignment targets the global (process-env) variable, unless it
    // shadows an existing `local` of the same name in the current function
    // scope -- matching bash, where functions don't auto-localize vars.
    fn assign_var(&mut self, name: &str, value: String) {
        self.assign_var_impl(name, value, false);
    }

    // `declare -g`/`local -g`: same readonly guard, SECONDS/RANDOM
    // specials, integer/case-fold attributes, and export mirroring as
    // assign_var, but always writes straight to the true global
    // (process-env) scope, bypassing any same-named local shadow in the
    // current function -- unlike assign_var/raw_var_write, which target
    // whichever scope already shadows the name.
    fn assign_var_global(&mut self, name: &str, value: String) {
        self.assign_var_impl(name, value, true);
    }

    fn assign_var_impl(&mut self, name: &str, value: String, force_global: bool) {
        let resolved;
        let name = if self.nameref_names.contains(name) {
            resolved = self.resolve_nameref(name);
            resolved.as_str()
        } else {
            name
        };
        if self.readonly_names.contains(name) || self.is_restricted_readonly_name(name) {
            sh_eprintln!(self, "bish: {}: readonly variable", name);
            return;
        }
        // `SECONDS=n` resets the elapsed-time counter to start counting
        // from n, matching bash -- lookup_var computes it live rather
        // than storing it, so the assignment records an offset instead of
        // writing a var_scopes/env entry.
        if name == "SECONDS" {
            if let Ok(n) = value.trim().parse::<i64>() {
                self.seconds_offset = n - self.shell_start.elapsed().as_secs() as i64;
            }
            return;
        }
        // `RANDOM=n` reseeds the generator, matching bash (rather than
        // making $RANDOM a static value forever).
        if name == "RANDOM" {
            if let Ok(n) = value.trim().parse::<u64>() {
                self.rng_state = if n == 0 { 0x2545F4914F6CDD1D } else { n };
            }
            return;
        }
        // `declare -i`/`local -i`: the assigned text is evaluated as an
        // arithmetic expression rather than stored literally (bash: `n="2+3"`
        // on an integer-attribute variable stores 5, not the string "2+3").
        let value =
            if self.integer_names.contains(name) { arith::eval(&value, self).unwrap_or(0).to_string() } else { value };
        // `declare -u`/`-l`: case-fold on every assignment.
        let value = if self.upper_names.contains(name) {
            value.to_uppercase()
        } else if self.lower_names.contains(name) {
            value.to_lowercase()
        } else {
            value
        };
        if force_global {
            // Bypass any local shadow entirely -- raw_var_write would
            // just update that shadow instead, same as plain assignment.
            unsafe {
                std::env::set_var(name, &value);
            }
            return;
        }
        if self.exported_names.contains(name) {
            unsafe {
                std::env::set_var(name, &value);
            }
        }
        self.raw_var_write(name, value);
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
    fn push_builtin_output_sink(&mut self, redirects: &[Redirect]) -> Result<bool, String> {
        // A word-based target (`>file`) or a dup onto some other already-
        // open fd (`>&3`, most commonly a fd `exec 3<>/dev/tcp/...` left
        // open) -- last one wins either way, matching real bash's own
        // "later redirects override earlier ones for the same fd" rule,
        // which two separate Option fields (one per shape) couldn't
        // express correctly across a mix of both shapes.
        enum Target {
            Path(String, bool),
            Fd(i32),
        }
        let mut stdout_target: Option<Target> = None;
        let mut stderr_target: Option<Target> = None;
        let mut dup_err_to_out = false;
        let mut dup_out_to_err = false;
        let mut touched = false;
        for r in redirects {
            match r {
                Redirect::Out { word, append } => {
                    stdout_target = Some(Target::Path(self.expand_word(word), *append));
                    dup_out_to_err = false;
                    touched = true;
                }
                Redirect::FdOut { fd, word, append } if *fd == 1 => {
                    stdout_target = Some(Target::Path(self.expand_word(word), *append));
                    dup_out_to_err = false;
                    touched = true;
                }
                Redirect::Err { word, append } => {
                    stderr_target = Some(Target::Path(self.expand_word(word), *append));
                    dup_err_to_out = false;
                    touched = true;
                }
                Redirect::FdOut { fd, word, append } if *fd == 2 => {
                    stderr_target = Some(Target::Path(self.expand_word(word), *append));
                    dup_err_to_out = false;
                    touched = true;
                }
                Redirect::Both { word, append } => {
                    stdout_target = Some(Target::Path(self.expand_word(word), *append));
                    dup_err_to_out = true;
                    dup_out_to_err = false;
                    touched = true;
                }
                Redirect::DupErrToOut => {
                    dup_err_to_out = true;
                    touched = true;
                }
                Redirect::FdDup { fd, target } if *fd == 2 && *target == 1 => {
                    dup_err_to_out = true;
                    touched = true;
                }
                Redirect::FdDup { fd, target } if *fd == 1 && *target == 2 => {
                    dup_out_to_err = true;
                    touched = true;
                }
                // Any other numbered-fd dup onto stdout/stderr (`>&3`,
                // `2>&4`, ...) -- see dup_existing_fd's own doc comment.
                // Only fd 1/2 as the *source* mean anything to a
                // builtin's own output at all (a builtin never writes
                // anywhere else), matching FdOut{1}/FdOut{2} above.
                Redirect::FdDup { fd, target } if *fd == 1 => {
                    stdout_target = Some(Target::Fd(*target as i32));
                    dup_out_to_err = false;
                    touched = true;
                }
                Redirect::FdDup { fd, target } if *fd == 2 => {
                    stderr_target = Some(Target::Fd(*target as i32));
                    dup_err_to_out = false;
                    touched = true;
                }
                _ => {}
            }
        }
        if !touched {
            return Ok(false);
        }
        let resolve = |t: &Option<Target>| -> Result<Option<Rc<RefCell<std::fs::File>>>, String> {
            match t {
                Some(Target::Path(p, append)) => Ok(Some(Rc::new(RefCell::new(self.open_out(p, *append)?)))),
                Some(Target::Fd(fd)) => Ok(dup_existing_fd(*fd).map(|f| Rc::new(RefCell::new(f)))),
                None => Ok(None),
            }
        };
        let stdout = resolve(&stdout_target)?;
        let stderr = resolve(&stderr_target)?;
        let previous = std::mem::replace(&mut self.sink, OutputSink::Real);
        self.sink = OutputSink::Builtin { previous: Box::new(previous), stdout, stderr, dup_err_to_out, dup_out_to_err };
        Ok(true)
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
    fn open_out(&self, path: &str, append: bool) -> Result<std::fs::File, String> {
        if self.opt_restricted {
            return Err(format!("{}: restricted: cannot redirect output", path));
        }
        if let Some(result) = dev_socket_file(path) {
            return result;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(path)
            .map_err(|e| format!("{}: {}", path, e))
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
        std::fs::File::open(path).map_err(|e| format!("{}: {}", path, e))
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
        std::fs::OpenOptions::new().create(true).read(true).write(true).open(path).map_err(|e| format!("{}: {}", path, e))
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
    fn is_restricted_readonly_name(&self, name: &str) -> bool {
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

    fn resolve_plain_fd012(
        &mut self,
        redirects: &[Redirect],
    ) -> Result<(Option<std::fs::File>, Option<std::fs::File>, Option<std::fs::File>), String> {
        // `bool`: also opened for writing (Redirect::InOut, bare `<>`) --
        // see open_in_out's own doc comment.
        let mut stdin_path: Option<(String, bool)> = None;
        let mut here_string: Option<String> = None;
        let mut stdout_target: Option<(String, bool)> = None;
        let mut stderr_target: Option<(String, bool)> = None;
        for r in redirects {
            match r {
                Redirect::In(w) => {
                    stdin_path = Some((self.expand_word(w), false));
                    here_string = None;
                }
                Redirect::InOut(w) => {
                    stdin_path = Some((self.expand_word(w), true));
                    here_string = None;
                }
                Redirect::HereString(w) => {
                    let mut content = self.expand_word(w);
                    content.push('\n');
                    here_string = Some(content);
                    stdin_path = None;
                }
                Redirect::HereDoc(w) => {
                    here_string = Some(self.expand_word(w));
                    stdin_path = None;
                }
                Redirect::Out { word, append } => {
                    stdout_target = Some((self.expand_word(word), *append));
                }
                Redirect::Err { word, append } => {
                    stderr_target = Some((self.expand_word(word), *append));
                }
                Redirect::Both { word, append } => {
                    let p = self.expand_word(word);
                    stdout_target = Some((p.clone(), *append));
                    stderr_target = Some((p, *append));
                }
                _ => {}
            }
        }
        let stdin = if let Some(content) = here_string {
            Some(here_string_file(&content)?)
        } else {
            match stdin_path {
                Some((p, true)) => Some(self.open_in_out(&p)?),
                Some((p, false)) => Some(self.open_in(&p)?),
                None => None,
            }
        };
        let stdout = match stdout_target {
            Some((p, append)) => Some(self.open_out(&p, append)?),
            None => None,
        };
        let stderr = match stderr_target {
            Some((p, append)) => Some(self.open_out(&p, append)?),
            None => None,
        };
        Ok((stdin, stdout, stderr))
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
    // here is one of those kinds. Mirrors resolve_redirect_list's own
    // `Both`/`DupErrToOut` treatment (a dup_err_to_out flag, not a second
    // independent `open()` of the same path -- two separate file opens
    // would track their own, unshared write positions, letting stdout/
    // stderr overwrite each other).
    fn resolve_simple_redirects_for_compound(&mut self, redirects: &[Redirect]) -> Result<ChildStdio, String> {
        let mut stdio = ChildStdio::default();
        let mut stdout_target: Option<(String, bool)> = None;
        let mut stderr_target: Option<(String, bool)> = None;
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
                Redirect::Out { word, append } | Redirect::FdOut { fd: 1, word, append } => {
                    stdout_target = Some((self.expand_word(word), *append));
                    stdio.dup_err_to_out = false;
                }
                Redirect::Err { word, append } | Redirect::FdOut { fd: 2, word, append } => {
                    stderr_target = Some((self.expand_word(word), *append));
                    stdio.dup_err_to_out = false;
                }
                Redirect::Both { word, append } => {
                    stdout_target = Some((self.expand_word(word), *append));
                    stdio.dup_err_to_out = true;
                }
                Redirect::DupErrToOut | Redirect::FdDup { fd: 2, target: 1 } => stdio.dup_err_to_out = true,
                Redirect::FdDup { fd: 1, target: 2 } => stdio.dup_out_to_err = true,
                _ => unreachable!("compound_redirects_are_simple already filtered these out"),
            }
        }
        if let Some((p, append)) = &stdout_target {
            stdio.stdout = Some(self.open_out(p, *append)?);
        }
        if !stdio.dup_err_to_out {
            if let Some((p, append)) = &stderr_target {
                stdio.stderr = Some(self.open_out(p, *append)?);
            }
        }
        Ok(stdio)
    }

    fn resolve_redirect_list(&mut self, redirects: &[Redirect]) -> Result<ResolvedRedirs, String> {
        let mut stdout_target: Option<(String, bool)> = None;
        let mut stderr_target: Option<(String, bool)> = None;
        // `bool`: also opened for writing (Redirect::InOut, bare `<>`) --
        // see open_in_out's own doc comment.
        let mut stdin_path: Option<(String, bool)> = None;
        let mut here_string: Option<String> = None;
        let mut dup_err_to_out = false;
        let mut extra_fds: Vec<ExtraFd> = Vec::new();

        for r in redirects {
            match r {
                Redirect::In(w) => {
                    stdin_path = Some((self.expand_word(w), false));
                    here_string = None;
                }
                Redirect::InOut(w) => {
                    stdin_path = Some((self.expand_word(w), true));
                    here_string = None;
                }
                Redirect::HereString(w) => {
                    let mut content = self.expand_word(w);
                    content.push('\n');
                    here_string = Some(content);
                    stdin_path = None;
                }
                Redirect::HereDoc(w) => {
                    // Body already ends in '\n' from capture_heredoc_body;
                    // reuses the same temp-file Stdio plumbing as <<<.
                    here_string = Some(self.expand_word(w));
                    stdin_path = None;
                }
                Redirect::Out { word, append } => {
                    stdout_target = Some((self.expand_word(word), *append));
                    dup_err_to_out = false;
                }
                Redirect::Err { word, append } => {
                    stderr_target = Some((self.expand_word(word), *append));
                    dup_err_to_out = false;
                }
                Redirect::Both { word, append } => {
                    let p = self.expand_word(word);
                    stdout_target = Some((p, *append));
                    dup_err_to_out = true;
                }
                Redirect::DupErrToOut => dup_err_to_out = true,
                Redirect::FdOut { fd, word, append } => {
                    let p = self.expand_word(word);
                    let file = self.open_out(&p, *append)?;
                    extra_fds.push(ExtraFd::Open { fd: *fd as i32, file });
                }
                Redirect::FdIn { fd, word } => {
                    let p = self.expand_word(word);
                    let file = self.open_in(&p)?;
                    extra_fds.push(ExtraFd::Open { fd: *fd as i32, file });
                }
                Redirect::FdInOut { fd, word } => {
                    let p = self.expand_word(word);
                    let file = self.open_in_out(&p)?;
                    extra_fds.push(ExtraFd::Open { fd: *fd as i32, file });
                }
                Redirect::FdDup { fd, target } => {
                    extra_fds.push(ExtraFd::Dup { fd: *fd as i32, source: *target as i32 });
                }
                Redirect::FdDupWord { fd, word } => {
                    let target_str = self.expand_word(word);
                    match target_str.trim().parse::<i32>() {
                        Ok(source) => extra_fds.push(ExtraFd::Dup { fd: *fd as i32, source }),
                        Err(_) => return Err(format!("{}: ambiguous redirect", target_str)),
                    }
                }
                Redirect::FdClose { fd } => {
                    extra_fds.push(ExtraFd::Close(*fd as i32));
                }
            }
        }

        let stdin = if let Some(content) = here_string {
            Some(Stdio::from(here_string_file(&content)?))
        } else {
            match stdin_path {
                Some((p, true)) => Some(Stdio::from(self.open_in_out(&p)?)),
                Some((p, false)) => Some(Stdio::from(self.open_in(&p)?)),
                None => None,
            }
        };
        let stdout_file: Option<std::fs::File> = match &stdout_target {
            Some((p, append)) => Some(self.open_out(p, *append)?),
            None => None,
        };
        // `2>&1`'s actual fd-dup happens via dup2_stderr_to_stdout at each
        // spawn site instead of resolving a Stdio here -- see
        // ResolvedRedirs::dup_stderr_to_stdout for why (a pipe destination
        // can't be "opened" the way a file can).
        let stderr_file: Option<std::fs::File> = if dup_err_to_out {
            None
        } else {
            match &stderr_target {
                Some((p, append)) => Some(self.open_out(p, *append)?),
                None => None,
            }
        };
        let stdout = stdout_file.map(Stdio::from);
        let stderr = stderr_file.map(Stdio::from);

        Ok(ResolvedRedirs { stdin, stdout, stderr, dup_stderr_to_stdout: dup_err_to_out, extra_fds })
    }
}

impl arith::VarContext for Shell {
    fn get(&mut self, name: &str) -> i64 {
        self.lookup_var(name).trim().parse().unwrap_or(0)
    }

    fn set(&mut self, name: &str, value: i64) {
        self.assign_var(name, value.to_string());
    }
}

struct ResolvedRedirs {
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
    // True fd-dup of whatever stdout ends up being -- a pipe, a file, or
    // inherited -- applied via a dup2 pre_exec hook at each spawn site
    // (see dup2_stderr_to_stdout). Stdio has no "duplicate of a sibling
    // fd" variant, so this is the only way `2>&1` can share stdout's
    // *actual* destination rather than a second, independently-opened
    // handle to it; it's what makes `cmd 2>&1 | other` correctly merge
    // stderr into the pipe, not just the `cmd > file 2>&1` shape.
    dup_stderr_to_stdout: bool,
    // Arbitrary-fd redirects (`3>file`, `4<&0`, ...), applied via the same
    // dup2-in-pre_exec approach as dup_stderr_to_stdout, in source order,
    // *after* dup_stderr_to_stdout -- see apply_fd_redirects. Scoped gap:
    // since these run after stdin/stdout/stderr's own Command-builder setup
    // rather than interleaved with it, a numbered-fd redirect that's meant
    // to capture fd 0/1/2's state *before* a later plain `>`/`<` redirect
    // in the same command changes it (the classic `3>&1 1>log 2>&3`
    // fd-juggling idiom) won't see the pre-redirect value. Every ordering
    // that doesn't depend on that interleaving works correctly.
    extra_fds: Vec<ExtraFd>,
}

enum ExtraFd {
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
struct OrderedMap {
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

    fn remove(&mut self, key: &str) -> Option<String> {
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
struct Job {
    id: u32,
    pids: Vec<u32>,
    children: Vec<std::process::Child>,
    cmd_text: String,
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
    pty_master: Option<std::fs::File>,
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
    pgid: Option<u32>,
    // True once this job has been observed stopped (via a WUNTRACED-
    // aware wait -- see waitpid_untraced) rather than exited. Checked by
    // `jobs`/`wait` before touching Job::poll/wait, neither of which can
    // ever observe a stop (Child::try_wait/wait never pass WUNTRACED, so
    // a stopped child just looks like it's still running to them --
    // correct for every *other* caller, since only run_fg's real-job-
    // control path and run_single's own foreground wait ever use
    // waitpid_untraced directly). Cleared by `fg`/`bg` resuming it.
    stopped: bool,
}

// Backs Shell.jobs (see that field's doc comment for why this lives behind
// Rc<RefCell<_>> instead of being owned directly).
struct JobTable {
    jobs: Vec<Job>,
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
    fn poll(&mut self) -> Option<i32> {
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
    fn wait(&mut self) -> i32 {
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

// Backslash-escape expansion shared by `echo -e` and `printf`'s FORMAT
// (see run_echo/run_printf's own doc comments) -- bash's own set:
// \\ \a \b \e \f \n \r \t \v, \0NNN (up to 3 octal digits), \xHH (up to
// 2 hex digits), and \c (stop all further output, including anything
// already queued after this point in the same echo/printf call --
// signaled via the returned bool, which run_echo checks directly and
// run_printf's format string never actually reaches since FORMAT itself
// doesn't support \c). An unrecognized escape (or a bare trailing
// backslash) is left exactly as written, matching bash rather than
// silently dropping the backslash.
fn echo_expand_escapes(s: &str) -> (String, bool) {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('a') => out.push('\u{7}'),
            Some('b') => out.push('\u{8}'),
            Some('c') => return (out, true),
            Some('e') => out.push('\u{1b}'),
            Some('f') => out.push('\u{c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('v') => out.push('\u{b}'),
            Some('0') => {
                let mut val: u32 = 0;
                let mut n = 0;
                while n < 3 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(d) => {
                            val = val * 8 + d;
                            chars.next();
                            n += 1;
                        }
                        None => break,
                    }
                }
                out.push(char::from_u32(val).unwrap_or('\0'));
            }
            Some('x') => {
                let mut val: u32 = 0;
                let mut n = 0;
                while n < 2 {
                    match chars.peek().and_then(|c| c.to_digit(16)) {
                        Some(d) => {
                            val = val * 16 + d;
                            chars.next();
                            n += 1;
                        }
                        None => break,
                    }
                }
                if n > 0 {
                    out.push(char::from_u32(val).unwrap_or('\0'));
                } else {
                    out.push('\\');
                    out.push('x');
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    (out, false)
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

// Runs FORMAT once against `values[*idx..]`, advancing `*idx` past
// however many of them it actually consumed, and appending the result
// to `out` -- see run_printf's own doc comment for the conversions and
// flags supported. Split out from run_printf so the caller can call
// this repeatedly to cycle FORMAT over more arguments than it has
// conversions for.
fn printf_format_once(format: &str, values: &[String], idx: &mut usize, out: &mut String) {
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('a') => out.push('\u{7}'),
                Some('b') => out.push('\u{8}'),
                Some('e') => out.push('\u{1b}'),
                Some('f') => out.push('\u{c}'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('v') => out.push('\u{b}'),
                Some('"') => out.push('"'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
            continue;
        }
        if c != '%' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            out.push('%');
            continue;
        }

        let mut left_align = false;
        let mut zero_pad = false;
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
                _ => break,
            }
        }
        let mut width_digits = String::new();
        while let Some(&p) = chars.peek() {
            if p.is_ascii_digit() {
                width_digits.push(p);
                chars.next();
            } else {
                break;
            }
        }
        let width: usize = width_digits.parse().unwrap_or(0);
        let mut precision: Option<usize> = None;
        if chars.peek() == Some(&'.') {
            chars.next();
            let mut p = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    p.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            precision = Some(p.parse().unwrap_or(0));
        }
        let Some(conv) = chars.next() else { break };

        let mut next_arg = || -> String {
            let v = values.get(*idx).cloned().unwrap_or_default();
            *idx += 1;
            v
        };
        let numeric = matches!(conv, 'd' | 'i' | 'u' | 'o' | 'x' | 'X');
        let mut piece = match conv {
            's' => {
                let mut s = next_arg();
                if let Some(p) = precision {
                    s.truncate(p);
                }
                s
            }
            'b' => echo_expand_escapes(&next_arg()).0,
            'c' => next_arg().chars().next().map(|c| c.to_string()).unwrap_or_default(),
            'q' => shell_quote(&next_arg()),
            'd' | 'i' => next_arg().trim().parse::<i64>().unwrap_or(0).to_string(),
            'u' => (next_arg().trim().parse::<i64>().unwrap_or(0) as u64).to_string(),
            'o' => format!("{:o}", next_arg().trim().parse::<i64>().unwrap_or(0)),
            'x' => format!("{:x}", next_arg().trim().parse::<i64>().unwrap_or(0)),
            'X' => format!("{:X}", next_arg().trim().parse::<i64>().unwrap_or(0)),
            // Unrecognized conversion: emitted literally, nothing
            // consumed -- matches bash treating it as plain text rather
            // than silently eating an argument.
            other => format!("%{}", other),
        };
        let len = piece.chars().count();
        if len < width {
            let pad = width - len;
            if left_align {
                piece.push_str(&" ".repeat(pad));
            } else if zero_pad && numeric {
                piece = format!("{}{}", "0".repeat(pad), piece);
            } else {
                piece = format!("{}{}", " ".repeat(pad), piece);
            }
        }
        out.push_str(&piece);
    }
}

// bash's exit-status convention for a process killed by a signal is
// 128+signum (ExitStatus::code() returns None in that case -- there's no
// normal exit code to report -- so this falls back to the signal via
// ExitStatusExt, matching what `$?`/`wait`/`fg` should actually show).
fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| status.signal().map(|s| 128 + s).unwrap_or(1))
}

// Support types for run_ulimit (moved from a builtins.rs free function in
// M6 -- see that method's doc comment).
#[repr(C)]
struct RLimit {
    cur: u64,
    max: u64,
}

const RLIM_INFINITY: u64 = u64::MAX;

struct LimitSpec {
    flag: char,
    resource: i32,
    label: &'static str,
    unit: &'static str,
    div: u64,
}

// Standard Linux/glibc RLIMIT_* numbers (stable ABI, safe to hardcode --
// same "libc is already linked, no crate needed" reasoning used elsewhere
// in this codebase for raw syscall numbers/signatures). No RLIMIT_PIPE
// exists on Linux (pipe capacity is a per-pipe fcntl setting, not an
// rlimit) so `-p`, which real bash reports as a fixed constant, is left
// out of `-a` rather than fabricating a value.
const LIMIT_SPECS: &[LimitSpec] = &[
    LimitSpec { flag: 'c', resource: 4, label: "core file size", unit: "blocks", div: 512 },
    LimitSpec { flag: 'd', resource: 2, label: "data seg size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'e', resource: 13, label: "scheduling priority", unit: "", div: 1 },
    LimitSpec { flag: 'f', resource: 1, label: "file size", unit: "blocks", div: 512 },
    LimitSpec { flag: 'i', resource: 11, label: "pending signals", unit: "", div: 1 },
    LimitSpec { flag: 'l', resource: 8, label: "max locked memory", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'm', resource: 5, label: "max memory size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'n', resource: 7, label: "open files", unit: "", div: 1 },
    LimitSpec { flag: 'q', resource: 12, label: "POSIX message queues", unit: "bytes", div: 1 },
    LimitSpec { flag: 'r', resource: 14, label: "real-time priority", unit: "", div: 1 },
    LimitSpec { flag: 's', resource: 3, label: "stack size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 't', resource: 0, label: "cpu time", unit: "seconds", div: 1 },
    LimitSpec { flag: 'u', resource: 6, label: "max user processes", unit: "", div: 1 },
    LimitSpec { flag: 'v', resource: 9, label: "virtual memory", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'x', resource: 10, label: "file locks", unit: "", div: 1 },
];

fn fmt_limit(v: u64, div: u64) -> String {
    if v == RLIM_INFINITY {
        "unlimited".to_string()
    } else {
        (v / div.max(1)).to_string()
    }
}

fn umask_symbolic(mask: u32) -> String {
    let perm_for = |shift: u32| -> String {
        let bits = (mask >> shift) & 0o7;
        let mut s = String::new();
        if bits & 0o4 == 0 {
            s.push('r');
        }
        if bits & 0o2 == 0 {
            s.push('w');
        }
        if bits & 0o1 == 0 {
            s.push('x');
        }
        s
    };
    format!("u={},g={},o={}", perm_for(6), perm_for(3), perm_for(0))
}

// Shared by the `read` builtin's two sources (a Box<dyn BufRead> from
// read_input_source, or a borrowed coproc fd via `-u`) -- factored out
// specifically so the `-u` path's borrow of self.coproc_fds can end right
// after this call, before the caller needs `&mut self` again for
// assign_var. Real bash: a line/char-count read that runs into EOF before
// seeing its delimiter (or before nchars is reached) still populates the
// variable(s) with whatever partial data it got, but returns non-zero --
// `clean` (the bool) tracks which case this was.
fn read_line_or_chars(reader: &mut dyn std::io::BufRead, nchars: Option<usize>, delim: u8) -> (Option<String>, bool) {
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
        match reader.read_until(delim, &mut buf) {
            Ok(0) => (None, false),
            Ok(_) => {
                let hit_delim = buf.last() == Some(&delim);
                if hit_delim {
                    buf.pop();
                    if delim == b'\n' && buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                }
                (Some(String::from_utf8_lossy(&buf).into_owned()), hit_delim)
            }
            Err(_) => (None, false),
        }
    }
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
fn current_umask() -> u32 {
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

// Accepts "INT", "SIGINT", or a bare number ("2"); "0"/"EXIT" is handled by
// the caller separately since it isn't a real signal.
fn signal_number(name: &str) -> Option<i32> {
    let bare = name.strip_prefix("SIG").unwrap_or(name);
    if let Some(&(_, n)) = SIGNAL_NAMES.iter().find(|(n, _)| *n == bare) {
        return Some(n);
    }
    bare.parse::<i32>().ok()
}

fn signal_name(num: i32) -> String {
    SIGNAL_NAMES.iter().find(|(_, n)| *n == num).map(|(name, _)| name.to_string()).unwrap_or_else(|| num.to_string())
}

fn send_signal(pid: u32, sig: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid as i32, sig) == 0 }
}

// kill(2) with a negative pid targets the whole process group (POSIX) --
// used to SIGCONT/SIGTERM/etc. a real-job-control job (Job::pgid) as a
// unit, matching how the terminal driver itself would signal it.
fn send_signal_to_pgrp(pgid: u32, sig: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(-(pgid as i32), sig) == 0 }
}

// Real job control (M11): SIGCONT, used to resume a Stopped job (Job::
// stopped) from `fg`/`bg`. Linux/glibc's standard number, same "safe to
// hardcode" reasoning as this file's other signal constants.
const SIGCONT: i32 = 18;
// SIGSTOP: used by FgJob::send_stop instead of SIGTSTP -- see its own
// doc comment for why the catchable version doesn't reliably work here.
const SIGSTOP: i32 = 19;

unsafe extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn getpgrp() -> i32;
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
enum JobWaitOutcome {
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
fn waitpid_untraced(pid: u32) -> JobWaitOutcome {
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

// The glibc/BSD `struct tm` layout (POSIX's 9 base fields plus the
// common tm_gmtoff/tm_zone extension both platforms agree on) --
// localtime_r writes a full struct tm's worth of bytes into its output
// pointer regardless of what this declares, so this has to match the
// real platform layout size-for-size, not just the fields this code
// actually reads.
#[repr(C)]
struct CTm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const i8,
}

// `${v@P}`'s `\d`/`\t`/`\T`/`\@`/`\A`/`\D{...}` all need the current
// local wall-clock time -- computed via the same raw libc FFI pattern
// already used elsewhere in this file (e.g. stdin_is_tty/stdin_ready),
// rather than pulling in a date/time crate for it.
fn local_time_now() -> CTm {
    unsafe extern "C" {
        fn time(t: *mut i64) -> i64;
        fn localtime_r(t: *const i64, result: *mut CTm) -> *mut CTm;
    }
    let mut t: i64 = 0;
    unsafe { time(&mut t as *mut i64) };
    let mut tm = CTm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    unsafe { localtime_r(&t as *const i64, &mut tm as *mut CTm) };
    tm
}

const WEEKDAY_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WEEKDAY_FULL: [&str; 7] =
    ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTH_ABBR: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const MONTH_FULL: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November",
    "December",
];

// `\d`: bash's own default (no-arg) date format, "Weekday Month Day"
// with the day space-padded to two columns (matching `%e`, e.g. "Tue
// May  6" for the 6th) -- no year, no locale support (bish has none at
// all), always English abbreviations.
fn prompt_date() -> String {
    let tm = local_time_now();
    format!("{} {} {:2}", WEEKDAY_ABBR[tm.tm_wday as usize % 7], MONTH_ABBR[tm.tm_mon as usize % 12], tm.tm_mday)
}

// A small strftime subset covering the specifiers a prompt format
// string would plausibly use -- not a general-purpose implementation
// (no locale support, no width/padding modifiers beyond what's baked
// into each specifier below). An unrecognized `%X` passes through
// literally, matching this codebase's own established convention for
// an unrecognized escape sequence elsewhere (e.g. expand_backslash_escapes).
fn strftime(fmt: &str, tm: &CTm) -> String {
    let year = tm.tm_year + 1900;
    let hour24 = tm.tm_hour;
    let hour12 = match hour24 % 12 {
        0 => 12,
        h => h,
    };
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(spec) = chars.next() else {
            out.push('%');
            break;
        };
        match spec {
            'Y' => out.push_str(&year.to_string()),
            'y' => out.push_str(&format!("{:02}", year.rem_euclid(100))),
            'm' => out.push_str(&format!("{:02}", tm.tm_mon + 1)),
            'd' => out.push_str(&format!("{:02}", tm.tm_mday)),
            'e' => out.push_str(&format!("{:2}", tm.tm_mday)),
            'H' => out.push_str(&format!("{:02}", hour24)),
            'I' => out.push_str(&format!("{:02}", hour12)),
            'M' => out.push_str(&format!("{:02}", tm.tm_min)),
            'S' => out.push_str(&format!("{:02}", tm.tm_sec)),
            'p' => out.push_str(if hour24 < 12 { "AM" } else { "PM" }),
            'a' => out.push_str(WEEKDAY_ABBR[tm.tm_wday as usize % 7]),
            'A' => out.push_str(WEEKDAY_FULL[tm.tm_wday as usize % 7]),
            'b' => out.push_str(MONTH_ABBR[tm.tm_mon as usize % 12]),
            'B' => out.push_str(MONTH_FULL[tm.tm_mon as usize % 12]),
            'j' => out.push_str(&format!("{:03}", tm.tm_yday + 1)),
            'T' => out.push_str(&format!("{:02}:{:02}:{:02}", hour24, tm.tm_min, tm.tm_sec)),
            'F' => out.push_str(&format!("{:04}-{:02}-{:02}", year, tm.tm_mon + 1, tm.tm_mday)),
            '%' => out.push('%'),
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
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
fn apply_fds_to_self(dup_stderr_to_stdout: bool, extra_fds: Vec<ExtraFd>) -> Result<(), String> {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    if dup_stderr_to_stdout {
        if unsafe { dup2(1, 2) } == -1 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        clear_cloexec(2);
    }
    for ef in extra_fds {
        match ef {
            ExtraFd::Open { fd, file } => {
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
            ExtraFd::Dup { fd, source } => {
                if unsafe { dup2(source, fd) } == -1 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                clear_cloexec(fd);
            }
            ExtraFd::Close(fd) => {
                if unsafe { close(fd) } == -1 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
            }
        }
    }
    Ok(())
}

// Persists resolve_plain_fd012's raw Files onto this process's own fd
// 0/1/2, for `exec`'s redirect-only form. Same dup2 + clear_cloexec +
// forget-on-self-dup pattern as apply_fds_to_self, and for the same
// reasons (no execve immediately follows to bypass Rust's normal Drop
// glue the way the pre_exec-based spawn path is protected).
fn apply_fd012_to_self(
    stdin: Option<std::fs::File>,
    stdout: Option<std::fs::File>,
    stderr: Option<std::fs::File>,
) -> Result<(), String> {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }
    for (target, file) in [(0, stdin), (1, stdout), (2, stderr)] {
        let Some(file) = file else { continue };
        let srcfd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
        if unsafe { dup2(srcfd, target) } == -1 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        clear_cloexec(target);
        if srcfd == target {
            std::mem::forget(file);
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

fn apply_fd_redirects(command: &mut Command, dup_stderr_to_stdout: bool, extra_fds: Vec<ExtraFd>) {
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
            if dup_stderr_to_stdout && dup2(1, 2) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            for ef in &extra_fds {
                match ef {
                    ExtraFd::Open { fd, file } => {
                        if dup2(std::os::unix::io::AsRawFd::as_raw_fd(file), *fd) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        clear_cloexec(*fd);
                    }
                    ExtraFd::Dup { fd, source } => {
                        if dup2(*source, *fd) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        clear_cloexec(*fd);
                    }
                    ExtraFd::Close(fd) => {
                        if close(*fd) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
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
fn dev_socket_file(path: &str) -> Option<Result<std::fs::File, String>> {
    let (proto, rest) = path
        .strip_prefix("/dev/tcp/")
        .map(|r| ("tcp", r))
        .or_else(|| path.strip_prefix("/dev/udp/").map(|r| ("udp", r)))?;
    let (host, port) = rest.split_once('/')?;
    if host.is_empty() || port.is_empty() {
        return None;
    }
    Some(connect_dev_socket(proto, host, port).map_err(|e| format!("{}: {}", path, e)))
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
fn ifs_next_field(s: &str, ifs: &str) -> Option<(String, String)> {
    let is_ws = |c: char| c.is_whitespace() && ifs.contains(c);
    let is_sep = |c: char| ifs.contains(c);
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n && !is_sep(chars[i]) {
        i += 1;
    }
    if i >= n {
        return None;
    }
    let field: String = chars[..i].iter().collect();
    while i < n && is_ws(chars[i]) {
        i += 1;
    }
    if i < n && is_sep(chars[i]) {
        i += 1;
        while i < n && is_ws(chars[i]) {
            i += 1;
        }
    }
    Some((field, chars[i..].iter().collect()))
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

// Kept in sync with run_single's builtin dispatch match by hand -- used
// only by `command -v`/`type` to classify a name, not to actually run
// anything, so a name missing from this list just means those two
// diagnostic builtins under-report it as an external/not-found rather
// than anything actually breaking.
const KNOWN_BUILTINS: &[&str] = &[
    ":",
    // Every name `dispatch_builtin_or_external_impl` handles has to be
    // here, not just for completion: `run_multi` uses this list to
    // decide whether a pipeline stage needs the self-exec that lets a
    // builtin run as one. A builtin missing from it is spawned as an
    // external program and fails with ENOENT -- which is exactly what
    // `echo hi | json .` did until `json` was added. See
    // `every_dispatched_builtin_is_known`.
    "json",
    "cd",
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
const KNOWN_SHOPT_OPTIONS: &[(&str, bool)] = &[
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

fn shopt_default_on(name: &str) -> Option<bool> {
    KNOWN_SHOPT_OPTIONS.iter().find(|(n, _)| *n == name).map(|(_, on)| *on)
}

// `compgen -A setopt`/`complete -A setopt`: valid `set -o NAME` names.
// Kept as its own list (rather than bash's full ~29-entry set) in sync
// with apply_shell_option's own match arms above -- only names that
// actually gate real bish behavior, same "don't advertise a name that
// does nothing" principle KNOWN_BISHOPTS follows for its own registry.
const SET_O_OPTIONS: &[&str] = &["pipefail", "errexit", "nounset", "xtrace", "noglob", "monitor", "posix"];

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
enum BishOptDefault {
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
enum BishOptValue {
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
const RESTRICTED: &str = "restricted";

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
        _ => &[],
    }
}

// The flags `::bish lsp add` takes, in the spelling its own usage line
// uses. `=`-terminated because each takes a value, and the completion
// menu offering `--lang=` rather than `--lang` puts the cursor where
// the next thing to type goes.
pub fn lsp_add_flags() -> &'static [&'static str] {
    &["--lang=", "--root=", "--root-cmd=", "--apply-edits="]
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

fn resolve_in_path(name: &str) -> Option<String> {
    if name.contains('/') {
        return if std::path::Path::new(name).is_file() { Some(name.to_string()) } else { None };
    }
    let path_var = std::env::var("PATH").unwrap_or_default();
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


fn command_own_redirects(cmd: &parser::Command) -> &[Redirect] {
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

fn kill_all(children: Vec<std::process::Child>) {
    for mut c in children {
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
fn write_diagnostic(target: &Option<String>, msg: &str, sink: OutputSink) {
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
#[cfg(test)]
mod tests {
    // `change_directory` moves the real process, so these must not run
    // beside anything else that reads or sets the cwd -- same shared-
    // mutex fix, and the same poisoned-lock recovery, as session.rs's
    // own ENV_LOCK.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // The bug this pins was found by driving the file browser through a
    // real pty: Ctrl-Y moved the shell correctly, and then `cd -` went
    // somewhere else entirely. `sync_real_state_in` reapplies the
    // session's env snapshot before every command, so an `OLDPWD`
    // written only to the real environment -- which is all a change made
    // *outside* a command can do -- lasted exactly until the next one.
    #[test]
    fn changing_directory_updates_the_session_snapshot_not_just_the_environment() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let original = std::env::current_dir().expect("a working directory");

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
        assert_eq!(std::env::var("PWD").as_deref(), Ok(inner_real.to_string_lossy().as_ref()));
        assert_eq!(std::env::var("OLDPWD").as_deref(), Ok(root_real.to_string_lossy().as_ref()));

        // The part that was broken: after the session restores its own
        // remembered environment, `cd -` still has somewhere to go.
        shell.sync_real_state_in();
        assert_eq!(
            std::env::var("OLDPWD").as_deref(),
            Ok(root_real.to_string_lossy().as_ref()),
            "OLDPWD must survive the snapshot being reapplied"
        );
        assert_eq!(std::env::var("PWD").as_deref(), Ok(inner_real.to_string_lossy().as_ref()));

        std::env::set_current_dir(&original).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    // Restricted mode refuses at the write path, so nothing that can
    // move the shell -- including the file browser -- can route around
    // it.
    #[test]
    fn a_restricted_shell_refuses_to_change_directory_at_all() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let original = std::env::current_dir().expect("a working directory");
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let err = shell.change_directory(std::path::Path::new("/")).unwrap_err();
        assert_eq!(err, RESTRICTED);
        assert_eq!(std::env::current_dir().unwrap(), original, "the real process must not have moved");
    }

    use super::*;

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
        assert_eq!(shell.run_abbr(&strs(&["-a", "gco", "git", "checkout"])), 0);
        assert_eq!(shell.abbrs, vec![Abbr::new("gco", "git checkout")]);
    }

    #[test]
    fn abbr_add_without_the_dash_a_flag_still_adds() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_abbr(&strs(&["ll", "ls", "-la"])), 0);
        assert_eq!(shell.abbrs, vec![Abbr::new("ll", "ls -la")]);
    }

    #[test]
    fn abbr_add_redefines_an_existing_name_in_place_rather_than_duplicating() {
        let mut shell = Shell::new();
        shell.run_abbr(&strs(&["-a", "gco", "git", "checkout"]));
        shell.run_abbr(&strs(&["-a", "gco", "git", "switch"]));
        assert_eq!(shell.abbrs, vec![Abbr::new("gco", "git switch")]);
    }

    #[test]
    fn abbr_erase_removes_a_known_name_and_reports_status_1_for_an_unknown_one() {
        let mut shell = Shell::new();
        shell.run_abbr(&strs(&["-a", "gco", "git", "checkout"]));
        assert_eq!(shell.run_abbr(&strs(&["-e", "gco"])), 0);
        assert!(shell.abbrs.is_empty());
        assert_eq!(shell.run_abbr(&strs(&["-e", "gco"])), 1);
    }

    #[test]
    fn abbr_query_is_zero_only_when_every_named_abbreviation_exists() {
        let mut shell = Shell::new();
        shell.run_abbr(&strs(&["-a", "gco", "git", "checkout"]));
        shell.run_abbr(&strs(&["-a", "gs", "git", "status"]));
        assert_eq!(shell.run_abbr(&strs(&["-q", "gco", "gs"])), 0);
        assert_eq!(shell.run_abbr(&strs(&["-q", "gco", "nope"])), 1);
    }

    #[test]
    fn abbr_list_and_show_report_status_0_without_mutating_the_table() {
        let mut shell = Shell::new();
        shell.run_abbr(&strs(&["-a", "gco", "git", "checkout"]));
        assert_eq!(shell.run_abbr(&strs(&["-l"])), 0);
        assert_eq!(shell.run_abbr(&strs(&["-s"])), 0);
        assert_eq!(shell.run_abbr(&[]), 0);
        assert_eq!(shell.abbrs, vec![Abbr::new("gco", "git checkout")]);
    }

    // The trailing-integer-run spelling for placeholder order is gone
    // -- order lives in the expansion now, as `$1`/`$2` -- so trailing
    // integers are plain expansion words again, with nothing to
    // disambiguate.
    #[test]
    fn abbr_add_keeps_trailing_integers_as_expansion_words() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_abbr(&strs(&["--add", "foo", "bar -x $1 -y $2", "2", "1"])), 0);
        assert_eq!(shell.abbrs, vec![Abbr::new("foo", "bar -x $1 -y $2 2 1")]);
        assert_eq!(shell.run_abbr(&strs(&["-a", "e12", "echo", "1", "2"])), 0);
        assert_eq!(shell.abbrs[1], Abbr::new("e12", "echo 1 2"));
    }

    #[test]
    fn abbr_show_round_trips_an_expansion_verbatim() {
        let mut shell = Shell::new();
        shell.run_abbr(&strs(&["-a", "foo", "bar -x ${1:x} -y $2"]));
        let out = capture_output(&mut shell);
        shell.run_abbr(&strs(&["-s"]));
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
            stdout: Some(file),
            stderr: None,
            dup_err_to_out: false,
            dup_out_to_err: false,
        };
        assert!(shell.sink_grid().is_some(), "a per-command redirect sink must not hide the session's own grid");
    }

    #[test]
    fn abbr_lang_scopes_an_abbreviation_and_the_same_name_can_mean_two_things() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_abbr(&strs(&["-a", "p", "echo bash"])), 0);
        assert_eq!(shell.run_abbr(&strs(&["--lang=rust", "-a", "p", "println!(\"%s\")"])), 0);
        assert_eq!(shell.abbrs.len(), 2, "same name, different language -- two entries, not a redefinition");
        // Redefining still replaces in place, keyed on both.
        assert_eq!(shell.run_abbr(&strs(&["--lang=rust", "-a", "p", "dbg!(%s)"])), 0);
        assert_eq!(shell.abbrs.len(), 2);
        assert_eq!(shell.abbrs[1].expansion, "dbg!(%s)");
        assert_eq!(shell.abbrs[0].lang, "bash");
    }

    #[test]
    fn abbr_erase_without_a_lang_erases_the_name_everywhere() {
        let mut shell = Shell::new();
        shell.run_abbr(&strs(&["-a", "p", "one"]));
        shell.run_abbr(&strs(&["--lang=rust", "-a", "p", "two"]));
        shell.run_abbr(&strs(&["-a", "q", "three"]));
        assert_eq!(shell.run_abbr(&strs(&["-e", "p"])), 0);
        assert_eq!(shell.abbrs.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["q"]);
    }

    #[test]
    fn abbr_erase_with_a_lang_erases_only_that_one() {
        let mut shell = Shell::new();
        shell.run_abbr(&strs(&["-a", "p", "one"]));
        shell.run_abbr(&strs(&["--lang=rust", "-a", "p", "two"]));
        assert_eq!(shell.run_abbr(&strs(&["--lang=rust", "-e", "p"])), 0);
        assert_eq!(shell.abbrs.len(), 1);
        assert_eq!(shell.abbrs[0].lang, "bash");
        // ...and erasing a language it isn't defined for is a miss.
        assert_eq!(shell.run_abbr(&strs(&["--lang=go", "-e", "p"])), 1);
    }

    #[test]
    fn abbr_query_can_ask_about_one_language() {
        let mut shell = Shell::new();
        shell.run_abbr(&strs(&["--lang=rust", "-a", "p", "two"]));
        assert_eq!(shell.run_abbr(&strs(&["-q", "p"])), 0, "any language counts without --lang");
        assert_eq!(shell.run_abbr(&strs(&["--lang=rust", "-q", "p"])), 0);
        assert_eq!(shell.run_abbr(&strs(&["--lang=bash", "-q", "p"])), 1);
    }

    #[test]
    fn abbr_show_round_trips_a_language() {
        let mut shell = Shell::new();
        shell.run_abbr(&strs(&["--lang=!(bash)", "-a", "p", "note %s"]));
        shell.run_abbr(&strs(&["-a", "plain", "echo hi"]));
        let out = capture_output(&mut shell);
        shell.run_abbr(&strs(&["-s"]));
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
        assert_eq!(shell.run_abbr(&strs(&["-a", "gco"])), 2);
        assert!(shell.abbrs.is_empty());
    }

    #[test]
    fn new_virtual_child_inherits_a_snapshot_of_the_parents_abbrs() {
        let mut parent = Shell::new();
        parent.run_abbr(&strs(&["-a", "gco", "git", "checkout"]));
        let mut child = parent.new_virtual_child();
        assert_eq!(child.abbrs, parent.abbrs);
        child.run_abbr(&strs(&["-a", "gs", "git", "status"]));
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
        assert_eq!(shell.run_shopt(&strs(&["-s", "nullglob"])), 0);
        assert!(shell.shopt_is_on("nullglob"));

        assert!(shell.shopt_is_on("cmdhist"));
        assert_eq!(shell.run_shopt(&strs(&["-u", "cmdhist"])), 0);
        assert!(!shell.shopt_is_on("cmdhist"));
    }

    #[test]
    fn shopt_q_reports_status_from_every_names_effective_state() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_shopt(&strs(&["-q", "cmdhist", "extglob"])), 0);
        assert_eq!(shell.run_shopt(&strs(&["-q", "cmdhist", "nullglob"])), 1);
    }

    #[test]
    fn shopt_rejects_an_unknown_option_name() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_shopt(&strs(&["bogus_option"])), 1);
        assert_eq!(shell.run_shopt(&strs(&["-s", "bogus_option"])), 1);
        assert!(shell.shopt_options.is_empty(), "a rejected name must not be recorded");
    }

    #[test]
    fn bare_shopt_and_s_and_u_alone_enumerate_every_known_option() {
        let mut shell = Shell::new();
        // Bare `shopt` (no -s/-u, no names): every known option is a
        // valid target, so this must not error and must not mutate
        // anything -- this is the bug this whole patch exists to fix
        // ("shopt without arguments does nothing").
        assert_eq!(shell.run_shopt(&[]), 0);
        assert!(shell.shopt_options.is_empty());

        // `shopt -s`/`shopt -u` alone list, not toggle -- no names means
        // nothing to turn on/off, unlike `shopt -s NAME`.
        assert_eq!(shell.run_shopt(&strs(&["-s"])), 0);
        assert_eq!(shell.run_shopt(&strs(&["-u"])), 0);
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
        assert_eq!(shell.run_bishopt(&[], &test_bishopts()), 0);
    }

    #[test]
    fn bishopt_get_on_a_bool_reports_its_value_via_exit_status_either_way() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bishopt(&strs(&["verbose"]), &test_bishopts()), 1, "unset bool defaults to off");
        shell.run_bishopt(&strs(&["--set", "verbose"]), &test_bishopts());
        assert_eq!(shell.run_bishopt(&strs(&["verbose"]), &test_bishopts()), 0);
    }

    #[test]
    fn bishopt_quiet_flag_behaves_like_the_bare_get_but_without_printing() {
        let mut shell = Shell::new();
        // -q/--quiet only changes whether get *prints* -- the exit status
        // (what shopt -q itself is for) is identical to the bare get's.
        assert_eq!(shell.run_bishopt(&strs(&["-q", "verbose"]), &test_bishopts()), 1);
        assert_eq!(shell.run_bishopt(&strs(&["--quiet", "greeting"]), &test_bishopts()), 0, "a Str's mere existence is enough under -q");
        shell.run_bishopt(&strs(&["--set", "verbose"]), &test_bishopts());
        assert_eq!(shell.run_bishopt(&strs(&["-q", "verbose"]), &test_bishopts()), 0);
    }

    #[test]
    fn bishopt_set_accepts_on_and_off_as_an_alternative_to_unset_for_a_bool() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bishopt(&strs(&["--set", "verbose", "on"]), &test_bishopts()), 0);
        assert_eq!(shell.bishopt_value(&test_bishopts(), "verbose"), Some(BishOptValue::Bool(true)));
        assert_eq!(shell.run_bishopt(&strs(&["--set", "verbose", "off"]), &test_bishopts()), 0);
        assert_eq!(shell.bishopt_value(&test_bishopts(), "verbose"), Some(BishOptValue::Bool(false)));
    }

    #[test]
    fn bishopt_get_on_a_str_prints_its_value_and_exits_0() {
        let mut shell = Shell::new();
        assert_eq!(shell.bishopt_value(&test_bishopts(), "greeting"), Some(BishOptValue::Str("hi".to_string())));
        assert_eq!(shell.run_bishopt(&strs(&["greeting"]), &test_bishopts()), 0);
        shell.run_bishopt(&strs(&["--set", "greeting", "hey"]), &test_bishopts());
        assert_eq!(shell.bishopt_value(&test_bishopts(), "greeting"), Some(BishOptValue::Str("hey".to_string())));
    }

    #[test]
    fn bishopt_set_rejects_a_value_on_a_bool_and_a_missing_value_on_a_str() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bishopt(&strs(&["--set", "verbose", "true"]), &test_bishopts()), 2);
        assert_eq!(shell.run_bishopt(&strs(&["--set", "greeting"]), &test_bishopts()), 2);
        assert_eq!(shell.bishopts, std::collections::HashMap::new(), "a rejected set must not be recorded");
    }

    #[test]
    fn bishopt_unset_reverts_to_each_types_own_default() {
        let mut shell = Shell::new();
        shell.run_bishopt(&strs(&["--set", "verbose"]), &test_bishopts());
        shell.run_bishopt(&strs(&["--set", "greeting", "hey"]), &test_bishopts());
        shell.run_bishopt(&strs(&["--set", "accent", "blue"]), &test_bishopts());
        assert_eq!(shell.run_bishopt(&strs(&["--unset", "verbose"]), &test_bishopts()), 0);
        assert_eq!(shell.run_bishopt(&strs(&["--unset", "greeting"]), &test_bishopts()), 0);
        assert_eq!(shell.run_bishopt(&strs(&["--unset", "accent"]), &test_bishopts()), 0);
        assert_eq!(shell.bishopt_value(&test_bishopts(), "verbose"), Some(BishOptValue::Bool(false)));
        assert_eq!(shell.bishopt_value(&test_bishopts(), "greeting"), Some(BishOptValue::Str("hi".to_string())));
        assert_eq!(shell.bishopt_value(&test_bishopts(), "accent"), Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))])));
    }

    #[test]
    fn bishopt_get_on_a_color_prints_the_original_text_not_a_re_serialization() {
        let mut shell = Shell::new();
        assert_eq!(shell.bishopt_value(&test_bishopts(), "accent"), Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))])));

        let buf = Rc::new(RefCell::new(String::new()));
        shell.set_sink_capture(buf.clone());
        assert_eq!(shell.run_bishopt(&strs(&["accent"]), &test_bishopts()), 0);
        assert_eq!(buf.borrow().as_str(), "red\n", "must echo back the registered default's own text, not \"#ff0000\"");

        buf.borrow_mut().clear();
        shell.run_bishopt(&strs(&["--set", "accent", "cornflowerblue"]), &test_bishopts());
        assert_eq!(shell.run_bishopt(&strs(&["accent"]), &test_bishopts()), 0);
        assert_eq!(buf.borrow().as_str(), "cornflowerblue\n", "must echo back what --set was actually given, not \"#6495ed\"");

        assert_eq!(shell.run_bishopt(&strs(&["-q", "accent"]), &test_bishopts()), 0, "no boolean meaning, but must not error");
    }

    #[test]
    fn bishopt_set_accepts_any_valid_css_color_syntax_including_color_mix() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bishopt(&strs(&["--set", "accent", "#00ff00"]), &test_bishopts()), 0);
        assert_eq!(shell.bishopt_value(&test_bishopts(), "accent"), Some(BishOptValue::Color("#00ff00".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(0, 255, 0, 255))])));

        assert_eq!(shell.run_bishopt(&strs(&["--set", "accent", "rgb(0 0 255)"]), &test_bishopts()), 0);
        assert_eq!(shell.bishopt_value(&test_bishopts(), "accent"), Some(BishOptValue::Color("rgb(0 0 255)".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(0, 0, 255, 255))])));

        assert_eq!(shell.run_bishopt(&strs(&["--set", "accent", "color-mix(in srgb, red, blue)"]), &test_bishopts()), 0);
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("color-mix(in srgb, red, blue)".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(128, 0, 128, 255))]))
        );
    }

    #[test]
    fn bishopt_set_rejects_an_invalid_color_and_does_not_mutate() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bishopt(&strs(&["--set", "accent", "not-a-color"]), &test_bishopts()), 2);
        assert_eq!(
            shell.bishopt_value(&test_bishopts(), "accent"),
            Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))])),
            "a rejected set must not overwrite the default"
        );
    }

    #[test]
    fn bishopt_rejects_an_unregistered_name_everywhere() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bishopt(&strs(&["nope"]), &test_bishopts()), 1);
        assert_eq!(shell.run_bishopt(&strs(&["--set", "nope"]), &test_bishopts()), 1);
        assert_eq!(shell.run_bishopt(&strs(&["--unset", "nope"]), &test_bishopts()), 1);
    }

    #[test]
    fn new_virtual_child_inherits_a_snapshot_of_the_parents_bishopts() {
        let mut parent = Shell::new();
        parent.run_bishopt(&strs(&["--set", "verbose"]), &test_bishopts());
        let mut child = parent.new_virtual_child();
        assert_eq!(child.bishopts, parent.bishopts);
        child.run_bishopt(&strs(&["--set", "greeting", "yo"]), &test_bishopts());
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
        assert_eq!(shell.run_bish(&strs(&["theme", "begin"])).status(), 0);
        assert_eq!(shell.run_bishopt(&strs(&["--set", "theme", "dark"]), KNOWN_BISHOPTS), 0);
        assert_eq!(shell.run_bishopt(&strs(&["--set", "accent", "blue"]), &test_bishopts()), 0);
        assert_eq!(shell.run_bish(&strs(&["theme", "end"])).status(), 0);

        // Neither "theme" nor "accent" was actually applied live -- both
        // still read as whatever they were before the declaration.
        assert_eq!(shell.bishopt_value(KNOWN_BISHOPTS, "theme"), Some(BishOptValue::Str(String::new())));
        assert_eq!(shell.bishopt_value(&test_bishopts(), "accent"), Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))])));
        // But the theme itself was registered, "theme" entry excluded.
        let dark = shell.themes.get("dark").expect("theme must be registered");
        assert_eq!(dark.opts.get("accent"), Some(&BishOptValue::Color("blue".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(0, 0, 255, 255))])));
        assert!(!dark.opts.contains_key("theme"), "a theme's own opts must not include a self-referential \"theme\" entry");
    }

    #[test]
    fn bish_theme_end_without_ever_naming_it_discards_the_whole_declaration() {
        let mut shell = Shell::new();
        shell.run_bish(&strs(&["theme", "begin"])).status();
        shell.run_bishopt(&strs(&["--set", "accent", "blue"]), &test_bishopts());
        assert_eq!(shell.run_bish(&strs(&["theme", "end"])).status(), 0);
        assert!(shell.themes.is_empty(), "no name was ever declared, so nothing should be registered");
        assert_eq!(shell.bishopt_value(&test_bishopts(), "accent"), Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))])), "still not applied live either");
    }

    #[test]
    fn activating_a_declared_theme_makes_its_opts_the_new_fallback_default() {
        let mut shell = Shell::new();
        shell.run_bish(&strs(&["theme", "begin"])).status();
        shell.run_bishopt(&strs(&["--set", "theme", "dark"]), KNOWN_BISHOPTS);
        shell.run_bishopt(&strs(&["--set", "accent", "blue"]), &test_bishopts());
        shell.run_bish(&strs(&["theme", "end"])).status();

        // Registering "dark" doesn't activate it by itself.
        assert_eq!(shell.bishopt_value(&test_bishopts(), "accent"), Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))])));

        // Activating it (an ordinary set, outside any declaration) makes
        // its opts the new fallback wherever nothing else was set.
        assert_eq!(shell.run_bishopt(&strs(&["--set", "theme", "dark"]), KNOWN_BISHOPTS), 0);
        assert_eq!(shell.bishopt_value(&test_bishopts(), "accent"), Some(BishOptValue::Color("blue".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(0, 0, 255, 255))])));

        // An explicit override still wins over the active theme.
        shell.run_bishopt(&strs(&["--set", "accent", "green"]), &test_bishopts());
        assert_eq!(shell.bishopt_value(&test_bishopts(), "accent"), Some(BishOptValue::Color("green".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(0, 128, 0, 255))])));
    }

    #[test]
    fn bish_theme_begin_refuses_to_nest() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bish(&strs(&["theme", "begin"])).status(), 0);
        assert_eq!(shell.run_bish(&strs(&["theme", "begin"])).status(), 1, "a second begin while one is already in progress must be refused");
        // The original declaration must still be intact -- a set right
        // after the refused nested begin still lands in it.
        shell.run_bishopt(&strs(&["--set", "theme", "t"]), KNOWN_BISHOPTS);
        shell.run_bish(&strs(&["theme", "end"])).status();
        assert!(shell.themes.contains_key("t"));
    }

    #[test]
    fn bish_theme_end_without_a_begin_is_an_error() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bish(&strs(&["theme", "end"])).status(), 1);
    }

    #[test]
    fn bish_unset_still_applies_live_even_mid_declaration() {
        let mut shell = Shell::new();
        shell.run_bishopt(&strs(&["--set", "accent", "blue"]), &test_bishopts());
        shell.run_bish(&strs(&["theme", "begin"])).status();
        assert_eq!(shell.run_bishopt(&strs(&["--unset", "accent"]), &test_bishopts()), 0);
        shell.run_bish(&strs(&["theme", "end"])).status();
        assert_eq!(shell.bishopt_value(&test_bishopts(), "accent"), Some(BishOptValue::Color("red".to_string(), vec![crate::csscolor::TermColor::Rgba(crate::csscolor::Rgba::new(255, 0, 0, 255))])), "--unset must not have been diverted into the pending theme");
    }

    #[test]
    fn bish_and_bish_theme_reject_unknown_subcommands() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bish(&strs(&["nonsense"])).status(), 2);
        assert_eq!(shell.run_bish(&strs(&[])).status(), 2);
        assert_eq!(shell.run_bish(&strs(&["theme", "nonsense"])).status(), 2);
        assert_eq!(shell.run_bish(&strs(&["theme"])).status(), 2);
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
            assert_eq!(shell.run_hl(&strs(&["--set", name, "#123456"])), 0, "{name} must be settable");
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
        assert_eq!(shell.run_hl(&strs(&["--set", "string", "#123456"])), 0);
        assert_eq!(shell.hl_color("string"), Some(vt100::Color::Rgb(0x12, 0x34, 0x56)));
        // An open namespace takes a name nothing produces yet, which is
        // what lets a server's semantic token types be coloured before
        // bish knows about them.
        assert_eq!(shell.run_hl(&strs(&["--set", "lsp_type_parameter", "#abcdef"])), 0);
        assert_eq!(shell.hl_color("lsp_type_parameter"), Some(vt100::Color::Rgb(0xab, 0xcd, 0xef)));
        // Unsetting takes it back to "nothing said".
        assert_eq!(shell.run_hl(&strs(&["--unset", "string"])), 0);
        assert_eq!(shell.hl_color("string"), None);
        assert_eq!(shell.run_hl(&strs(&["--unset", "string"])), 1, "unsetting what is not set says so");
    }

    // The point of `::bish hl` being its own command but not its own
    // *concept*: a theme is one thing you switch to, and it carries the
    // palette along with the options.
    #[test]
    fn a_theme_captures_highlight_colours_alongside_bishopts() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bish_theme(&strs(&["begin"])), 0);
        shell.run_bishopt(&strs(&["--set", "theme", "midnight"]), KNOWN_BISHOPTS);
        shell.run_bishopt(&strs(&["--set", "ui_col_directory", "#111111"]), KNOWN_BISHOPTS);
        assert_eq!(shell.run_hl(&strs(&["--set", "string", "#222222"])), 0);
        assert_eq!(shell.run_bish_theme(&strs(&["end"])), 0);

        // Declaring is not switching, so nothing has changed yet.
        assert_eq!(shell.hl_color("string"), None);
        // Switching brings both halves.
        shell.run_bishopt(&strs(&["--set", "theme", "midnight"]), KNOWN_BISHOPTS);
        assert_eq!(shell.bishopt_color("ui_col_directory"), Some(vt100::Color::Rgb(0x11, 0x11, 0x11)));
        assert_eq!(shell.hl_color("string"), Some(vt100::Color::Rgb(0x22, 0x22, 0x22)));

        // Something set directly still wins over the theme, the same
        // precedence a bishopt has.
        assert_eq!(shell.run_hl(&strs(&["--set", "string", "#333333"])), 0);
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
        assert_eq!(shell.run_hl(&strs(&["--set", "string", "not-a-colour"])), 2);
        assert_eq!(shell.hl_color("string"), None);
    }

    #[test]
    fn bishopt_color_resolves_the_default_then_an_override_then_none_for_unknown() {
        let mut shell = Shell::new();
        // The chrome colours stayed bishopts, and still have defaults:
        // `-bish-blue` is ANSI slot 4, terminal-resolved, not a fixed
        // RGB -- so a fresh install renders as it always did.
        assert_eq!(shell.bishopt_color("ui_col_directory"), Some(vt100::Color::Indexed(4)));
        shell.run_bishopt(&strs(&["--set", "ui_col_directory", "#123456"]), KNOWN_BISHOPTS);
        assert_eq!(shell.bishopt_color("ui_col_directory"), Some(vt100::Color::Rgb(0x12, 0x34, 0x56)));
        assert_eq!(shell.bishopt_color("not_a_real_option"), None);
    }

    #[test]
    fn bishopt_color_accepts_a_vendor_ansi_reference_and_reads_it_back_verbatim() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bishopt(&strs(&["--set", "accent", "-bish-bright-red"]), &test_bishopts()), 0);
        assert_eq!(shell.bishopt_value(&test_bishopts(), "accent"), Some(BishOptValue::Color("-bish-bright-red".to_string(), vec![crate::csscolor::TermColor::Ansi(9)])));

        let buf = Rc::new(RefCell::new(String::new()));
        shell.set_sink_capture(buf.clone());
        shell.run_bishopt(&strs(&["accent"]), &test_bishopts());
        assert_eq!(buf.borrow().as_str(), "-bish-bright-red\n");
    }

    #[test]
    fn bishopt_set_rejects_a_vendor_color_used_inside_color_mix() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bishopt(&strs(&["--set", "accent", "color-mix(in srgb, -bish-red, blue)"]), &test_bishopts()), 2);
    }

    #[test]
    fn bishopt_set_accepts_a_font_family_style_fallback_list() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bishopt(&strs(&["--set", "accent", "#ff0000, -bish-ansi(1), -bish-red"]), &test_bishopts()), 0);
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
        shell.run_bishopt(&strs(&["accent"]), &test_bishopts());
        assert_eq!(buf.borrow().as_str(), "#ff0000, -bish-ansi(1), -bish-red\n");
    }

    #[test]
    fn bishopt_set_rejects_a_fallback_list_with_any_invalid_candidate() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_bishopt(&strs(&["--set", "accent", "red, not-a-color, blue"]), &test_bishopts()), 2);
    }

    #[test]
    fn bishopt_color_for_picks_the_best_candidate_the_terminals_support_allows() {
        use crate::csscolor::ColorSupport;
        let mut shell = Shell::new();
        shell.run_bishopt(&strs(&["--set", "ui_col_directory", "#ff0000, -bish-ansi(200), -bish-red"]), KNOWN_BISHOPTS);
        assert_eq!(shell.bishopt_color_for("ui_col_directory", ColorSupport::Truecolor), Some(vt100::Color::Rgb(255, 0, 0)));
        assert_eq!(shell.bishopt_color_for("ui_col_directory", ColorSupport::Ansi256), Some(vt100::Color::Indexed(200)));
        assert_eq!(shell.bishopt_color_for("ui_col_directory", ColorSupport::Ansi16), Some(vt100::Color::Indexed(1)));
        // Nothing in this particular list suits ColorSupport::None -- the
        // least-demanding candidate (last in the list) is still used.
        assert_eq!(shell.bishopt_color_for("ui_col_directory", ColorSupport::None), Some(vt100::Color::Indexed(1)));
        // And the same tiering through `::bish hl`, which shares the
        // candidate-picking with bishopt rather than re-deriving it.
        assert_eq!(shell.run_hl(&strs(&["--set", "string", "#ff0000, -bish-ansi(200), -bish-red"])), 0);
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
        assert_eq!(shell.run_compgen(&strs(&["-W", "banana apple cherry apple"])), 0);
        assert_eq!(buf.borrow().as_str(), "banana\napple\ncherry\napple\n", "no sort, no dedup -- matches real bash");

        buf.borrow_mut().clear();
        assert_eq!(shell.run_compgen(&strs(&["-W", "banana apple cherry", "--", "a"])), 0);
        assert_eq!(buf.borrow().as_str(), "apple\n");
    }

    #[test]
    fn compgen_exit_status_is_1_only_when_a_source_was_given_and_yielded_nothing() {
        let mut shell = Shell::new();
        let _buf = capture_output(&mut shell);
        // No source at all -- always exits 0, even with a trailing word and
        // even though nothing gets printed.
        assert_eq!(shell.run_compgen(&[]), 0);
        assert_eq!(shell.run_compgen(&strs(&["zzz"])), 0);
        // A real source that produced zero matches -- exits 1.
        assert_eq!(shell.run_compgen(&strs(&["-W", "", "--", "x"])), 1);
        assert_eq!(shell.run_compgen(&strs(&["-W", "abc def", "--", "zzz"])), 1);
        // A real source with a match -- exits 0.
        assert_eq!(shell.run_compgen(&strs(&["-W", "abc def", "--", "a"])), 0);
    }

    #[test]
    fn compgen_x_filter_excludes_by_default_and_keeps_only_matches_when_negated() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_compgen(&strs(&["-W", "apple banana avocado", "-X", "a*"]));
        assert_eq!(buf.borrow().as_str(), "banana\n");

        buf.borrow_mut().clear();
        shell.run_compgen(&strs(&["-W", "apple banana avocado", "-X", "!a*"]));
        assert_eq!(buf.borrow().as_str(), "apple\navocado\n");
    }

    #[test]
    fn compgen_prefix_and_suffix_wrap_every_candidate() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_compgen(&strs(&["-P", "<", "-S", ">", "-W", "a b"]));
        assert_eq!(buf.borrow().as_str(), "<a>\n<b>\n");
    }

    #[test]
    fn compgen_keyword_action_lists_bish_reserved_words_only() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_compgen(&strs(&["-A", "keyword"]));
        let owned: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        assert!(owned.iter().any(|n| n == "if"), "{owned:?}");
        assert!(owned.iter().any(|n| n == "done"), "{owned:?}");
        assert!(owned.iter().any(|n| n == "[["), "{owned:?}");
        // Real bash's own list also has "!" and "time" -- not reserved
        // words in bish's own grammar, so deliberately absent here.
        assert!(!owned.iter().any(|n| n == "!"), "{owned:?}");
        assert!(!owned.iter().any(|n| n == "time"), "{owned:?}");
    }

    #[test]
    fn compgen_signal_action_includes_exit_pseudo_signal_and_sig_prefixed_names() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_compgen(&strs(&["-A", "signal"]));
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
        shell.run_compgen(&strs(&["-A", "builtin"]));
        let via_a: Vec<String> = buf.borrow().lines().map(str::to_string).collect();

        buf.borrow_mut().clear();
        shell.run_compgen(&strs(&["-b"]));
        let via_flag: Vec<String> = buf.borrow().lines().map(str::to_string).collect();

        assert_eq!(via_a, via_flag);
        assert!(via_a.iter().any(|n| n == "cd"), "{via_a:?}");
        assert!(via_a.iter().any(|n| n == "compgen"), "{via_a:?}");
    }

    #[test]
    fn compgen_shorthand_flags_combine_into_one_token() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_compgen(&strs(&["-ab"]));
        let names: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        assert!(names.iter().any(|n| n == "cd"), "builtin action missing: {names:?}");
    }

    #[test]
    fn compgen_setopt_and_shopt_actions_mirror_their_own_registries() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_compgen(&strs(&["-A", "setopt"]));
        assert_eq!(buf.borrow().lines().collect::<Vec<_>>(), SET_O_OPTIONS.to_vec());

        buf.borrow_mut().clear();
        shell.run_compgen(&strs(&["-A", "shopt"]));
        let names: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        let expected: Vec<String> = KNOWN_SHOPT_OPTIONS.iter().map(|(n, _)| n.to_string()).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn compgen_rejects_an_unknown_action_name_and_an_unknown_option() {
        let mut shell = Shell::new();
        let _buf = capture_output(&mut shell);
        assert_eq!(shell.run_compgen(&strs(&["-A", "bogus"])), 2);
        assert_eq!(shell.run_compgen(&strs(&["-Z"])), 2);
        assert_eq!(shell.run_compgen(&strs(&["-o", "bogus"])), 2);
        assert_eq!(shell.run_compgen(&strs(&["-o", "nosort", "-W", "a"])), 0, "a recognized -o name must not error");
    }

    #[test]
    fn compgen_f_option_with_a_nonexistent_function_errors_and_exits_1() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        assert_eq!(shell.run_compgen(&strs(&["-F", "not_a_real_function", "--", "a"])), 1);
        // Capture is a combined stdout+stderr sink (see OutputSink::Capture's
        // own doc comment), so the error message lands right here too.
        assert_eq!(buf.borrow().as_str(), "bish: compgen: not_a_real_function: function not found\n");
    }

    #[test]
    fn compgen_v_option_stores_into_an_indexed_array_instead_of_printing() {
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        assert_eq!(shell.run_compgen(&strs(&["-V", "myarr", "-W", "a b c", "--", "a"])), 0);
        assert_eq!(buf.borrow().as_str(), "", "must not print when -V is given");
        assert_eq!(shell.arrays.get("myarr").map(|m| m.values().cloned().collect::<Vec<_>>()), Some(vec!["a".to_string()]));

        assert_eq!(shell.run_compgen(&strs(&["-V", "myarr", "-W", "a b c", "--", "zzz"])), 1, "same had-a-source-and-got-nothing rule applies under -V");
        assert_eq!(shell.arrays.get("myarr").map(|m| m.len()), Some(0));
    }

    #[test]
    fn compgen_disabled_is_always_empty_and_enabled_matches_known_builtins() {
        let mut shell = Shell::new();
        // Same "a source was given and yielded nothing" rule as any other
        // empty action -- `disabled` is a real, meaningfully-empty source,
        // not "no source at all", so this still exits 1.
        assert_eq!(shell.run_compgen(&strs(&["-A", "disabled"])), 1);
        let buf = capture_output(&mut shell);
        shell.run_compgen(&strs(&["-A", "enabled"]));
        let names: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        assert_eq!(names.len(), KNOWN_BUILTINS.len());
    }

    #[test]
    fn compgen_arrayvar_action_lists_both_indexed_and_associative_array_names() {
        let mut shell = Shell::new();
        shell.arrays.insert("idxarr".to_string(), std::collections::BTreeMap::new());
        shell.assoc_arrays.insert("assocarr".to_string(), OrderedMap::default());
        let buf = capture_output(&mut shell);
        shell.run_compgen(&strs(&["-A", "arrayvar"]));
        let names: Vec<String> = buf.borrow().lines().map(str::to_string).collect();
        assert!(names.iter().any(|n| n == "idxarr"), "{names:?}");
        assert!(names.iter().any(|n| n == "assocarr"), "{names:?}");
    }

    #[test]
    fn compgen_variable_action_sees_both_an_env_var_and_a_local_scope_var() {
        let mut shell = Shell::new();
        // SAFETY: single-threaded test setup/teardown of an env var this
        // test owns exclusively, same reasoning as this file's other
        // env-mutating tests.
        unsafe { std::env::set_var("BISH_COMPGEN_TEST_VAR", "1") };
        shell.var_scopes.push(HashMap::from([("local_only_var".to_string(), "x".to_string())]));
        let buf = capture_output(&mut shell);
        shell.run_compgen(&strs(&["-A", "variable"]));
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
        shell.run_compgen(&strs(&["-A", "alias"]));
        assert_eq!(buf.borrow().as_str(), "myalias\n");

        buf.borrow_mut().clear();
        shell.run_compgen(&strs(&["-A", "function"]));
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
        let status = shell.run_compgen(&strs(&["-G", &pattern]));

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
        shell.run_compgen(&strs(&["-f", "--", "sub/in"]));
        let split_result = buf.borrow().clone();

        buf.borrow_mut().clear();
        shell.run_compgen(&strs(&["-f"]));
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
    fn an_unrecognized_at_transform_letter_falls_back_to_a_literal_name() {
        // Matches every other "unrecognized operator syntax" case in
        // parse_brace_content -- `${v@Z}` isn't one of the four
        // implemented transform letters, so the whole `${...}` is
        // treated as a best-effort literal (nonexistent) variable name
        // rather than crashing or silently dropping the '@'.
        let mut shell = Shell::new();
        let buf = capture_output(&mut shell);
        shell.run_source_here(r#"v=hi; echo "[${v@Z}]""#, "<test>");
        assert_eq!(buf.borrow().as_str(), "[]\n");
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
        let status = shell.run_declare(&strs(&["-p", "DEFINITELY_NOT_SET_XYZ"]), &[]);
        assert_eq!(status, 1);
    }

    #[test]
    fn declare_f_prints_a_reparsable_function_definition() {
        let mut shell = Shell::new();
        shell.run_source_here("foo() { echo hi; }", "<test>");
        let buf = capture_output(&mut shell);
        assert_eq!(shell.run_declare(&strs(&["-f", "foo"]), &[]), 0);
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
        shell.run_declare(&strs(&["-F", "foo"]), &[]);
        assert_eq!(buf.borrow().as_str(), "declare -f foo\n");
    }

    #[test]
    fn declare_f_on_an_unknown_function_errors_and_exits_1() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_declare(&strs(&["-f", "not_a_real_function"]), &[]), 1);
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
        let mut shell = Shell::new();
        shell.run_source_here("set -r", "<test>");
        let result = shell.run_source_here("cd /tmp", "<test>");
        assert!(matches!(result, ExecResult::Status(1)), "{result:?}");
    }

    #[test]
    fn restricted_mode_is_a_one_way_latch() {
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
            let result = shell.run_source_here(&format!("{name}=/tmp"), "<test>");
            assert!(matches!(result, ExecResult::Status(0)), "{name}: {result:?}");
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
        assert_eq!(shell.run_bishopt(&strs(&["--set", "shiftwidth", "2"]), KNOWN_BISHOPTS), 0);
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
        assert_eq!(shell.run_bishopt(&strs(&["--describe", "nonsense"]), KNOWN_BISHOPTS), 1);
    }

    fn hook_ids(shell: &mut Shell) -> Vec<u64> {
        shell.hooks.iter().map(|h| h.id).collect()
    }

    #[test]
    fn adding_a_hook_returns_an_id_that_removes_it() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_hook(&strs(&["add", "editor:file:open", "__setup"])), 0);
        assert_eq!(shell.run_hook(&strs(&["add", "editor:file:close", "__teardown"])), 0);
        assert_eq!(hook_ids(&mut shell), vec![1, 2], "ids come from a counter, in order");
        assert_eq!(shell.run_hook(&strs(&["rm", "1"])), 0);
        assert_eq!(hook_ids(&mut shell), vec![2]);
        // ...and an id is never reused, so `rm` can't hit the wrong one.
        assert_eq!(shell.run_hook(&strs(&["add", "editor:file:open", "__again"])), 0);
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
            shell.run_bish(&strs(args));
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
            assert_eq!(shell.run_lsp(&strs(&["add", &format!("--apply-edits={value}"), "x"])), 0, "{value}");
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
        assert!(
            missing.is_empty(),
            "these are dispatched as builtins but missing from KNOWN_BUILTINS, so they break in a pipeline: {missing:?}"
        );
    }

    #[test]
    fn adding_a_language_server_returns_an_id_that_removes_it() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_lsp(&strs(&["add", "--lang=rust", "rust-analyzer"])), 0);
        assert_eq!(shell.run_lsp(&strs(&["add", "--lang=python", "pylsp"])), 0);
        assert_eq!(lsp_ids(&shell), vec![1, 2], "ids come from a counter, in order");
        assert_eq!(shell.run_lsp(&strs(&["rm", "1"])), 0);
        assert_eq!(lsp_ids(&shell), vec![2]);
        // Never reused, so `rm` can't hit the wrong one -- same
        // contract `hook` ids have.
        assert_eq!(shell.run_lsp(&strs(&["add", "gopls"])), 0);
        assert_eq!(lsp_ids(&shell), vec![2, 3]);
        assert_eq!(shell.run_lsp(&strs(&["rm", "99"])), 1);
        assert_eq!(shell.run_lsp(&strs(&["rm", "nonsense"])), 2);
    }

    #[test]
    fn a_command_keeps_its_words_and_defaults_to_a_git_root() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_lsp(&strs(&["add", "--lang=c", "clangd", "--background-index"])), 0);
        let server = &shell.lsp_servers[0];
        assert_eq!(server.command, vec!["clangd".to_string(), "--background-index".to_string()]);
        assert_eq!(server.lang, "c");
        assert_eq!(server.root_markers, vec![".git".to_string()]);
        assert_eq!(server.command_line(), "clangd --background-index");
        // ...and a word that would stop being one word does get quoted.
        assert_eq!(shell.run_lsp(&strs(&["add", "some server", "--flag=a b"])), 0);
        assert_eq!(shell.lsp_servers[1].command_line(), "'some server' '--flag=a b'");
    }

    #[test]
    fn a_root_command_is_recorded_and_shown_instead_of_the_markers() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_lsp(&strs(&["add", "--lang=rust", "--root-cmd", "cargo metadata | json .workspace_root", "rust-analyzer"])), 0);
        assert_eq!(shell.lsp_servers[0].root_cmd, "cargo metadata | json .workspace_root");
        // `--root` still has its default, since the command is what
        // gets asked first and the markers are the fallback.
        assert_eq!(shell.lsp_servers[0].root_markers, vec![".git".to_string()]);

        // Either order, and the `=` spelling.
        assert_eq!(shell.run_lsp(&strs(&["add", "--root=go.mod", "--root-cmd=go env GOMOD", "gopls"])), 0);
        assert_eq!(shell.lsp_servers[1].root_cmd, "go env GOMOD");
        assert_eq!(shell.lsp_servers[1].root_markers, vec!["go.mod".to_string()]);

        // A flag with nothing usable after it is a config error, not a
        // silently empty command that would never run.
        assert_eq!(shell.run_lsp(&strs(&["add", "--root-cmd"])), 2);
        assert_eq!(shell.run_lsp(&strs(&["add", "--root-cmd", "   ", "x"])), 2);
        assert_eq!(shell.lsp_servers.len(), 2);
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
            let seen = out.borrow().clone();
            seen
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
        assert_eq!(shell.run_lsp(&strs(&["add", "--lang=rust", "rust-analyzer"])), 0);
        assert_eq!(shell.lsp_servers[0].apply_edits, "scoped", "the default is the one that needs no thought");

        assert_eq!(shell.run_lsp(&strs(&["add", "--apply-edits=always", "gopls"])), 0);
        assert_eq!(shell.lsp_servers[1].apply_edits, "always");
        assert_eq!(shell.run_lsp(&strs(&["add", "--apply-edits", "never", "clangd"])), 0);
        assert_eq!(shell.lsp_servers[2].apply_edits, "never");

        // A misspelling is a config error rather than a silent
        // downgrade: "--apply-edits=alwyas" quietly meaning `scoped`
        // is exactly the kind of thing nobody notices until a refactor
        // does nothing.
        assert_eq!(shell.run_lsp(&strs(&["add", "--apply-edits=sometimes", "x"])), 2);
        assert_eq!(shell.run_lsp(&strs(&["add", "--apply-edits"])), 2);
        assert_eq!(shell.lsp_servers.len(), 3);
    }

    // Four flags is enough that insisting on one order means the wrong
    // one silently becomes part of the command to run.
    #[test]
    fn add_takes_its_flags_in_any_order() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_lsp(&strs(&["add", "--apply-edits=always", "--root=Cargo.toml", "--lang=rust", "--root-cmd=cargo x", "ra", "--stdio"])), 0);
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
        assert_eq!(shell.run_lsp(&strs(&["add", "--root=Cargo.toml,.git", "rust-analyzer"])), 0);
        assert_eq!(shell.lsp_servers[0].root_markers, vec!["Cargo.toml".to_string(), ".git".to_string()]);
        assert_eq!(shell.run_lsp(&strs(&["add", "--root", "go.mod", "gopls"])), 0);
        assert_eq!(shell.lsp_servers[1].root_markers, vec!["go.mod".to_string()]);
        // A flag with nothing usable after it is a config error, not a
        // silently empty marker list that would never match anything.
        assert_eq!(shell.run_lsp(&strs(&["add", "--root=", "x"])), 2);
        assert_eq!(shell.run_lsp(&strs(&["add", "--root"])), 2);
        assert_eq!(shell.run_lsp(&strs(&["add"])), 2, "a registration with no command at all");
        assert_eq!(shell.lsp_servers.len(), 2);
    }

    #[test]
    fn an_unknown_lsp_subcommand_is_refused() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_lsp(&strs(&["restart"])), 2);
    }

    #[test]
    fn a_child_shell_inherits_the_declared_language_servers() {
        let mut shell = Shell::new();
        shell.run_lsp(&strs(&["add", "--lang=rust", "rust-analyzer"]));
        let child = shell.new_virtual_child();
        assert_eq!(child.lsp_servers, shell.lsp_servers);
        assert_eq!(child.next_lsp_id, shell.next_lsp_id, "or the child would hand out an id the parent already used");
    }

    #[test]
    fn removing_a_hook_that_is_not_there_fails() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_hook(&strs(&["rm", "99"])), 1);
        assert_eq!(shell.run_hook(&strs(&["rm", "nonsense"])), 2);
    }

    // A typo'd event is the mistake this can actually catch, and a hook
    // that never fires is the worst way to find out.
    #[test]
    fn an_unknown_event_is_refused() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_hook(&strs(&["add", "editor:file:prewrite", "__x"])), 2);
        assert!(shell.hooks.is_empty());
        assert_eq!(shell.run_hook(&strs(&["add", "editor:file:write:pre", "__x"])), 0);
    }

    #[test]
    fn a_hook_fires_only_for_its_event_and_language() {
        let mut shell = Shell::new();
        shell.run_hook(&strs(&["add", "--lang=rust", "editor:file:open", "__rust"]));
        shell.run_hook(&strs(&["add", "--lang", "!(rust)", "editor:file:open", "__other"]));
        shell.run_hook(&strs(&["add", "editor:file:open", "__any"]));
        shell.run_hook(&strs(&["add", "--lang=rust", "editor:file:close", "__bye"]));
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
            shell.run_hook(&strs(&["add", "editor:file:open", name]));
        }
        assert_eq!(shell.hooks_for("editor:file:open", "bash"), vec!["__first", "__second", "__third"]);
    }

    // A split window should behave like the one it was split from.
    #[test]
    fn a_child_shell_inherits_the_hooks() {
        let mut shell = Shell::new();
        shell.run_hook(&strs(&["add", "editor:file:open", "__setup"]));
        let child = shell.new_virtual_child();
        assert_eq!(child.hooks_for("editor:file:open", "bash"), vec!["__setup"]);
    }

    #[test]
    fn a_command_with_arguments_is_kept_whole() {
        let mut shell = Shell::new();
        shell.run_hook(&strs(&["add", "editor:file:open", "lsp", "start", "--quiet"]));
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
        assert_eq!(shell.run_hook(&strs(&["add", "shell:exec:pre", "__timer"])), 0);
        assert_eq!(shell.run_hook(&strs(&["add", "shell:cwd:change", "__ls"])), 0);
        assert_eq!(shell.hooks_for("shell:exec:pre", "bash"), vec!["__timer"]);
        assert_eq!(shell.hooks_for("shell:cwd:change", "bash"), vec!["__ls"]);
    }

    // A hook that causes its own event has to fire once, not forever.
    #[test]
    fn a_hook_cannot_trigger_more_hooks_while_it_runs() {
        let mut shell = Shell::new();
        shell.run_hook(&strs(&["add", "shell:cwd:change", "__cd_somewhere"]));
        assert_eq!(shell.hooks_for("shell:cwd:change", "bash").len(), 1);
        shell.set_firing_hooks(true);
        assert!(shell.hooks_for("shell:cwd:change", "bash").is_empty(), "nothing fires while a hook runs");
        shell.set_firing_hooks(false);
        assert_eq!(shell.hooks_for("shell:cwd:change", "bash").len(), 1);
    }
}
