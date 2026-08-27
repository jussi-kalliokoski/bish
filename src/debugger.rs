// The script debugger's own non-UI half: `DebugSession` (the debugged
// script's own persistent `Shell` plus whatever else outlives a single
// `:dbg run`/`continue`/`next`/`step`, e.g. for `:dbg print`/`K` hover
// after a run has already finished) and `PauseState` (the `DebugHook`
// actually installed for the *duration* of one run, holding what a
// single paused-in-place moment needs).
//
// This used to be a completely standalone view, then (briefly) a small
// status pill drawn into the `Frame::DebugRun` sibling pane while
// paused -- both superseded, see plan.md's own notes on why. The
// deciding user feedback on the pill: pausing shouldn't show yet another
// distinct UI at all -- it should feel like landing *in the editor* at
// that exact line, free to navigate around and inspect things with the
// real motions/`K`, then `:dbg continue` (or `next`/`step`) to resume.
//
// `PauseState::on_statement` is still the one place that can't reach
// repl.rs's real `Frame::Edit` pane at all (see `DebugHook`'s own doc
// comment in exec.rs: deliberately `&mut Shell`-only, so exec.rs stays
// decoupled from repl.rs's window/session state; and running the script
// as a real separate process to avoid this would break breakpoint
// visibility inside subshells/command substitutions, the entire reason
// this whole feature runs scripts in-process). So instead of reaching
// that pane, a pause opens its *own* `TextBuffer` (a fresh, read-only
// re-open of the same file -- safe, since the real buffer is readonly
// for as long as any `:dbg` session is attached, so there's nothing for
// this second copy to ever go stale against) and drives it with the
// exact same real machinery the actual editor pane uses underneath --
// `VimKeys`, `editor::apply_motion_or_reselect`, Visual mode + yank,
// `fileeditor::build_editor_frame`, `docs::hover_lines_at` for `K` --
// painted directly into the real `Frame::Edit` pane's own rect. Visually
// indistinguishable from actually being in that pane; the only real
// difference is `<C-w>` can't switch panes during this one blocked
// moment (inherent to blocking in place, not an oversight) and only
// `:dbg <subcommand>` (not the file's own `:w`/`:git`/etc, which need
// real `&mut sessions`/`&mut windows` this can't reach) works from its
// colon-line.
//
// The running script itself still gets the real `Frame::DebugRun`
// sibling pane (`repl.rs::split_debug_run_pane`) for as long as it's
// actually executing uninterrupted (see `hand_off_to_script`'s own doc
// comment) -- untouched by a pause, which only ever paints into the
// editor pane's own separate rect.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;

use crate::bishedit::registers::Registers;
use crate::bishedit::textbuffer::TextBuffer;
use crate::bishedit::vimkeys::{KeyOutcome, Op, VimKeys};
use crate::bishedit::Buffer as _;
use crate::docs::{self, DocIndex};
use crate::editor::{self, Key};
use crate::exec::{DebugAction, DebugDepth, DebugHook, Shell};
use crate::fileeditor::{self, EditorMode};
use crate::repl::Rect;
use crate::term;

// What a paused PauseState is waiting to do next.
#[derive(Clone, Copy)]
enum PendingStop {
    // Not stepping -- only an explicit breakpoint stops execution.
    None,
    // `s`/`step`: stop at the very next statement, any depth.
    Anywhere,
    // `n`/`next`: stop at the next statement whose depth is no deeper
    // than this -- i.e. don't stop again just because execution
    // descended into a function call or a converted foreground subshell/
    // command-substitution in the meantime.
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
// its own borrow open (see `PauseState`'s own doc comment for why the
// hook is a separate, small `Rc<RefCell<PauseState>>` instead).
pub struct DebugSession {
    pub(crate) shell: Shell,
    src: String,
}

impl DebugSession {
    // Reads `path` once, up front -- the buffer this session is attached
    // to is readonly for as long as it stays attached (repl.rs's own
    // `:dbg` handling, `TextBuffer::set_readonly`), so there's no live-
    // editing story to go stale against.
    pub fn attach(path: &Path) -> std::io::Result<DebugSession> {
        let src = std::fs::read_to_string(path)?;
        let mut shell = Shell::new();
        shell.set_script_args(path.to_string_lossy().to_string(), Vec::new());
        Ok(DebugSession { shell, src })
    }

