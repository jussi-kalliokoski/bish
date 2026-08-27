// `bish tool debug <script>` -- a small, standalone interactive debugger.
// Deliberately NOT built on repl.rs's session/window/pane/compositor
// machinery: that infrastructure exists to multiplex several concurrent
// shells sharing one terminal, which this single-purpose view has no
// need for (see plan.md's own note on this). It prints straight to the
// real terminal with plain ANSI sequences, the same "no vt100 grid model
// involved" style fileeditor.rs's own render_editor_frame uses before a
// session is promoted into the windowed compositor.
//
// The source view itself IS the real file editor -- a genuine
// bishedit::TextBuffer, rendered via fileeditor::build_editor_frame (the
// exact same gutter/syntax-highlighting/cursor pipeline `e` uses), driven
// with the real VimKeys engine so every ordinary motion (hjkl, w/b/e,
// f/t, %, gg/G, search, marks, jumps, Visual mode + yank, ...) works
// exactly as it does in the real editor -- just with every *mutating*
// KeyOutcome (EnterInsert, Operator(Delete/Change/...), Put,
// DeleteCharForward, Join, ReplaceChar, EnterReplace, ToggleCase,
// AdjustNumber, OpenLine, surround) simply never wired to anything,
// matching the read-only convention `NavBuffer::ReadOnly` already
// establishes elsewhere in this codebase ("enforced by omission," not a
// buffer-level read-only flag). This is a smaller, self-contained
// re-derivation of repl.rs's own run_normal_mode_navigation for
// precisely that read-only-compatible subset, rather than threading a
// read-only flag through that much larger function (which also carries a
// lot of window/pane/session machinery this standalone view has no
// session or window to hook into) or faking up a whole sessions/windows
// map just to reuse it unchanged. Two things it does not attempt: vim
// macros (`q`/`@`) and Ctrl-W window commands -- neither is meaningful
// without repl.rs's own window/pane state.
//
// The debug/dbg command-mode builtin the user asked for is this view's
// own dedicated `:` colon-line reader (read_colon_line/dispatch_colon_
// command below) -- not a call into repl.rs's real run_command_mode,
// which needs live `&mut sessions`/`&mut windows` access unavailable
// from mid-run_program (see DebugController::on_statement's own doc
// comment for exactly why). Every debug-specific action (run, toggle a
// breakpoint, continue/next/step, quit) goes through this colon-line --
// there used to also be bare single-key shortcuts for these (b/r/c/n/s/
// q), removed after user feedback that they silently collided with what
// those same keys already mean as real vim motions/operators (b =
// word-back, s = substitute, n = repeat-search, c = the change operator,
// q = start a macro recording -- none of which worked correctly while
// debugging before this). `K` (hover the identifier under the cursor)
// deliberately stayed a bare key: it isn't a "debug shortcut" being
// added on top, it's the same key vim/neovim already use to show
// info/documentation for whatever's under the cursor (keywordprg,
// LSP hover) -- this is simply the first thing in this codebase to give
// that convention a real implementation, not a new one.
//
// `:dbg [file]`/`:debug [file]` in the real file editor's own command
// mode (run_command_mode, repl.rs) launches this same `run` entry point
// against the current (or given) file, refusing on a dirty buffer with
// no explicit path the same way `:git blame`/`:git diff` already do --
// see run_command_mode's own "dbg"/"debug" match arm.
//
// `K`'s own hover content (show_hover) tries three things in order, the
// first that has something to say wins: (1) the identifier's live value
// while a run is active/paused (Shell::debug_peek_var, unchanged from
// before); (2) a godoc-style `#`-comment doc attached to its definition
// (crate::docs -- this script's own precursor to a real LSP hover, see
// that module's own doc comment), covering both the entry script and
// whatever it statically `source`s; (3) a man-page snippet, for a name
// that's an external command rather than anything this script itself
// defines (crate::bishedit::manpages, already built for highlight.rs's
// flag/subcommand recognition and reused here as-is -- same cache, same
// non-blocking "Pending now, ready by the next redraw" contract). Shown
// in a small floating popup near the cursor (render_hover_popup) rather
// than the single-line status row the old value-only hover used --
// needed the moment a doc comment can be several lines long, and kept
// for the single-line cases too, so `K` behaves one consistent way
// regardless of which of the three answered it.

