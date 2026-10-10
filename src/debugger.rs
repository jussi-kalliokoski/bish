// The script debugger's non-UI half. `DebugSession` is the debugged
// script's persistent `Shell` and whatever outlives one `:dbg run`,
// `continue`, `next` or `step` (for `:dbg print` and `K` after a run ends).
// `PauseState` is the `DebugHook` installed for one run.
//
// A pause should feel like landing in the editor at that line, free to
// navigate and inspect with the real motions and `K`, then `:dbg continue`.
// But `PauseState::on_statement` can't reach repl.rs's `Frame::Edit` pane:
// `DebugHook` is `&mut Shell`-only (see exec.rs), keeping exec.rs apart
// from repl.rs's window state, and running the script as a separate
// process would hide breakpoints inside subshells and substitutions,
// which is why scripts run in-process. So a pause opens its own read-only
// `TextBuffer` on the file (the real one is read-only while `:dbg` is
// attached, so this copy can't go stale) and drives it with the editor's
// own machinery (`VimKeys`, `editor::apply_motion_or_reselect`, Visual
// yank, `fileeditor::build_editor_frame`, `docs::hover_lines_at`), painted
// into the editor pane's rect. The differences: `<C-w>` can't switch panes
// during the pause, and the colon line only takes `:dbg` subcommands,
// since `:w` and `:git` need window state this can't reach.
//
// While the script runs uninterrupted it gets the `Frame::DebugRun` pane
// (`repl.rs::split_debug_run_pane`; see `hand_off_to_script`), which a
// pause leaves alone.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;

use crate::bishedit::Buffer as _;
use crate::bishedit::registers::Registers;
use crate::bishedit::textbuffer::TextBuffer;
use crate::bishedit::unicode_width::col_of;
use crate::bishedit::vimkeys::{KeyOutcome, Op, VimKeys};
use crate::docs::{self, DocIndex};
use crate::editor::{self, Key};
use crate::exec::{DebugAction, DebugDepth, DebugHook, Shell};
use crate::fileeditor::{self, EditorMode};
use crate::term;
use crate::window::Rect;

// What a paused `PauseState` will do next.
#[derive(Clone, Copy)]
enum PendingStop {
    // Not stepping: only a breakpoint stops.
    None,
    // `s`/`step`: stop at the next statement, at any depth.
    Anywhere,
    // `n`/`next`: stop at the next statement no deeper than this, so not
    // inside a function call or subshell entered meanwhile.
    AtOrBelow(DebugDepth),
}

// An attached `:dbg` session's persistent state, in repl.rs's
// `debug_frames`. Plain, not `Rc<RefCell<...>>`: the hook installed on
// `shell` is its own `Rc<RefCell<PauseState>>`, because `on_statement`
// runs inside `run_source_here`, which holds a borrow of the shell, and
// one shared RefCell would make it borrow itself.
pub struct DebugSession {
    pub(crate) shell: Shell,
    src: String,
}

impl DebugSession {
    // Reads `path` once: the attached buffer is read-only while attached
    // (`TextBuffer::set_readonly`), so there's nothing to go stale against.
    pub fn attach(path: &Path) -> std::io::Result<DebugSession> {
        if !cfg!(feature = "dap") {
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "the debugger is not part of this build"));
        }
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

// The `DebugHook` installed on `DebugSession::shell` for one `:dbg run`,
// `continue`, `next` or `step` (see `DebugSession` for why it's separate),
// dropped with its `raw_guard` and `nav_buf` when the call returns. Only
// the final cursor (`nav_cursor`) is read back out.
pub struct PauseState {
    pending_stop: PendingStop,
    paused_at: Option<usize>,
    quit_requested: bool,
    // Whether this run paused at all, so `nav_cursor` doesn't hand back
    // `nav_buf`'s untouched (0, 0) and reset the editor's cursor to the top
    // after a run that hit no breakpoint.
    visited: bool,
    // The `Frame::DebugRun` pane's rect; see `hand_off_to_script`.
    run_rect: Rect,
    // The `Frame::Edit` pane's rect, which a pause paints into.
    editor_rect: Rect,
    term_rows: usize,
    term_cols: usize,
    // A separate read-only open of the file, where navigation during a pause
    // happens; the editor's own buffer is out of reach (see the module
    // header). Reopened each run, so it matches the real one.
    nav_buf: TextBuffer,
    vk: VimKeys,
    registers: Registers,
    docs: DocIndex,
    // `K`'s popup, as in repl.rs's Normal-mode loop: cleared by any other
    // key, rebuilt on each press.
    hover_lines: Vec<String>,
    // A one-line message (a `:dbg print` or `help` result, or an unknown
    // command), shown instead of the status text until the next key.
    message: Option<String>,
    // A raw-mode guard of its own for this run, nested inside the editor's.
    // `suspend_raw`/`resume_raw` (term.rs) read the live termios state each
    // time, which makes nesting safe.
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

    // The cursor's final position in the pause's buffer, `Some` only if
    // this run paused (see `visited`). repl.rs's `dbg run` arm applies it to
    // the editor's buffer, so the cursor stays where it was left, as when
    // leaving any pane.
    pub fn nav_cursor(&self) -> Option<(usize, usize)> {
        self.visited.then(|| self.nav_buf.cursor())
    }

