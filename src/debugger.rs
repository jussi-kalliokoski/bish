// The script debugger's own non-UI half: `DebugSession` (the debugged
// script's own persistent `Shell` plus whatever else outlives a single
// `:dbg run`/`continue`/`next`/`step`, e.g. for `:dbg print`/`K` hover
// after a run has already finished) and `PauseState` (the `DebugHook`
// actually installed for the *duration* of one run, holding only what a
// single paused-in-place moment needs).
//
// This used to be a completely standalone view (its own Normal-mode
// navigation subset, its own colon-line reader, its own full-terminal
// takeover) -- see plan.md's own note on that superseded design. It's
// gone now: the source view is just the real, read-only `Frame::Edit`
// pane everything else uses (repl.rs's `run_normal_mode_navigation`/
// `run_command_mode`, gated by `TextBuffer::is_readonly` -- see that
// field's own doc comment), and the running script gets a real sibling
// pane (`Frame::DebugRun`, `repl.rs::split_debug_run_pane`) that becomes
// focused while `:dbg run` (or `continue`/`next`/`step`) is actually
// driving it.
//
// One thing that couldn't move: a paused breakpoint (or a statement
// blocked on real input, e.g. `read -p`) still has to block in place,
// same thread, inside `DebugHook::on_statement` -- see that trait's own
// doc comment for why (exec.rs can't depend on repl.rs's window/session
// state, and running the script as a real separate process would break
// breakpoint visibility inside subshells/command substitutions, the
// entire reason this whole feature runs scripts in-process to begin
// with). `PauseState::on_statement` below is that one remaining
// exception: it paints directly into the `Frame::DebugRun` pane's own
// screen rect with plain positioned ANSI (`\x1b[{row};{col}H`), the same
// *kind* of low-level primitive repl.rs's own render_diagnostics_list_
// frame/render_output_pane use while *they're* the pane actively being
// driven, since there's no reaching repl.rs's real compositor from here
// either. `<C-w>` genuinely can't switch panes while this loop owns the
// terminal -- inherent to "block in place," not an oversight.
//
// `:dbg run` is the only thing that ever starts a fresh execution
// (`Shell::run_source_here`, blocking); once running, `:dbg continue`/
// `next`/`step`/`print`/`quit`/`help` (plus single-key aliases c/n/s/q/
// h/?, safe here since this pause loop has no vim motions of its own to
// collide with) are read directly by `PauseState`'s own loop while
// genuinely paused -- the *outer* `run_command_mode`'s own "dbg" arm
// (repl.rs) can't literally reach that moment (the whole process is
// blocked inside it), so its own `continue`/`next`/`step` cases mainly
// exist for a consistent, honest "not running" error if typed with
// nothing paused, sharing this same vocabulary rather than a
// second one.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::editor::{self, Key};
use crate::exec::{DebugAction, DebugDepth, DebugHook, Shell};
use crate::repl::Rect;
use crate::term;

// What a paused PauseState is waiting to do next -- see PendingStop's
// original doc comment (unchanged from the standalone design this
// replaces).
#[derive(Clone, Copy)]
enum PendingStop {
    None,
    Anywhere,
    AtOrBelow(DebugDepth),
}

// One attached `:dbg` session's own persistent state -- everything that
// outlives a single run/continue/next/step call, stored in repl.rs's own
// `debug_frames: HashMap<EditFrameId, DebugSession>`. Deliberately plain
// (not `Rc<RefCell<...>>`): `shell` lives here, and `DebugHook::
// on_statement` needs its own `Rc<RefCell<dyn DebugHook>>` (installed on
// *this* `shell`) -- putting `shell` and the hook behind the *same*
// RefCell would deadlock the instant on_statement tried to borrow itself
// while already being called through a `run_source_here` that's holding
// its own borrow open (see PauseState's own doc comment for why the
// hook is a separate, small `Rc<RefCell<PauseState>>` instead).
pub struct DebugSession {
    pub(crate) shell: Shell,
    src: String,
    src_lines: Vec<String>,
}

static NEXT_PAUSE_ID: AtomicUsize = AtomicUsize::new(0);