use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::rc::Rc;

use crate::bishedit::registers::Registers;
use crate::bishedit::textbuffer::TextBuffer;
use crate::bishedit::vimkeys::{KeyOutcome, Op, VimKeys};
use crate::bishedit::Buffer as _;
use crate::docs::DocIndex;
use crate::editor::{self, Key};
use crate::exec::{DebugAction, DebugDepth, DebugHook, Shell};
use crate::fileeditor::{self, EditorMode};
use crate::repl::Rect;
use crate::term;

// What a paused DebugController is waiting to do next, decided the last
// time it was actually stopped (a breakpoint, or the very first
// statement once `r`/Enter starts a run) and consulted by every
// subsequent on_statement call until it fires again.
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

pub struct DebugController {
    buf: TextBuffer,
    vk: VimKeys,
    registers: Registers,
    paused_at: Option<usize>,
    pending_stop: PendingStop,
    // Set by `:quit`/a failed key read while paused -- checked by run()
    // after run_program returns, to distinguish "the script simply
    // finished" from "the user asked to abandon this run" (both unwind
    // through the same ExecResult::Exit path from run_program's own
    // perspective, so this is the only way run() can tell them apart).
    quit_requested: bool,
    // Whether a script is actually mid-execution right now (true for the
    // whole span of run()'s own run_source_here call, including every
    // pause in between -- not just while genuinely stopped) -- lets
    // dispatch_colon_command give a real "not running"/"already running"
    // answer to :continue/:next/:step/:run instead of silently doing
    // nothing, since the same colon-line is reached from both run()'s
    // outer "nothing started yet" loop and on_statement's own paused
    // loop, and the two contexts disagree on which of those commands
    // make sense.
    running: bool,
    term_rows: usize,
    term_cols: usize,
    status: String,
    // The running script's own output -- combined stdout+stderr from
    // builtins (echo/printf/...) via Shell::set_sink_capture, plus a
    // spawned *external* process's own stdout (see ext_stdout_path's own
    // doc comment; a spawned command's stderr still goes straight to the
    // real terminal, the same accepted asymmetry `$(...)` already has).
    // Rendered in its own small pane below the source view by
    // render_output_pane -- a logical child of that view exactly the way
    // repl.rs's own `Frame::Diagnostics` can't exist without the
    // `Frame::Edit` pane it splits off from (see that type's own doc
    // comment): there's no window/pane machinery here to make that
    // relationship structural (see this module's own top-of-file doc
    // comment on why), so it's just "the other half of render()," always
    // drawn together, never addressable on its own. Cleared at the start
    // of every `:run`.
    output: Rc<RefCell<String>>,
    // A real temp file an external process's own stdout is redirected
    // into (Shell::set_stdout_capture_file) -- unlike builtin output,
    // which lands directly in `output` via the sink, a spawned process
    // writes to a real fd this shell doesn't otherwise get to intercept
    // in-process, so this is drained into `output` instead (see
    // drain_external_output). One path, reused (truncated) across every
    // `:run` in this debugger session rather than a fresh temp file each
    // time -- nothing else in this one-shot-per-process tool needs it to
    // survive past the run that wrote it.
    ext_stdout_path: std::path::PathBuf,
    // How much of ext_stdout_path has already been folded into `output`
    // -- see drain_external_output's own doc comment.
    ext_drain_offset: u64,
    // godoc-style doc comments harvested from the script itself plus
    // whatever it statically `source`s -- built once, up front (see
    // `run`'s own construction site), not re-scanned per hover: this
    // standalone view has no live-editing story for the file it's
    // debugging (read-only, see this module's own top-of-file doc
    // comment), so nothing here can go stale mid-session.
    docs: DocIndex,
    // `K`'s own popup content, one entry per already-wrapped-for-width
    // line -- empty means nothing is showing. Cleared by any key other
    // than `K` itself (see both call sites below); rebuilt fresh every
    // time `K` is pressed, never appended to.
    hover_lines: Vec<String>,
}