    // Hands the terminal to the script while it runs uninterrupted, as
    // vim's `:!command` leaves its display: cooked mode (`suspend_raw`)
    // restores echo and line editing, and the output sink is the real one, so
    // `read -p` works. Prints one banner at the `Frame::DebugRun` pane's
    // corner, once per stretch (`handed_off`); the script's output after that
    // isn't confined to the pane, as with `:!`.
    fn hand_off_to_script(&mut self, shell: &mut Shell) {
        if self.handed_off {
            return;
        }
        self.handed_off = true;
        self.raw_guard.suspend_raw();
        shell.set_sink_real();
        shell.clear_stdio_override();
        print!(
            "\x1b[{};{}H\x1b[?25h(script running -- back to the debugger once it pauses or finishes)\r\n",
            self.run_rect.row + 1,
            self.run_rect.col + 1
        );
        let _ = std::io::stdout().flush();
    }

    // The inverse: raw mode back for single-key pause navigation. The next
    // `render_editor` repaints the whole pane.
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

    // The global status row's text (as the editor's): the message if one is
    // showing, else the paused-at-line reminder, with position and line count
    // on the right, as `fileeditor::status_text` lays it out.
    fn status_text(&self) -> String {
        let mut left = self
            .message
            .clone()
            .unwrap_or_else(|| format!("-- PAUSED at line {} -- :dbg continue/next/step/print/quit/help --", self.paused_at.unwrap_or(0)));
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

    // The cursor's screen position, allowing for scroll and the gutter: the
    // hover popup's anchor, computed as repl.rs's `K` does.
    fn cursor_screen_pos(&self) -> (usize, usize) {
        let rect = self.editor_rect;
        let (row, col) = self.nav_buf.cursor();
        let gutter_width = rect.cols.saturating_sub(fileeditor::editor_content_cols(&self.nav_buf, rect));
        let screen_row = rect.row + row.saturating_sub(self.nav_buf.viewport_top());
        let cursor_col = col_of(&self.nav_buf.line_chars(row), col);
        let screen_col = rect.col + gutter_width + cursor_col.saturating_sub(self.nav_buf.viewport_left());
        (screen_row, screen_col)
    }

    fn render_hover_popup(&self) -> String {
        let (row, col) = self.cursor_screen_pos();
        fileeditor::render_hover_popup(&self.hover_lines, row, col, self.editor_rect)
    }

    // `K`: `docs::hover_lines_at`, as the editor's `K` uses, with live values
    // answered straight from `shell`.
    fn show_hover(&mut self, shell: &Shell) {
        let (row, col) = self.nav_buf.cursor();
        let chars = self.nav_buf.line_chars(row);
        let line_text: String = chars.iter().collect();
        self.hover_lines = match docs::hover_lines_at(&chars, col, &line_text, &self.docs, |name| shell.debug_peek_var(name)) {
            lines if lines.is_empty() => vec![docs::NOTHING_UNDER_THE_CURSOR.to_string()],
            lines => lines,
        };
    }

    // Repaints the editor pane's rect: the status row (or the colon line
    // being typed), the file via `fileeditor::build_editor_frame`, and the
    // hover popup if any, then puts the cursor where input is.
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
        out.push_str(&fileeditor::build_editor_frame(
            &self.nav_buf,
            &self.vk,
            EditorMode::Normal,
            self.editor_rect,
            self.editor_rect.row,
            self.editor_rect.col,
            None,
        ));
        out.push_str(&self.render_hover_popup());
        let (row, col) = match colon_input {
            Some(buf) => (self.term_rows.saturating_sub(2), 1 + buf.chars().count()),
            None => self.cursor_screen_pos(),
        };
        out.push_str(&format!("\x1b[{};{}H", row + 1, col + 1));
        print!("{out}");
        let _ = std::io::stdout().flush();
    }

    // A small colon-line reader: Enter submits, Escape or Backspace on empty
    // cancels, as the real command line, without history. Drawn in the status
    // row, like the editor's `:`.
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

    // The same subcommands and aliases as `:dbg` (repl.rs's `dbg` arm). A
    // leading `dbg ` is accepted but not needed: this line only takes debug
    // commands. Returns `Some` only for continue, next, step and quit, which
    // resume; print, help and anything unknown set `message`.
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
                self.message = Some(
                    "dbg (paused): :dbg continue|next|step|print NAME|quit|help (short c/n/s/p/q/h) -- real vim navigation otherwise, K hovers"
                        .to_string(),
                );
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

    // vim motions, search, marks, jumps and Visual yank, applied to
    // `nav_buf` with the helpers `run_normal_mode_navigation` uses. Mutating
    // outcomes and `Window` aren't matched at all, the same
    // enforced-by-omission as `TextBuffer::readonly`.
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
            KeyOutcome::SwapVisualEnds => {
                if let Some((_, anchor)) = self.vk.visual_anchor() {
                    let cursor = self.nav_buf.cursor();
                    self.nav_buf.set_cursor(anchor.0, anchor.1);
                    self.vk.set_visual_anchor(cursor);
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
    // Blocks in place, on the same thread; see the module header.
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
        // About to pause: take the terminal back first (a no-op if it was
        // never handed off).
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
            // Any key but `K` dismisses the hover popup, and any key the
            // message.
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
                                // Continue, next and step resume the
                                // script, so the terminal goes back to it;
                                // quit runs nothing more.
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