impl DebugSession {
    // Reads `path` once, up front -- the buffer this session is attached
    // to is readonly for as long as it stays attached (repl.rs's own
    // `:dbg` handling, `TextBuffer::set_readonly`), so there's no live-
    // editing story to go stale against.
    pub fn attach(path: &std::path::Path) -> std::io::Result<DebugSession> {
        let src = std::fs::read_to_string(path)?;
        let src_lines: Vec<String> = src.lines().map(|s| s.to_string()).collect();
        let mut shell = Shell::new();
        shell.set_script_args(path.to_string_lossy().to_string(), Vec::new());
        Ok(DebugSession { shell, src, src_lines })
    }

    pub fn peek_var(&self, name: &str) -> Option<String> {
        self.shell.debug_peek_var(name)
    }

    pub fn source(&self) -> &str {
        &self.src
    }

    pub fn source_lines(&self) -> &[String] {
        &self.src_lines
    }
}

// The DebugHook actually installed on `DebugSession::shell` for the
// duration of exactly one `:dbg run`/`continue`/`next`/`step` call --
// see DebugSession's own doc comment for why this is a separate `Rc<
// RefCell<...>>` rather than living on DebugSession itself. Dropped (and
// its own `raw_guard` along with it) the instant that call returns --
// nothing here is meant to outlive a single run.
pub struct PauseState {
    breakpoints: BTreeSet<usize>,
    pending_stop: PendingStop,
    paused_at: Option<usize>,
    quit_requested: bool,
    rect: Rect,
    src_lines: Vec<String>,
    // The running script's own combined stdout+stderr from builtins
    // (Shell::set_sink_capture), plus a spawned *external* process's own
    // stdout (see ext_stdout_path) -- only actually fills up during the
    // brief stretches this hook isn't handed off to the script (see
    // hand_off_to_script's own doc comment): mostly the run's very first
    // statement(s), and whatever runs again immediately after a pause,
    // right up until the next hand-off.
    output: Rc<RefCell<String>>,
    ext_stdout_path: std::path::PathBuf,
    ext_drain_offset: u64,
    // A *nested* raw-mode guard, freshly created for this one run (not
    // the ambient one `run_normal_mode_navigation` already holds for the
    // whole editor session) -- same reasoning debugger.rs's original
    // standalone `run()` had for its own guard: suspend_raw/resume_raw
    // (term.rs) were specifically fixed this session to derive fresh
    // from the *live* termios state on every call rather than a stored
    // snapshot, exactly so nesting like this behaves correctly.
    raw_guard: Rc<term::RawGuard>,
    handed_off: bool,
}

impl PauseState {
    pub fn new(breakpoints: BTreeSet<usize>, rect: Rect, src_lines: Vec<String>, raw_guard: Rc<term::RawGuard>) -> PauseState {
        let id = NEXT_PAUSE_ID.fetch_add(1, Ordering::Relaxed);
        PauseState {
            breakpoints,
            pending_stop: PendingStop::None,
            paused_at: None,
            quit_requested: false,
            rect,
            src_lines,
            output: Rc::new(RefCell::new(String::new())),
            ext_stdout_path: std::env::temp_dir().join(format!("bish-debugger-stdout-{}-{id}.tmp", std::process::id())),
            ext_drain_offset: 0,
            raw_guard,
            handed_off: false,
        }
    }

    pub fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    // Rewires `shell`'s own output paths into this pause's own captured
    // buffer and resets both to empty -- called once, right before
    // `shell.run_source_here` (see repl.rs's own "dbg run"/"continue"/
    // "next"/"step" handling).
    pub fn begin_capturing_output(&self, shell: &mut Shell) {
        self.output.borrow_mut().clear();
        shell.set_sink_capture(self.output.clone());
        if let Ok(file) = std::fs::File::create(&self.ext_stdout_path) {
            shell.set_stdout_capture_file(file);
        }
    }

    // Folds whatever new bytes a spawned external process has written to
    // ext_stdout_path since the last drain into `output` -- see the
    // original standalone design's identical method for why this needs
    // its own file at all (builtin output lands in `output` directly via
    // the sink and needs no draining).
    fn drain_external_output(&mut self) {
        let Ok(mut file) = std::fs::File::open(&self.ext_stdout_path) else { return };
        if file.seek(SeekFrom::Start(self.ext_drain_offset)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
            self.ext_drain_offset += buf.len() as u64;
            self.output.borrow_mut().push_str(&String::from_utf8_lossy(&buf));
        }
    }

    pub fn cleanup_ext_stdout_path(&self) {
        let _ = std::fs::remove_file(&self.ext_stdout_path);
    }