// How many of the output pane's own lines actually show at once while
// there's real content to show -- capped so a chatty script can't crowd
// the source view down to nothing; the pane itself still only takes up
// a collapsed single title row when there's no output yet at all (see
// output_pane_rows).
const MAX_OUTPUT_CONTENT_ROWS: usize = 8;

impl DebugController {
    fn new(buf: TextBuffer, term_rows: usize, term_cols: usize, docs: DocIndex) -> Self {
        DebugController {
            buf,
            vk: VimKeys::new(),
            registers: Registers::new(),
            paused_at: None,
            pending_stop: PendingStop::None,
            quit_requested: false,
            running: false,
            term_rows,
            term_cols,
            status: "hjkl/w/b/gg/G/...: navigate  K: hover  :run  :break [line]  :quit".to_string(),
            output: Rc::new(RefCell::new(String::new())),
            ext_stdout_path: std::env::temp_dir().join(format!("bish-debugger-stdout-{}.tmp", std::process::id())),
            ext_drain_offset: 0,
            docs,
            hover_lines: Vec::new(),
        }
    }

    fn output_lines(&self) -> Vec<String> {
        self.output.borrow().lines().map(|s| s.to_string()).collect()
    }

    // 1 (just the collapsed title) when nothing's been printed this run;
    // otherwise the title row plus up to MAX_OUTPUT_CONTENT_ROWS of the
    // most recent lines (a `tail -f`-style view, not a scrollable one --
    // there's no navigation into this pane, matching its "logical child,
    // not its own addressable thing" role).
    fn output_pane_rows(&self) -> usize {
        let count = self.output_lines().len();
        if count == 0 {
            1
        } else {
            1 + count.min(MAX_OUTPUT_CONTENT_ROWS)
        }
    }

    // One row reserved at the bottom for the status line/colon-line
    // (shared, exactly like the real editor's own status row -- there's
    // no tab bar here to also reserve a second row for), then whatever
    // output_pane_rows wants directly above that -- the source view gets
    // whatever's left, with at least 3 rows kept for it regardless of
    // how much output there is.
    fn rect(&self) -> Rect {
        let total = self.term_rows.saturating_sub(1).max(1);
        let output_rows = self.output_pane_rows().min(total.saturating_sub(3));
        Rect { row: 0, col: 0, rows: total.saturating_sub(output_rows), cols: self.term_cols }
    }

    fn output_rect(&self) -> Rect {
        let source = self.rect();
        let total = self.term_rows.saturating_sub(1).max(1);
        Rect { row: source.rows, col: 0, rows: total.saturating_sub(source.rows), cols: self.term_cols }
    }

    fn render(&self) {
        let rect = self.rect();
        let mut out = crate::repl::render_global_status_row(&self.status, self.term_rows);
        out.push_str(&fileeditor::build_editor_frame(&self.buf, &self.vk, EditorMode::Normal, rect, rect.row, rect.col, None));
        out.push_str(&self.render_output_pane());
        out.push_str(&self.render_hover_popup());
        print!("{}", out);
        let _ = std::io::stdout().flush();
    }