    pub fn peek_var(&self, name: &str) -> Option<String> {
        self.shell.debug_peek_var(name)
    }

    pub fn source(&self) -> &str {
        &self.src
    }
}

// The `DebugHook` actually installed on `DebugSession::shell` for the
// duration of exactly one `:dbg run`/`continue`/`next`/`step` call --
// see `DebugSession`'s own doc comment for why this is a separate `Rc<
// RefCell<...>>` rather than living on `DebugSession` itself. Dropped
// (and its own `raw_guard`/`nav_buf` along with it) the instant that
// call returns -- nothing here is meant to outlive a single run, except
// the final cursor position (`nav_cursor`), read back out by the caller
// right before dropping this.
pub struct PauseState {
    pending_stop: PendingStop,
    paused_at: Option<usize>,
    quit_requested: bool,
    // Whether this run ever actually paused -- `nav_cursor`'s own guard
    // against handing back a `nav_buf` cursor that was never actually
    // navigated anywhere this run (still sitting at `TextBuffer::open`'s
    // own default (0, 0), which would otherwise silently reset the real
    // editor pane's cursor to the top of the file after an uneventful
    // `:dbg run` that never hit a breakpoint at all).
    visited: bool,
    // The real `Frame::DebugRun` pane's own rect -- see `hand_off_to_
    // script`'s own doc comment; untouched by a pause.
    run_rect: Rect,
    // The real `Frame::Edit` pane's own rect -- what a pause paints
    // into, see this module's own top-of-file doc comment.
    editor_rect: Rect,
    term_rows: usize,
    term_cols: usize,
    // A second, independent, read-only re-open of the same file being
    // debugged -- real navigation during a pause happens here, not on
    // the actual editor pane's own buffer (unreachable from here at all,
    // see this module's own top-of-file doc comment). Never diverges
    // from the real one: both are the same readonly file, and this one
    // is freshly re-opened for every run.
    nav_buf: TextBuffer,
    vk: VimKeys,
    registers: Registers,
    docs: DocIndex,
    // `K`'s own popup content -- see repl.rs's identical field on the
    // real editor pane's own Normal-mode loop for the shared convention
    // (cleared by any key other than `K`, rebuilt fresh each press).
    hover_lines: Vec<String>,
    // A transient one-line message (a `:dbg print`/`help` result, or an
    // unrecognized command) -- shown in place of the ordinary status
    // text for exactly one render, then cleared the moment any further
    // key arrives (whether or not that key sets a new one).
    message: Option<String>,
    // A *nested* raw-mode guard, freshly created for this one run (not
    // the ambient one `run_normal_mode_navigation` already holds for the
    // whole editor session) -- same reasoning this module's original
    // standalone design had for its own guard: `suspend_raw`/
    // `resume_raw` (term.rs) derive fresh from the *live* termios state
    // on every call rather than a stored snapshot, exactly so nesting
    // like this behaves correctly.
    raw_guard: Rc<term::RawGuard>,
    handed_off: bool,
}

impl PauseState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: &Path,
        breakpoints: BTreeSet<usize>,
        run_rect: Rect,
        editor_rect: Rect,
        term_rows: usize,
        term_cols: usize,
        raw_guard: Rc<term::RawGuard>,
    ) -> std::io::Result<PauseState> {
        let mut nav_buf = TextBuffer::open(path, editor_rect.rows.max(1))?;
        nav_buf.breakpoints = breakpoints;
        nav_buf.set_readonly(true);
        let docs = DocIndex::build_from_source(&nav_buf.text(), path);
        Ok(PauseState {
            pending_stop: PendingStop::None,
            paused_at: None,
            quit_requested: false,
            visited: false,
            run_rect,
            editor_rect,
            term_rows,
            term_cols,
            nav_buf,
            vk: VimKeys::new(),
            registers: Registers::new(),
            docs,
            hover_lines: Vec::new(),
            message: None,
            raw_guard,
            handed_off: false,
        })
    }

    pub fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    // The cursor's own final position within the pause's own navigation
    // buffer -- `Some` only if a pause actually happened this run (see
    // `visited`'s own doc comment). The caller (repl.rs's own "dbg run"
    // arm) applies this to the *real* editor pane's own buffer once this
    // whole call returns, so navigating around while paused leaves the
    // cursor wherever it was left, matching how leaving any other pane
    // and coming back works everywhere else in this codebase.
    pub fn nav_cursor(&self) -> Option<(usize, usize)> {
        self.visited.then(|| self.nav_buf.cursor())
    }

    // Cedes the real terminal to the script for as long as it keeps
    // running uninterrupted -- exactly like running it outside the
    // debugger entirely, the same way vim's own `:!command` temporarily
    // leaves its own full-screen display for a shell command rather than
    // trying to show both at once. This is what actually makes a script
    // that reads from the user (`read -p`, most visibly) work: cooked
    // mode (`raw_guard::suspend_raw`) restores kernel-driven echo/line-
    // editing, and a real (not captured) sink means the prompt itself
    // shows up immediately. Prints one small banner at the `Frame::
    // DebugRun` pane's own corner the first time this fires in a row
    // (idempotent via `handed_off`) -- everything the script prints past
    // that point is a real, unclipped terminal write (no pty layer here
    // to actually confine it to one pane's own columns), the same
    // accepted tradeoff vim's own `:!command` makes, just anchored to
    // this pane's own corner instead of the very top-left.
    fn hand_off_to_script(&mut self, shell: &mut Shell) {
        if self.handed_off {
            return;
        }
        self.handed_off = true;
        self.raw_guard.suspend_raw();
        shell.set_sink_real();
        shell.clear_stdio_override();
        print!("\x1b[{};{}H\x1b[?25h(script running -- back to the debugger once it pauses or finishes)\r\n", self.run_rect.row + 1, self.run_rect.col + 1);
        let _ = std::io::stdout().flush();
    }

    // The inverse of hand_off_to_script -- puts the terminal back into
    // raw/no-echo mode so this struct's own single-keystroke reads work
    // for pause navigation. No explicit redraw here: the very next
    // render_editor call (this loop's first iteration once paused)
    // already repaints the whole editor pane's own rect from scratch.
    fn reclaim_from_script(&mut self) {
        if !self.handed_off {
            return;
        }
        self.handed_off = false;
        self.raw_guard.resume_raw();
    }

    fn read_key(&self) -> Option<Key> {
        editor::read_key_idle(&mut || {}).ok().flatten()
    }

    // The status row's own text (repl.rs's `render_global_status_row`,
    // the exact same global row -- not a pane-local one -- the real
    // editor uses for its own `-- NORMAL --`/position indicator): a
    // transient `message` if one's showing, else the paused-at-line
    // reminder, right-aligned with the cursor's own position/line count,
    // matching `fileeditor::status_text`'s own layout convention.
    fn status_text(&self) -> String {
        let mut left = self.message.clone().unwrap_or_else(|| format!("-- PAUSED at line {} -- :dbg continue/next/step/print/quit/help --", self.paused_at.unwrap_or(0)));
        let (row, col) = self.nav_buf.cursor();
        let total = self.nav_buf.line_count();
        let right = format!("{},{}  {}/{}", row + 1, col + 1, row + 1, total);
        let left_len = left.chars().count();
        let right_len = right.chars().count();
        if left_len + right_len < self.term_cols {
            left.push_str(&" ".repeat(self.term_cols - left_len - right_len));
            left.push_str(&right);
            left
        } else {
            left.chars().take(self.term_cols).collect()
        }
    }

    // The cursor's own current screen position (row, col), accounting
    // for scroll and the gutter -- render_hover_popup's own anchor
    // point, same arithmetic repl.rs's own `K` arm uses for the real
    // editor pane.
    fn cursor_screen_pos(&self) -> (usize, usize) {
        let rect = self.editor_rect;
        let (row, col) = self.nav_buf.cursor();
        let gutter_width = rect.cols.saturating_sub(fileeditor::editor_content_cols(&self.nav_buf, rect));
        let screen_row = rect.row + row.saturating_sub(self.nav_buf.viewport_top());
        let screen_col = rect.col + gutter_width + col.saturating_sub(self.nav_buf.viewport_left());
        (screen_row, screen_col)
    }

    fn render_hover_popup(&self) -> String {
        let (row, col) = self.cursor_screen_pos();
        fileeditor::render_hover_popup(&self.hover_lines, row, col, self.editor_rect)
    }

    // `K`'s own hover lookup -- the exact same shared `docs::
    // hover_lines_at` repl.rs's own real editor `K` arm uses, just with
    // the live-value tier answered directly from `shell` (always
    // reachable here, unlike that arm which has to go through a
    // `debug_frames` lookup first).
    fn show_hover(&mut self, shell: &Shell) {
        let (row, col) = self.nav_buf.cursor();
        let chars = self.nav_buf.line_chars(row);
        let line_text: String = chars.iter().collect();
        self.hover_lines = docs::hover_lines_at(&chars, col, &line_text, &self.docs, |name| shell.debug_peek_var(name));
    }

    // Draws the real editor pane's own rect from scratch: the global
    // status row (either the ordinary paused reminder, or a colon-line
    // being typed), the file content via the exact same `fileeditor::
    // build_editor_frame` the real pane uses, and `K`'s own hover popup
    // if one's showing -- then leaves the real cursor at whichever of
    // those is actually live right now.
    fn render_editor(&self, colon_input: Option<&str>) {
        let status = match colon_input {
            Some(buf) => {
                let mut s = format!(":{buf}");
                let len = s.chars().count();
                if len < self.term_cols {
                    s.push_str(&" ".repeat(self.term_cols - len));
                } else {
                    s = s.chars().take(self.term_cols).collect();
                }
                s
            }
            None => self.status_text(),
        };
        let mut out = crate::repl::render_global_status_row(&status, self.term_rows);
        out.push_str(&fileeditor::build_editor_frame(&self.nav_buf, &self.vk, EditorMode::Normal, self.editor_rect, self.editor_rect.row, self.editor_rect.col, None));
        out.push_str(&self.render_hover_popup());
        let (row, col) = match colon_input {
            Some(buf) => (self.term_rows.saturating_sub(2), 1 + buf.chars().count()),
            None => self.cursor_screen_pos(),
        };
        out.push_str(&format!("\x1b[{};{}H", row + 1, col + 1));
        print!("{out}");
        let _ = std::io::stdout().flush();
    }

    // A small colon-line reader -- Enter submits, Escape/empty-Backspace
    // cancels, matching repl.rs's own real command-line convention just
    // without history/multi-line continuation. Drawn into the global
    // status row (render_editor's own `colon_input` branch) rather than
    // a separate row, so it reads as "the same status line, mid-type"
    // exactly like the real editor's own `:` does.
    fn read_colon_line(&mut self) -> Option<String> {
        let mut buf = String::new();
        loop {
            self.render_editor(Some(&buf));
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
    // leading "dbg " is accepted and stripped for anyone who types it
    // out of habit, but isn't required (this colon-line only ever means
    // a debug command in the first place -- everything else, including
    // real vim motions/operators, is handled entirely separately, see
    // `handle_nav_key`). Returns `Some(action)` only for continue/next/
    // step/quit, the only ones that actually resume the script;
    // print/help/an unrecognized line just set `message` and return
    // `None`.
    fn dispatch(&mut self, line: &str, depth: DebugDepth, shell: &Shell) -> Option<DebugAction> {
        let line = line.trim().strip_prefix("dbg ").unwrap_or(line.trim()).trim();
        match line {
            "c" | "continue" => {
                self.paused_at = None;
                Some(DebugAction::Continue)
            }
            "n" | "next" => {
                self.pending_stop = PendingStop::AtOrBelow(depth);
                self.paused_at = None;
                Some(DebugAction::StepOver)
            }
            "s" | "step" => {
                self.pending_stop = PendingStop::Anywhere;
                self.paused_at = None;
                Some(DebugAction::StepInto)
            }
            "q" | "quit" => {
                self.paused_at = None;
                self.quit_requested = true;
                Some(DebugAction::Quit)
            }
            "h" | "help" | "?" => {
                self.message = Some("dbg (paused): :dbg continue|next|step|print NAME|quit|help (short c/n/s/p/q/h) -- real vim navigation otherwise, K hovers".to_string());
                None
            }
            _ => {
                if let Some(rest) = line.strip_prefix("print ").or_else(|| line.strip_prefix("p ")) {
                    let name = rest.trim();
                    self.message = Some(match shell.debug_peek_var(name) {
                        Some(v) => format!("{name} = {v}"),
                        None => format!("{name}: unset or not inspectable"),
                    });
                } else if !line.is_empty() {
                    self.message = Some(format!("bish: dbg: unknown command: {line}"));
                }
                None
            }
        }
    }

    // Real vim motions/search/marks/jumps/Visual-mode-plus-yank, applied
    // directly against `nav_buf` via the same buffer-generic helpers
    // repl.rs's own `run_normal_mode_navigation` uses for the identical
    // `KeyOutcome`s. Every mutating outcome (`EnterInsert`, a non-Yank
    // `Operator`/`OperatorLines`, `Put`, `DeleteCharForward`, `Join`,
    // surround, `ReplaceChar`, `EnterReplace`, `ToggleCase`,
    // `AdjustNumber`, `OpenLine`) or one needing window/pane state this
    // has none of (`Window`) is simply never matched at all -- the same
    // "enforced by omission" convention `TextBuffer::readonly` already
    // establishes for the real editor pane.
    fn handle_nav_key(&mut self, key: Key) {
        if self.vk.is_idle() && (self.vk.is_visual() || !self.nav_buf.selections.is_empty()) {
            match key {
                Key::Char('Z') => {
                    self.commit_active_selection();
                    let end = self.nav_buf.cursor();
                    self.vk.end_visual(end);
                    return;
                }
                Key::Char('y') => {
                    self.commit_active_selection();
                    let register = self.vk.take_pending_register();
                    let end = self.nav_buf.cursor();
                    self.nav_buf.yank_selections(&mut self.registers, register);
                    self.nav_buf.selections.clear();
                    self.vk.end_visual(end);
                    return;
                }
                Key::Escape | Key::CtrlC => {
                    let end = self.nav_buf.cursor();
                    self.vk.end_visual(end);
                    self.nav_buf.selections.clear();
                    return;
                }
                _ => {}
            }
        }
        match self.vk.feed(key) {
            KeyOutcome::Motion(m, count) => {
                editor::apply_motion_or_reselect(&mut self.vk, &mut self.nav_buf, m, count);
                let content_cols = fileeditor::editor_content_cols(&self.nav_buf, self.editor_rect);
                crate::repl::scroll_to_show_cursor(&mut self.nav_buf, content_cols);
            }
            KeyOutcome::EnterVisual(shape) => {
                let cursor = self.nav_buf.cursor();
                self.vk.begin_visual(shape, cursor);
            }
            KeyOutcome::ReselectVisual => {
                if let Some((shape, anchor, cursor)) = self.vk.last_visual() {
                    self.nav_buf.set_cursor(cursor.0, cursor.1);
                    self.vk.begin_visual(shape, anchor);
                }
            }
            KeyOutcome::Jump { forward } => {
                let current = self.nav_buf.cursor();
                let target = if forward { self.vk.jump_forward(current) } else { self.vk.jump_back(current) };
                if let Some((row, col)) = target {
                    let row = row.min(self.nav_buf.line_count() - 1);
                    let col = col.min(self.nav_buf.line_len(row));
                    self.nav_buf.set_cursor(row, col);
                    let content_cols = fileeditor::editor_content_cols(&self.nav_buf, self.editor_rect);
                    crate::repl::scroll_to_show_cursor(&mut self.nav_buf, content_cols);
                }
            }
            KeyOutcome::Operator(Op::Yank, motion, count, register) => {
                editor::yank_motion(&mut self.nav_buf, &mut self.registers, motion, count, register);
            }
            KeyOutcome::OperatorLines(Op::Yank, count, register) => {
                editor::yank_lines(&self.nav_buf, &mut self.registers, count, register);
            }
            _ => {}
        }
    }

    fn commit_active_selection(&mut self) {
        if let Some(range) = crate::repl::active_visual_range(&self.vk, &self.nav_buf) {
            self.nav_buf.selections.push(range);
        }
    }
}

impl DebugHook for PauseState {
    // Blocks in place, same thread -- see this module's own top-of-file
    // doc comment.
    fn on_statement(&mut self, line: usize, depth: DebugDepth, shell: &mut Shell) -> DebugAction {
        let should_pause = self.nav_buf.breakpoints.contains(&line)
            || match self.pending_stop {
                PendingStop::None => false,
                PendingStop::Anywhere => true,
                PendingStop::AtOrBelow(d) => depth <= d,
            };
        if !should_pause {
            self.hand_off_to_script(shell);
            return DebugAction::Continue;
        }
        // About to actually pause -- reclaim the terminal first (a
        // no-op if it was never handed off, e.g. a breakpoint on this
        // script's very first statement).
        self.reclaim_from_script();
        self.pending_stop = PendingStop::None;
        self.paused_at = Some(line);
        self.visited = true;
        self.nav_buf.set_cursor(line.saturating_sub(1), 0);
        let content_cols = fileeditor::editor_content_cols(&self.nav_buf, self.editor_rect);
        crate::repl::scroll_to_show_cursor(&mut self.nav_buf, content_cols);

        loop {
            self.render_editor(None);
            let key = match self.read_key() {
                Some(k) => k,
                None => {
                    self.quit_requested = true;
                    return DebugAction::Quit;
                }
            };
            // Any key other than K itself dismisses a showing hover
            // popup, and any key at all dismisses a showing `message` --
            // both are transient, not a mode.
            if !matches!(key, Key::Char('K')) {
                self.hover_lines.clear();
            }
            self.message = None;
            if self.vk.is_idle() {
                match key {
                    Key::Char('K') => {
                        self.show_hover(shell);
                        continue;
                    }
                    Key::Char(':') => {
                        if let Some(cmd) = self.read_colon_line() {
                            if let Some(action) = self.dispatch(&cmd, depth, shell) {
                                // Same reasoning `should_pause`'s own
                                // fast path above has -- continue/next/
                                // step all mean "let the real script
                                // resume running now," so the terminal
                                // needs handing back over to it; quit
                                // doesn't run anything further.
                                if !matches!(action, DebugAction::Quit) {
                                    self.hand_off_to_script(shell);
                                }
                                return action;
                            }
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            self.handle_nav_key(key);
        }
    }
}