    fn output_lines(&self) -> Vec<String> {
        self.output.borrow().lines().map(|s| s.to_string()).collect()
    }

    // Cedes the real terminal to the script for as long as it keeps
    // running uninterrupted -- see the original standalone design's
    // identical method for the full "why" (cooked mode + a real,
    // uncaptured sink is what actually makes `read -p`'s own prompt show
    // up immediately). Positions at this pane's own rect origin rather
    // than clearing the whole real terminal -- everything the script
    // prints past that point is still, unavoidably, a real, unclipped
    // terminal write (there's no pty layer here to actually confine a
    // spawned process's own output to one pane's columns), matching vim's
    // own `:!command` taking over the *whole* display for the span of an
    // external command, just anchored to this pane's own corner instead
    // of the very top-left.
    fn hand_off_to_script(&mut self, shell: &mut Shell) {
        if self.handed_off {
            return;
        }
        self.handed_off = true;
        self.raw_guard.suspend_raw();
        shell.set_sink_real();
        shell.clear_stdio_override();
        print!("\x1b[{};{}H\x1b[?25h(script running -- back to the debugger once it pauses or finishes)\r\n", self.rect.row + 1, self.rect.col + 1);
        let _ = std::io::stdout().flush();
    }

    // The inverse of hand_off_to_script -- see that method's own doc
    // comment. No explicit redraw here: the caller's own very next
    // render() call (this loop's first iteration once paused, or
    // repl.rs's own compositor_redraw once the whole run ends) already
    // repaints from scratch.
    fn reclaim_from_script(&mut self, shell: &mut Shell) {
        if !self.handed_off {
            return;
        }
        self.handed_off = false;
        self.raw_guard.resume_raw();
        shell.set_sink_capture(self.output.clone());
        if let Ok(file) = std::fs::OpenOptions::new().append(true).open(&self.ext_stdout_path) {
            shell.set_stdout_capture_file(file);
        }
    }

    fn read_key(&self) -> Option<Key> {
        editor::read_key_idle(&mut || {}).ok().flatten()
    }

    // Draws entirely within this pause's own `rect`: a reverse-video
    // status pill on the first row ("paused at line N: <source text>"),
    // the most recent captured output filling whatever's left, and a
    // colon-line/hint on the last row.
    fn render(&self, prompt: Option<&str>) {
        let r = self.rect;
        if r.rows == 0 || r.cols == 0 {
            return;
        }
        let status = match self.paused_at {
            Some(line) => {
                let text = self.src_lines.get(line.saturating_sub(1)).map(|s| s.trim()).unwrap_or("");
                format!(" paused at line {line}: {text} ")
            }
            None => " dbg ".to_string(),
        };
        let mut out = format!("\x1b[{};{}H\x1b[K\x1b[7m", r.row + 1, r.col + 1);
        out.push_str(&status.chars().take(r.cols).collect::<String>());
        out.push_str("\x1b[0m");

        let content_rows = r.rows.saturating_sub(2);
        let lines = self.output_lines();
        let shown = &lines[lines.len().saturating_sub(content_rows)..];
        for i in 0..content_rows {
            out.push_str(&format!("\x1b[{};{}H\x1b[K", r.row + 2 + i, r.col + 1));
            if let Some(line) = shown.get(i) {
                out.push_str(&line.chars().take(r.cols).collect::<String>());
            }
        }
        if r.rows >= 2 {
            out.push_str(&format!("\x1b[{};{}H\x1b[K", r.row + r.rows, r.col + 1));
            match prompt {
                Some(p) => out.push_str(&format!(":{p}")),
                None => out.push_str("\x1b[2mc)ontinue n)ext s)tep p)rint q)uit h)elp  or `:dbg <name>`\x1b[0m"),
            }
        }
        print!("{out}");
        let _ = std::io::stdout().flush();
    }

    // A small colon-line reader -- Enter submits, Escape/empty-Backspace
    // cancels, matching repl.rs's own real command-line convention just
    // without history/multi-line continuation. `seed` pre-fills the
    // buffer (bare `p` seeds "print ", so only the name itself needs
    // typing); an empty Backspace still cancels once the seed itself has
    // been backed out of, not just for a literally-empty buffer.
    fn read_colon_line(&self, seed: &str) -> Option<String> {
        let mut buf = seed.to_string();
        loop {
            self.render(Some(&buf));
            match self.read_key()? {
                Key::Enter => return Some(buf),
                Key::Escape => return None,
                Key::Backspace => {
                    if buf.is_empty() {
                        return None;
                    }
                    buf.pop();
                }
                Key::Char(c) => buf.push(c),
                _ => {}
            }
        }
    }