    // Draws the output pane's own title/divider row (same "dashes +
    // reverse-video pill + dashes" style repl.rs's own
    // render_diagnostics_title uses for the diagnostics pane's collapsed
    // title -- this is the same *kind* of thing, just drawn directly
    // instead of through repl.rs's session/window/pane machinery) plus
    // however many of the most recent output lines currently fit.
    fn render_output_pane(&self) -> String {
        let orect = self.output_rect();
        let lines = self.output_lines();
        let title = if lines.is_empty() {
            " output (nothing printed yet) ".to_string()
        } else {
            format!(" output ({} line{}) ", lines.len(), if lines.len() == 1 { "" } else { "s" })
        };
        let pill: String = title.chars().take(orect.cols).collect();
        let pill_len = pill.chars().count();
        let left = 2.min(orect.cols.saturating_sub(pill_len));
        let right = orect.cols.saturating_sub(pill_len + left);
        let mut out = format!("\x1b[{};{}H\x1b[K", orect.row + 1, orect.col + 1);
        out.push_str(&"─".repeat(left));
        out.push_str("\x1b[7m");
        out.push_str(&pill);
        out.push_str("\x1b[0m");
        out.push_str(&"─".repeat(right));

        let content_rows = orect.rows.saturating_sub(1);
        let shown = &lines[lines.len().saturating_sub(content_rows)..];
        for (i, line) in shown.iter().enumerate() {
            out.push_str(&format!("\x1b[{};{}H\x1b[K", orect.row + 2 + i, orect.col + 1));
            let clipped: String = line.chars().take(orect.cols).collect();
            out.push_str(&clipped);
        }
        out
    }

    // The cursor's own current screen position (row, col), accounting
    // for scroll (viewport_top/viewport_left) and the gutter (line
    // numbers, breakpoint markers) -- render_hover_popup's own anchor
    // point. `editor_content_cols` is the one piece of this arithmetic
    // fileeditor.rs already exposes; the gutter width itself is just
    // whatever's left of `rect.cols` once that's subtracted, no need for
    // fileeditor.rs's own (private) total_gutter_width.
    fn cursor_screen_pos(&self) -> (usize, usize) {
        let rect = self.rect();
        let (row, col) = self.buf.cursor();
        let gutter_width = rect.cols.saturating_sub(fileeditor::editor_content_cols(&self.buf, rect));
        let screen_row = rect.row + row.saturating_sub(self.buf.viewport_top());
        let screen_col = rect.col + gutter_width + col.saturating_sub(self.buf.viewport_left());
        (screen_row, screen_col)
    }

    // `K`'s own hover lookup -- docs::hover_lines (shared with repl.rs's
    // real file editor) does the actual three-tier lookup; this just
    // supplies the one thing only a debugger session actually has, the
    // identifier's live value.
    fn show_hover(&mut self, name: &str, shell: &Shell) {
        self.hover_lines = crate::docs::hover_lines(name, shell.debug_peek_var(name).as_deref(), &self.docs);
    }

    fn dismiss_hover(&mut self) {
        self.hover_lines.clear();
    }

    // Draws `hover_lines` in a small bordered popup anchored just below
    // the cursor's own screen position -- see fileeditor::render_hover_
    // popup's own doc comment (shared with repl.rs's real file editor).
    fn render_hover_popup(&self) -> String {
        let (cursor_row, cursor_col) = self.cursor_screen_pos();
        fileeditor::render_hover_popup(&self.hover_lines, cursor_row, cursor_col, self.rect())
    }

    // Rewires `shell`'s own output paths (builtin sink + external
    // process stdout, see `output`/`ext_stdout_path`'s own doc comments)
    // into this controller's pane and resets both to empty -- called
    // once at the start of every `:run`, before shell.run_source_here.
    fn begin_capturing_output(&mut self, shell: &mut Shell) {
        self.output.borrow_mut().clear();
        self.ext_drain_offset = 0;
        shell.set_sink_capture(self.output.clone());
        if let Ok(file) = std::fs::File::create(&self.ext_stdout_path) {
            shell.set_stdout_capture_file(file);
        }
    }

