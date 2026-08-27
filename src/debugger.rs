// `bish tool debug <script>` -- a small, standalone interactive debugger.
// Deliberately NOT built on repl.rs's session/window/pane/compositor
// machinery: that infrastructure exists to multiplex several concurrent
// shells sharing one terminal, which this single-purpose view has no
// need for (see plan.md's own note on this). It prints straight to the
// real terminal with plain ANSI sequences, the same "no vt100 grid model
// involved" style fileeditor.rs's own render_editor_frame uses before a
// session is promoted into the windowed compositor.
//
// The debug/dbg command-mode builtin the user asked for is this view's
// own dedicated `:` colon-line reader (read_colon_line/dispatch_colon_
// command below) -- not a call into repl.rs's real run_command_mode,
// which needs live `&mut sessions`/`&mut windows` access unavailable
// from mid-run_program (see DebugController::on_statement's own doc
// comment for exactly why).

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::Write;
use std::rc::Rc;

use crate::editor::{self, Key};
use crate::exec::{DebugAction, DebugDepth, DebugHook, Shell};
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
    lines: Vec<String>,
    breakpoints: BTreeSet<usize>,
    // 1-based -- the line the read-only view's own navigation cursor
    // sits on, independent of whatever line execution is actually
    // paused at (paused_at).
    cursor_line: usize,
    viewport_top: usize,
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

const GUTTER_WIDTH: usize = 4; // breakpoint marker (2) + a thin separator (2)

impl DebugController {
    fn new(src: &str, term_rows: usize, term_cols: usize) -> Self {
        let lines: Vec<String> = src.lines().map(|s| s.to_string()).collect();
        DebugController {
            lines,
            breakpoints: BTreeSet::new(),
            cursor_line: 1,
            viewport_top: 0,
            paused_at: None,
            pending_stop: PendingStop::None,
            quit_requested: false,
            term_rows,
            term_cols,
            status: "r/Enter: run  b: toggle breakpoint  j/k: move  :: command  q: quit".to_string(),
        }
    }

    fn content_rows(&self) -> usize {
        self.term_rows.saturating_sub(1).max(1)
    }

    fn line_number_width(&self) -> usize {
        self.lines.len().max(1).to_string().len()
    }

    fn scroll_to_show(&mut self, line: usize) {
        let rows = self.content_rows();
        let idx = line.saturating_sub(1);
        if idx < self.viewport_top {
            self.viewport_top = idx;
        } else if idx >= self.viewport_top + rows {
            self.viewport_top = idx + 1 - rows;
        }
    }

    fn render(&self, shell: Option<&Shell>) {
        let rows = self.content_rows();
        let num_width = self.line_number_width();
        let mut out = String::new();
        out.push_str("\x1b[H");
        for r in 0..rows {
            let idx = self.viewport_top + r;
            out.push_str(&format!("\x1b[{};1H\x1b[K", r + 1));
            if idx >= self.lines.len() {
                continue;
            }
            let line_no = idx + 1;
            let bp_marker = if self.breakpoints.contains(&line_no) { "\x1b[1;31m*\x1b[0m " } else { "  " };
            let is_paused = self.paused_at == Some(line_no);
            let is_cursor = self.cursor_line == line_no && self.paused_at.is_none();
            let text: String = self.lines[idx].chars().take(self.term_cols.saturating_sub(GUTTER_WIDTH + num_width + 1)).collect();
            let num = format!("{:>width$}", line_no, width = num_width);
            if is_paused {
                out.push_str(&format!("{}\x1b[2m{}\x1b[0m \x1b[7m{}\x1b[0m", bp_marker, num, text));
            } else if is_cursor {
                out.push_str(&format!("{}\x1b[2m{}\x1b[0m \x1b[4m{}\x1b[0m", bp_marker, num, text));
            } else {
                out.push_str(&format!("{}\x1b[2m{}\x1b[0m {}", bp_marker, num, text));
            }
        }
        out.push_str(&format!("\x1b[{};1H\x1b[K\x1b[7m{}\x1b[0m", self.term_rows, truncate(&self.status, self.term_cols)));
        let _ = shell; // reserved for a future richer status (call stack, etc.)
        print!("{}", out);
        let _ = std::io::stdout().flush();
    }

    fn set_status(&mut self, s: String) {
        self.status = s;
    }