    // Recognizes the exact same subcommand names/short aliases the outer
    // `:dbg` command mode does (repl.rs's own "dbg" arm) -- an optional
    // leading "dbg " is accepted and stripped for anyone who types it out
    // of habit, but isn't required (this whole loop's context is already
    // unambiguous). Returns `Some(action)` only for continue/next/step/
    // quit, since those are the only ones that actually resume the
    // script; print/help/an unrecognized line just update what render()
    // shows next and return `None`.
    fn dispatch(&mut self, line: &str, depth: DebugDepth, shell: &Shell) -> (Option<DebugAction>, Option<usize>) {
        let line = line.trim().strip_prefix("dbg ").unwrap_or(line.trim()).trim();
        match line {
            "c" | "continue" => {
                self.paused_at = None;
                (Some(DebugAction::Continue), None)
            }
            "n" | "next" => {
                self.pending_stop = PendingStop::AtOrBelow(depth);
                self.paused_at = None;
                (Some(DebugAction::StepOver), None)
            }
            "s" | "step" => {
                self.pending_stop = PendingStop::Anywhere;
                self.paused_at = None;
                (Some(DebugAction::StepInto), None)
            }
            "q" | "quit" => {
                self.paused_at = None;
                self.quit_requested = true;
                (Some(DebugAction::Quit), None)
            }
            "h" | "help" | "?" => {
                self.output.borrow_mut().push_str(
                    "dbg (paused): c)ontinue  n)ext  s)tep  p)rint NAME  q)uit  h)elp -- bare key or `:` then the long/short name, `dbg ` prefix optional\n",
                );
                (None, None)
            }
            _ => {
                if let Some(rest) = line.strip_prefix("print ").or_else(|| line.strip_prefix("p ")) {
                    let name = rest.trim();
                    let msg = match shell.debug_peek_var(name) {
                        Some(v) => format!("{name} = {v}\n"),
                        None => format!("{name}: unset or not inspectable\n"),
                    };
                    self.output.borrow_mut().push_str(&msg);
                } else if !line.is_empty() {
                    self.output.borrow_mut().push_str(&format!("bish: dbg: unknown command: {line}\n"));
                }
                (None, None)
            }
        }
    }
}

impl DebugHook for PauseState {
    // Blocks in place, same thread -- see this module's own top-of-file
    // doc comment.
    fn on_statement(&mut self, line: usize, depth: DebugDepth, shell: &mut Shell) -> DebugAction {
        self.drain_external_output();
        let should_pause = self.breakpoints.contains(&line)
            || match self.pending_stop {
                PendingStop::None => false,
                PendingStop::Anywhere => true,
                PendingStop::AtOrBelow(d) => depth <= d,
            };
        if !should_pause {
            self.hand_off_to_script(shell);
            return DebugAction::Continue;
        }
        self.reclaim_from_script(shell);
        self.pending_stop = PendingStop::None;
        self.paused_at = Some(line);

        loop {
            self.render(None);
            let key = match self.read_key() {
                Some(k) => k,
                None => {
                    self.quit_requested = true;
                    return DebugAction::Quit;
                }
            };
            match key {
                Key::Char(c @ ('c' | 'n' | 's' | 'q' | 'h' | '?')) => {
                    let (action, _) = self.dispatch(&c.to_string(), depth, shell);
                    if let Some(action) = action {
                        if !matches!(action, DebugAction::Quit) {
                            self.hand_off_to_script(shell);
                        }
                        return action;
                    }
                }
                Key::Char(c @ ('p' | ':')) => {
                    let seed = if c == 'p' { "print " } else { "" };
                    let Some(cmd) = self.read_colon_line(seed) else { continue };
                    let (action, _) = self.dispatch(&cmd, depth, shell);
                    if let Some(action) = action {
                        if !matches!(action, DebugAction::Quit) {
                            self.hand_off_to_script(shell);
                        }
                        return action;
                    }
                }
                _ => {}
            }
        }
    }
}