    // Folds whatever new bytes a spawned external process has written to
    // ext_stdout_path since the last drain into `output` (builtin output
    // lands there directly via the sink and needs no draining) --
    // called at the top of on_statement, before deciding whether to
    // pause, so the pane is already current the moment a breakpoint
    // might stop here; and once more right after run_source_here returns,
    // to catch the very last statement's own output (on_statement only
    // ever runs *before* a statement, never after the final one).
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

    fn set_status(&mut self, s: String) {
        self.status = s;
    }

    fn toggle_breakpoint(&mut self, line: usize) {
        if !self.buf.breakpoints.insert(line) {
            self.buf.breakpoints.remove(&line);
        }
    }

    fn read_key(&self) -> Option<Key> {
        editor::read_key_idle(&mut || {}).ok().flatten()
    }

    // A small, dedicated colon-line reader -- not repl.rs's real command
    // mode (see this module's own top-of-file doc comment for why).
    // Enter submits, Escape/empty-Backspace cancels. No history, no
    // completion, no multi-line continuation -- deliberately simpler than
    // the real thing.
    fn read_colon_line(&self) -> Option<String> {
        let mut buf = String::new();
        loop {
            print!("\x1b[{};1H\x1b[K:{}", self.term_rows, buf);
            let _ = std::io::stdout().flush();
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

    // Identifier under the cursor's own column on its own line -- `K`'s
    // own hover target (docs::identifier_at, shared with repl.rs's real
    // file editor).
    fn identifier_at_cursor(&self) -> Option<String> {
        let (row, col) = self.buf.cursor();
        crate::docs::identifier_at(&self.buf.line_chars(row), col)
    }

    // Every debug action (run/break/continue/next/step/print/quit) is
    // reached exclusively through here now -- see this module's own
    // top-of-file doc comment on why the earlier bare single-key
    // shortcuts (b/r/c/n/s/q) were removed: several of them collided
    // with what those exact keys already mean as real vim
    // motions/operators (b = word-back, s = substitute, n = repeat-
    // search, c = the change operator, q = start a macro recording),
    // which made ordinary navigation subtly broken while debugging.
    // `K` (hover) stays a bare key -- not a debug-specific shortcut but
    // an extension of vim/neovim's own existing convention of `K`
    // showing info about whatever's under the cursor.
    fn dispatch_colon_command(&mut self, line: &str, depth: DebugDepth, shell: &Shell) -> Option<DebugAction> {
        let line = line.trim();
        match line {
            "c" | "continue" => {
                if !self.running {
                    self.set_status("bish: dbg: not running -- use :run to start".to_string());
                    return None;
                }
                self.paused_at = None;
                return Some(DebugAction::Continue);
            }
            "n" | "next" => {
                if !self.running {
                    self.set_status("bish: dbg: not running -- use :run to start".to_string());
                    return None;
                }
                self.pending_stop = PendingStop::AtOrBelow(depth);
                self.paused_at = None;
                return Some(DebugAction::StepOver);
            }
            "s" | "step" => {
                if !self.running {
                    self.set_status("bish: dbg: not running -- use :run to start".to_string());
                    return None;
                }
                self.pending_stop = PendingStop::Anywhere;
                self.paused_at = None;
                return Some(DebugAction::StepInto);
            }
            // Only ever reached here while `self.running` -- run()'s own
            // outer loop intercepts "r"/"run" itself (it's the one place
            // that actually has the script source/Shell to start a run
            // with), only falling through to dispatch_colon_command for
            // everything else, so a bare `:run` can only land here once
            // a run is already under way.
            "r" | "run" => {
                self.set_status("bish: dbg: already running".to_string());
                return None;
            }
            "q" | "quit" => {
                self.paused_at = None;
                self.quit_requested = true;
                return Some(DebugAction::Quit);
            }
            // Bare `break`/`b` (no line number) toggles at the cursor's
            // own current line -- the direct replacement for the old
            // bare `b` key.
            "b" | "break" => {
                let line = self.buf.cursor().0 + 1;
                self.toggle_breakpoint(line);
                self.set_status(format!("breakpoint toggled at line {}", line));
                return None;
            }
            _ => {}
        }
        if let Some(rest) = line.strip_prefix("break add ").or_else(|| line.strip_prefix("b add ")) {
            match rest.trim().parse::<usize>() {
                Ok(n) => {
                    self.buf.breakpoints.insert(n);
                    self.set_status(format!("breakpoint added at line {}", n));
                }
                Err(_) => self.set_status(format!("bish: dbg: {}: invalid line number", rest)),
            }
        } else if let Some(rest) = line.strip_prefix("break remove ").or_else(|| line.strip_prefix("b remove ")) {
            match rest.trim().parse::<usize>() {
                Ok(n) => {
                    self.buf.breakpoints.remove(&n);
                    self.set_status(format!("breakpoint removed at line {}", n));
                }
                Err(_) => self.set_status(format!("bish: dbg: {}: invalid line number", rest)),
            }
        } else if let Some(rest) = line.strip_prefix("break ").or_else(|| line.strip_prefix("b ")) {
            match rest.trim().parse::<usize>() {
                Ok(n) => {
                    self.toggle_breakpoint(n);
                    self.set_status(format!("breakpoint toggled at line {}", n));
                }
                Err(_) => self.set_status(format!("bish: dbg: {}: invalid line number", rest)),
            }
        } else if let Some(rest) = line.strip_prefix("print ").or_else(|| line.strip_prefix("p ")) {
            let name = rest.trim();
            match shell.debug_peek_var(name) {
                Some(v) => self.set_status(format!("{} = {}", name, v)),
                None => self.set_status(format!("{}: unset or not inspectable", name)),
            }
        } else if !line.is_empty() {
            self.set_status(format!("bish: dbg: unknown command: {}", line));
        }
        None
    }

    // Real vim motions/search/marks/jumps/Visual-mode-plus-yank, applied
    // directly against the real TextBuffer via the same buffer-generic
    // helpers repl.rs's own run_normal_mode_navigation uses for the
    // identical KeyOutcomes -- see this module's own top-of-file doc
    // comment for exactly which outcomes this covers and why every
    // mutating one is simply never matched at all.
    fn handle_navigation_key(&mut self, key: Key) {
        if self.vk.is_idle() && (self.vk.is_visual() || !self.buf.selections.is_empty()) {
            match key {
                Key::Char('Z') => {
                    self.commit_active_selection();
                    let end = self.buf.cursor();
                    self.vk.end_visual(end);
                    return;
                }
                Key::Char('y') => {
                    self.commit_active_selection();
                    let register = self.vk.take_pending_register();
                    let end = self.buf.cursor();
                    self.buf.yank_selections(&mut self.registers, register);
                    self.buf.selections.clear();
                    self.vk.end_visual(end);
                    return;
                }
                Key::Escape | Key::CtrlC => {
                    let end = self.buf.cursor();
                    self.vk.end_visual(end);
                    self.buf.selections.clear();
                    return;
                }
                _ => {}
            }
        }
        match self.vk.feed(key) {
            KeyOutcome::Motion(m, count) => {
                editor::apply_motion_or_reselect(&mut self.vk, &mut self.buf, m, count);
                let content_cols = fileeditor::editor_content_cols(&self.buf, self.rect());
                crate::repl::scroll_to_show_cursor(&mut self.buf, content_cols);
            }
            KeyOutcome::EnterVisual(shape) => {
                let cursor = self.buf.cursor();
                self.vk.begin_visual(shape, cursor);
            }
            KeyOutcome::ReselectVisual => {
                if let Some((shape, anchor, cursor)) = self.vk.last_visual() {
                    self.buf.set_cursor(cursor.0, cursor.1);
                    self.vk.begin_visual(shape, anchor);
                }
            }
            KeyOutcome::Jump { forward } => {
                let current = self.buf.cursor();
                let target = if forward { self.vk.jump_forward(current) } else { self.vk.jump_back(current) };
                if let Some((row, col)) = target {
                    let row = row.min(self.buf.line_count() - 1);
                    let col = col.min(self.buf.line_len(row));
                    self.buf.set_cursor(row, col);
                    let content_cols = fileeditor::editor_content_cols(&self.buf, self.rect());
                    crate::repl::scroll_to_show_cursor(&mut self.buf, content_cols);
                }
            }
            KeyOutcome::Operator(Op::Yank, motion, count, register) => {
                editor::yank_motion(&mut self.buf, &mut self.registers, motion, count, register);
            }
            KeyOutcome::OperatorLines(Op::Yank, count, register) => {
                editor::yank_lines(&self.buf, &mut self.registers, count, register);
            }
            // Every other outcome mutates (EnterInsert, a non-Yank
            // Operator/OperatorLines, Put, DeleteCharForward, Join,
            // surround, ReplaceChar, EnterReplace, ToggleCase,
            // AdjustNumber, OpenLine) or needs window/pane state this
            // standalone view doesn't have (Window) -- silently a no-op,
            // same "enforced by omission" convention NavBuffer::ReadOnly
            // already establishes.
            _ => {}
        }
    }

    fn commit_active_selection(&mut self) {
        if let Some(range) = crate::repl::active_visual_range(&self.vk, &self.buf) {
            self.buf.selections.push(range);
        }
    }
}

impl DebugHook for DebugController {
    // Blocks in place, same thread -- see this module's own top-of-file
    // doc comment. Reads keys/renders directly, exactly like run_
    // command_mode's own nested colon-line loop does for the same reason
    // (no event loop to hand control back to here -- this is called from
    // deep inside run_program, already borrowed by whoever called it).
    fn on_statement(&mut self, line: usize, depth: DebugDepth, shell: &Shell) -> DebugAction {
        self.drain_external_output();
        let should_pause = self.buf.breakpoints.contains(&line)
            || match self.pending_stop {
                PendingStop::None => false,
                PendingStop::Anywhere => true,
                PendingStop::AtOrBelow(d) => depth <= d,
            };
        if !should_pause {
            return DebugAction::Continue;
        }
        self.pending_stop = PendingStop::None;
        self.paused_at = Some(line);
        self.buf.set_cursor(line.saturating_sub(1), 0);
        let content_cols = fileeditor::editor_content_cols(&self.buf, self.rect());
        crate::repl::scroll_to_show_cursor(&mut self.buf, content_cols);
        self.set_status(format!("paused at line {} -- K: hover  :continue  :next  :step  :quit", line));

        loop {
            self.render();
            let key = match self.read_key() {
                Some(k) => k,
                None => {
                    self.quit_requested = true;
                    return DebugAction::Quit;
                }
            };
            // Any key other than K itself dismisses a showing hover
            // popup -- it's a transient tooltip, not a mode, so anything
            // else the user does just closes it and still takes effect
            // normally (see this module's own top-of-file doc comment).
            if !matches!(key, Key::Char('K')) {
                self.dismiss_hover();
            }
            if self.vk.is_idle() {
                match key {
                    Key::Char('K') => {
                        match self.identifier_at_cursor() {
                            Some(name) => self.show_hover(&name, shell),
                            None => self.hover_lines = vec!["no identifier under the cursor".to_string()],
                        }
                        continue;
                    }
                    Key::Char(':') => {
                        if let Some(cmd) = self.read_colon_line() {
                            if let Some(action) = self.dispatch_colon_command(&cmd, depth, shell) {
                                return action;
                            }
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            self.handle_navigation_key(key);
        }
    }
}

// `bish tool debug <script>` -- reads the file, then drives its own
// small event loop directly (no repl.rs session/window involved). Runs
// the whole script (a real re-run each time `:run` is invoked) rather
// than trying to resume execution from a stopped point -- v1's scope, no
// different from the fact this whole debugger is one-shot per process.
pub fn run(path: &str) -> i32 {
    let (rows, cols) = match crate::pty::get_size(0) {
        Ok(ws) if ws.rows > 0 && ws.cols > 0 => (ws.rows as usize, ws.cols as usize),
        _ => (24, 80),
    };
    let buf = match TextBuffer::open(std::path::Path::new(path), rows.saturating_sub(1).max(1)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("bish: {}: {}", path, e);
            return 1;
        }
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bish: {}: {}", path, e);
            return 1;
        }
    };
    let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else {
        eprintln!("bish: debug: not a terminal");
        return 1;
    };
    print!("\x1b[2J");

    let docs = DocIndex::build_from_source(&src, std::path::Path::new(path));
    let controller = std::rc::Rc::new(std::cell::RefCell::new(DebugController::new(buf, rows, cols, docs)));
    let mut shell = Shell::new();
    shell.set_script_args(path.to_string(), Vec::new());

    loop {
        controller.borrow().render();
        let key = match controller.borrow().read_key() {
            Some(k) => k,
            None => break,
        };
        // See on_statement's own identical dismissal -- a hover popup is
        // a transient tooltip, not a mode.
        if !matches!(key, Key::Char('K')) {
            controller.borrow_mut().dismiss_hover();
        }
        let is_idle = controller.borrow().vk.is_idle();
        // Escape is purely a "cancel whatever's pending" key (a pending
        // count/prefix, an active Visual selection) and never exits on
        // its own; a bare Escape with nothing pending is simply a no-op,
        // same as in the real editor, so it's not special-cased here at
        // all and just falls through to handle_navigation_key below like
        // any other key. `K` stays a bare key -- see dispatch_colon_
        // command's own doc comment on why it's the one exception to
        // "every debug action goes through `:`".
        if is_idle {
            match key {
                Key::Char('K') => {
                    let mut c = controller.borrow_mut();
                    match c.identifier_at_cursor() {
                        Some(name) => c.show_hover(&name, &shell),
                        None => c.hover_lines = vec!["no identifier under the cursor".to_string()],
                    }
                    continue;
                }
                Key::Char(':') => {
                    let cmd = controller.borrow().read_colon_line();
                    if let Some(cmd) = cmd {
                        let trimmed = cmd.trim();
                        // "r"/"run" is intercepted here rather than in
                        // dispatch_colon_command: this is the one place
                        // that actually has the script source/Shell
                        // needed to start a run at all -- everything
                        // else that command could mean (once a run is
                        // already under way) is handled there instead
                        // (see its own doc comment).
                        if trimmed == "r" || trimmed == "run" {
                            {
                                let mut c = controller.borrow_mut();
                                c.running = true;
                                c.begin_capturing_output(&mut shell);
                            }
                            shell.set_debug_hook(Some(controller.clone() as std::rc::Rc<std::cell::RefCell<dyn DebugHook>>));
                            shell.run_source_here(&src, path);
                            shell.set_debug_hook(None);
                            {
                                let mut c = controller.borrow_mut();
                                c.drain_external_output();
                                c.running = false;
                            }
                            if controller.borrow().quit_requested {
                                break;
                            }
                            controller.borrow_mut().set_status("run finished -- :run to run again  :quit to quit".to_string());
                        } else {
                            let depth = DebugDepth { subshell_depth: 0, call_depth: 0 };
                            if let Some(DebugAction::Quit) = controller.borrow_mut().dispatch_colon_command(trimmed, depth, &shell) {
                                break;
                            }
                        }
                    }
                    continue;
                }
                _ => {}
            }
        }
        controller.borrow_mut().handle_navigation_key(key);
    }
    let _ = std::fs::remove_file(&controller.borrow().ext_stdout_path);
    print!("\x1b[2J\x1b[H");
    let _ = std::io::stdout().flush();
    0
}
