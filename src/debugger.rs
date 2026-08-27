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
// comment for exactly why).

use std::io::Write;

use crate::bishedit::registers::Registers;
use crate::bishedit::textbuffer::TextBuffer;
use crate::bishedit::vimkeys::{KeyOutcome, Op, VimKeys};
use crate::bishedit::Buffer as _;
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
    // Set by `q`/Escape/a failed key read while paused -- checked by
    // run() after run_program returns, to distinguish "the script simply
    // finished" from "the user asked to abandon this run" (both unwind
    // through the same ExecResult::Exit path from run_program's own
    // perspective, so this is the only way run() can tell them apart).
    quit_requested: bool,
    term_rows: usize,
    term_cols: usize,
    status: String,
}

impl DebugController {
    fn new(buf: TextBuffer, term_rows: usize, term_cols: usize) -> Self {
        DebugController {
            buf,
            vk: VimKeys::new(),
            registers: Registers::new(),
            paused_at: None,
            pending_stop: PendingStop::None,
            quit_requested: false,
            term_rows,
            term_cols,
            status: "r/Enter: run  b: toggle breakpoint  hjkl/w/b/gg/G/...: navigate  K: hover  ::  q: quit".to_string(),
        }
    }

    // One row reserved at the bottom for the status line/colon-line
    // (shared, exactly like the real editor's own status row -- there's
    // no tab bar here to also reserve a second row for).
    fn rect(&self) -> Rect {
        Rect { row: 0, col: 0, rows: self.term_rows.saturating_sub(1).max(1), cols: self.term_cols }
    }

    fn render(&self) {
        let rect = self.rect();
        let mut out = crate::repl::render_global_status_row(&self.status, self.term_rows);
        out.push_str(&fileeditor::build_editor_frame(&self.buf, &self.vk, EditorMode::Normal, rect, rect.row, rect.col, None));
        print!("{}", out);
        let _ = std::io::stdout().flush();
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
    // own hover target.
    fn identifier_at_cursor(&self) -> Option<String> {
        let (row, col) = self.buf.cursor();
        let chars = self.buf.line_chars(row);
        let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
        if col >= chars.len() || !is_ident(chars[col]) {
            return None;
        }
        let start = (0..=col).rev().take_while(|&i| is_ident(chars[i])).last().unwrap_or(col);
        let end = (col..chars.len()).take_while(|&i| is_ident(chars[i])).count() + col;
        Some(chars[start..end].iter().collect())
    }

    fn dispatch_colon_command(&mut self, line: &str, depth: DebugDepth, shell: &Shell) -> Option<DebugAction> {
        let line = line.trim();
        match line {
            "c" | "continue" => {
                self.paused_at = None;
                return Some(DebugAction::Continue);
            }
            "n" | "next" => {
                self.pending_stop = PendingStop::AtOrBelow(depth);
                self.paused_at = None;
                return Some(DebugAction::StepOver);
            }
            "s" | "step" => {
                self.pending_stop = PendingStop::Anywhere;
                self.paused_at = None;
                return Some(DebugAction::StepInto);
            }
            "q" | "quit" => {
                self.paused_at = None;
                self.quit_requested = true;
                return Some(DebugAction::Quit);
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
        self.set_status(format!("paused at line {} -- c: continue  n: next  s: step  K: hover  ::  q: quit", line));

        loop {
            self.render();
            let key = match self.read_key() {
                Some(k) => k,
                None => {
                    self.quit_requested = true;
                    return DebugAction::Quit;
                }
            };
            if self.vk.is_idle() {
                match key {
                    Key::Char('c') => {
                        self.paused_at = None;
                        return DebugAction::Continue;
                    }
                    Key::Char('n') => {
                        self.pending_stop = PendingStop::AtOrBelow(depth);
                        self.paused_at = None;
                        return DebugAction::StepOver;
                    }
                    Key::Char('s') => {
                        self.pending_stop = PendingStop::Anywhere;
                        self.paused_at = None;
                        return DebugAction::StepInto;
                    }
                    Key::Char('q') => {
                        self.paused_at = None;
                        self.quit_requested = true;
                        return DebugAction::Quit;
                    }
                    Key::Char('b') => {
                        let line = self.buf.cursor().0 + 1;
                        self.toggle_breakpoint(line);
                        continue;
                    }
                    Key::Char('K') => {
                        let msg = match self.identifier_at_cursor().and_then(|name| shell.debug_peek_var(&name).map(|v| (name, v))) {
                            Some((name, v)) => format!("{} = {}", name, v),
                            None => "no inspectable variable under the cursor".to_string(),
                        };
                        self.set_status(msg);
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
// the whole script (a real re-run each time `r`/Enter is pressed) rather
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

    let controller = std::rc::Rc::new(std::cell::RefCell::new(DebugController::new(buf, rows, cols)));
    let mut shell = Shell::new();
    shell.set_script_args(path.to_string(), Vec::new());

    loop {
        controller.borrow().render();
        let key = match controller.borrow().read_key() {
            Some(k) => k,
            None => break,
        };
        let is_idle = controller.borrow().vk.is_idle();
        // Only `q` quits -- matching real vim, Escape is purely a "cancel
        // whatever's pending" key (a pending count/prefix, an active
        // Visual selection) and never exits on its own; a bare Escape
        // with nothing pending is simply a no-op, same as in the real
        // editor, so it's not special-cased here at all and just falls
        // through to handle_navigation_key below like any other key.
        if is_idle {
            match key {
                Key::Char('q') => break,
                Key::Char('r') | Key::Enter => {
                    shell.set_debug_hook(Some(controller.clone() as std::rc::Rc<std::cell::RefCell<dyn DebugHook>>));
                    shell.run_source_here(&src, path);
                    shell.set_debug_hook(None);
                    if controller.borrow().quit_requested {
                        break;
                    }
                    controller.borrow_mut().set_status("run finished -- r/Enter: run again  q: quit".to_string());
                    continue;
                }
                Key::Char('b') => {
                    let mut c = controller.borrow_mut();
                    let line = c.buf.cursor().0 + 1;
                    c.toggle_breakpoint(line);
                    continue;
                }
                Key::Char('K') => {
                    let mut c = controller.borrow_mut();
                    let msg = match c.identifier_at_cursor() {
                        Some(name) => format!("{}: not running -- start with r/Enter to inspect live values", name),
                        None => "no identifier under the cursor".to_string(),
                    };
                    c.set_status(msg);
                    continue;
                }
                Key::Char(':') => {
                    let cmd = controller.borrow().read_colon_line();
                    if let Some(cmd) = cmd {
                        let depth = DebugDepth { subshell_depth: 0, call_depth: 0 };
                        controller.borrow_mut().dispatch_colon_command(&cmd, depth, &shell);
                    }
                    continue;
                }
                _ => {}
            }
        }
        controller.borrow_mut().handle_navigation_key(key);
    }
    print!("\x1b[2J\x1b[H");
    let _ = std::io::stdout().flush();
    0
}