    fn toggle_breakpoint(&mut self, line: usize) {
        if !self.breakpoints.insert(line) {
            self.breakpoints.remove(&line);
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
            let mut row = format!("\x1b[{};1H\x1b[K:{}", self.term_rows, buf);
            print!("{}", row);
            let _ = std::io::stdout().flush();
            row.clear();
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

    // Identifier under (or starting at) the cursor's own line -- the
    // debug view has no per-column cursor of its own (only a current
    // *line*), so this looks for the first identifier on that line
    // rather than a true under-the-mouse-cursor lookup; a natural follow-
    // up once this view gains real column-level navigation.
    fn first_identifier_on_line(&self, line: usize) -> Option<String> {
        let text = self.lines.get(line.checked_sub(1)?)?;
        let mut chars = text.chars().peekable();
        loop {
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphabetic() || c == '_' {
                    break;
                }
                chars.next();
            }
            if chars.peek().is_none() {
                return None;
            }
            let mut ident = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    ident.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            if !ident.is_empty() {
                return Some(ident);
            }
        }
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
                    self.breakpoints.insert(n);
                    self.set_status(format!("breakpoint added at line {}", n));
                }
                Err(_) => self.set_status(format!("bish: dbg: {}: invalid line number", rest)),
            }
        } else if let Some(rest) = line.strip_prefix("break remove ").or_else(|| line.strip_prefix("b remove ")) {
            match rest.trim().parse::<usize>() {
                Ok(n) => {
                    self.breakpoints.remove(&n);
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
}

fn truncate(s: &str, cols: usize) -> String {
    s.chars().take(cols).collect()
}

impl DebugHook for DebugController {
    // Blocks in place, same thread -- see this module's own top-of-file
    // doc comment. Reads keys/renders directly, exactly like run_
    // command_mode's own nested colon-line loop does for the same reason
    // (no event loop to hand control back to here -- this is called from
    // deep inside run_program, already borrowed by whoever called it).
    fn on_statement(&mut self, line: usize, depth: DebugDepth, shell: &Shell) -> DebugAction {
        let should_pause = self.breakpoints.contains(&line)
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
        self.cursor_line = line;
        self.scroll_to_show(line);
        self.set_status(format!("paused at line {} -- c: continue  n: next  s: step  K: hover  ::  q: quit", line));

        loop {
            self.render(Some(shell));
            let key = match self.read_key() {
                Some(k) => k,
                None => {
                    self.quit_requested = true;
                    return DebugAction::Quit;
                }
            };
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
                Key::Char('q') | Key::Escape => {
                    self.paused_at = None;
                    self.quit_requested = true;
                    return DebugAction::Quit;
                }
                Key::Char('j') | Key::Down => {
                    self.cursor_line = (self.cursor_line + 1).min(self.lines.len().max(1));
                    self.scroll_to_show(self.cursor_line);
                }
                Key::Char('k') | Key::Up => {
                    self.cursor_line = self.cursor_line.saturating_sub(1).max(1);
                    self.scroll_to_show(self.cursor_line);
                }
                Key::Char('b') => self.toggle_breakpoint(self.cursor_line),
                Key::Char('K') => {
                    let msg = match self.first_identifier_on_line(self.cursor_line).and_then(|name| shell.debug_peek_var(&name).map(|v| (name, v))) {
                        Some((name, v)) => format!("{} = {}", name, v),
                        None => "no inspectable variable on this line".to_string(),
                    };
                    self.set_status(msg);
                }
                Key::Char(':') => {
                    if let Some(cmd) = self.read_colon_line() {
                        if let Some(action) = self.dispatch_colon_command(&cmd, depth, shell) {
                            return action;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

// `bish tool debug <script>` -- reads the file, then drives its own
// small event loop directly (no repl.rs session/window involved). Runs
// the whole script (a real re-run each time `r`/Enter is pressed) rather
// than trying to resume execution from a stopped point -- v1's scope, no
// different from the fact this whole debugger is one-shot per process.
pub fn run(path: &str) -> i32 {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bish: {}: {}", path, e);
            return 1;
        }
    };
    let (rows, cols) = match crate::pty::get_size(0) {
        Ok(ws) if ws.rows > 0 && ws.cols > 0 => (ws.rows as usize, ws.cols as usize),
        _ => (24, 80),
    };
    let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else {
        eprintln!("bish: debug: not a terminal");
        return 1;
    };
    print!("\x1b[2J");

    let controller = Rc::new(RefCell::new(DebugController::new(&src, rows, cols)));
    let mut shell = Shell::new();
    shell.set_script_args(path.to_string(), Vec::new());

    let status = loop {
        controller.borrow().render(None);
        let key = match controller.borrow().read_key() {
            Some(k) => k,
            None => break 0,
        };
        match key {
            Key::Char('q') | Key::Escape => break 0,
            Key::Char('r') | Key::Enter => {
                shell.set_debug_hook(Some(controller.clone() as Rc<RefCell<dyn DebugHook>>));
                shell.run_source_here(&src, path);
                shell.set_debug_hook(None);
                if controller.borrow().quit_requested {
                    break 0;
                }
                controller.borrow_mut().set_status("run finished -- r/Enter: run again  q: quit".to_string());
            }
            Key::Char('j') | Key::Down => {
                let mut c = controller.borrow_mut();
                let n = c.lines.len().max(1);
                c.cursor_line = (c.cursor_line + 1).min(n);
                let line = c.cursor_line;
                c.scroll_to_show(line);
            }
            Key::Char('k') | Key::Up => {
                let mut c = controller.borrow_mut();
                c.cursor_line = c.cursor_line.saturating_sub(1).max(1);
                let line = c.cursor_line;
                c.scroll_to_show(line);
            }
            Key::Char('b') => {
                let mut c = controller.borrow_mut();
                let line = c.cursor_line;
                c.toggle_breakpoint(line);
            }
            Key::Char(':') => {
                let cmd = controller.borrow().read_colon_line();
                if let Some(cmd) = cmd {
                    let depth = DebugDepth { subshell_depth: 0, call_depth: 0 };
                    let mut c = controller.borrow_mut();
                    c.dispatch_colon_command(&cmd, depth, &shell);
                }
            }
            _ => {}
        }
    };
    print!("\x1b[2J\x1b[H");
    let _ = std::io::stdout().flush();
    status
}
