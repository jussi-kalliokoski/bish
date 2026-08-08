use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Write};
use std::rc::Rc;

use crate::editor::{self, ReadOutcome};
use crate::exec::{self, ExecResult, Shell, WindowAction};
use crate::history::History;
use crate::lexer::Lexer;
use crate::parser::{AndOr, Command, Parser, Pipeline, Program};
use crate::prompt;
use crate::pty;
use crate::term;
use crate::vt100;

type SessionId = u32;

// One virtual shell session -- either the original process's own session
// (the root, id 0) or one created by `window new` (via Shell::
// new_virtual_child). Each has its own insert-mode multi-line
// continuation buffer, its own history_boundary (see History's doc
// comment), and its own VT100 grid: before promotion the grid sits empty
// and unused (the session's Shell writes straight to the real terminal);
// after promotion every session's output is captured into its own grid
// (see apply_window_action), so switching `window`s can redraw whatever
// that window last drew instead of showing stale/real-terminal content.
struct SessionState {
    shell: Shell,
    buffer: String,
    history_boundary: usize,
    screen: Rc<RefCell<vt100::Screen>>,
}

// One layer of a window's view stack. Currently only ever Session --
// Job frames (fg-ing a background job onto a window's own stack, poll-
// driven against the job's pty, instead of today's Shell-local
// drive_pending_fg single-slot) land in a later milestone. A stack
// rather than a single field is what lets `window fg <id>` push another
// window's current frame on top of this window's own -- vim-like "the
// same session shown in multiple windows" -- and what will give a
// fg'd job a real "ancestry" to unwind back through once Job frames
// exist.
#[derive(Clone, Copy, PartialEq)]
enum Frame {
    Session(SessionId),
}

// A window's view stack: the last (top) entry is what's currently
// focused/rendered there. Every window starts with exactly one frame
// (its own session) and stays that way until `window fg` pushes
// another. `window close` pops the top frame; the window itself is only
// actually removed once its stack empties (see apply_window_action),
// refused if it's the last window with nothing left to reveal
// underneath. Since the same SessionId can legally be the top of more
// than one window's stack at once (that's the whole point of `window
// fg`), a session is only ever dropped from `sessions` once no window's
// stack references it anywhere, at any depth -- see
// close_orphaned_sessions.
struct WindowEntry {
    id: u32,
    stack: Vec<Frame>,
}

impl WindowEntry {
    fn top_session(&self) -> SessionId {
        match self.stack.last().expect("a window's stack is never empty") {
            Frame::Session(id) => *id,
        }
    }
}

// Queries the real terminal's size via the same TIOCGWINSZ ioctl pty.rs
// uses for pty slaves -- it works identically on any tty fd, including
// our own controlling terminal (fd 0). Falls back to a conservative
// default if the query fails (e.g. stdin somehow isn't a tty by the time
// this runs, which main.rs already guards against before calling here).
fn query_term_size() -> (usize, usize) {
    match pty::get_size(0) {
        Ok(ws) if ws.rows > 0 && ws.cols > 0 => (ws.rows as usize, ws.cols as usize),
        _ => (24, 80),
    }
}

fn content_rows(term_rows: usize) -> usize {
    term_rows.saturating_sub(1).max(1)
}

// True if `sid` is reachable from some (window, stack-depth) pair other
// than the exact top frame of `windows[current_window]` -- i.e. whether
// that one reference is the *only* thing keeping the session alive. Used
// by the EOF handler to decide whether closing this one reference would
// actually orphan the session (and so should run its exit trap) or just
// drop one of several still-live references to it (see WindowEntry's
// doc comment on why the same session can appear more than once).
fn session_referenced_elsewhere(windows: &[WindowEntry], current_window: usize, sid: SessionId) -> bool {
    for (i, w) in windows.iter().enumerate() {
        for (depth, frame) in w.stack.iter().enumerate() {
            let matches = match frame {
                Frame::Session(s) => *s == sid,
            };
            if !matches {
                continue;
            }
            let is_the_one_reference_in_question = i == current_window && depth == w.stack.len() - 1;
            if !is_the_one_reference_in_question {
                return true;
            }
        }
    }
    false
}

pub fn run(shell: Shell) {
    // The shell itself must survive Ctrl-C (bash's own top-level
    // interactive behavior); a foreground child still dies/interrupts
    // normally since exec() resets a *caught* signal like this back to
    // default. See term::ignore_sigint's doc comment.
    term::ignore_sigint();
    exec::install_winch_handler();

    let mut history = History::load(".bish_history");
    let mut cmd_history = History::load(".bish_cmd_history");

    let (mut term_rows, mut term_cols) = query_term_size();

    let mut sessions: HashMap<SessionId, SessionState> = HashMap::new();
    let root_screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(term_rows), term_cols)));
    sessions.insert(0, SessionState { shell, buffer: String::new(), history_boundary: 0, screen: root_screen });
    let mut windows: Vec<WindowEntry> = vec![WindowEntry { id: 0, stack: vec![Frame::Session(0)] }];
    let mut current_window: usize = 0;
    let mut next_session_id: SessionId = 1;
    let mut next_window_id: u32 = 1;
    // Flips true (and stays true) the first time any window-family
    // command promotes the terminal -- see apply_window_action. Every
    // session's sink is Real until then, matching today's plain behavior
    // exactly when `:`/`window` are never invoked.
    let mut sinks_are_grid = false;

    loop {
        // Polled once per loop iteration rather than truly asynchronously:
        // editor::read_line below blocks on the next keypress with no
        // signal-aware select/poll of its own, so a resize that arrives
        // mid-block is only noticed once that read returns (the user's
        // next keystroke or Enter). A real event loop that can react to
        // WINCH *while* blocked on input is M9b's job (the same compositor
        // work that makes poll-driven `fg` possible) -- acknowledged,
        // temporary, same spirit as this codebase's other documented
        // scope boundaries.
        if exec::take_winch() {
            let (new_rows, new_cols) = query_term_size();
            if (new_rows, new_cols) != (term_rows, term_cols) {
                term_rows = new_rows;
                term_cols = new_cols;
                for s in sessions.values() {
                    s.screen.borrow_mut().resize(content_rows(term_rows), term_cols);
                }
                if sinks_are_grid {
                    compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                }
            }
        }

        let session_id = windows[current_window].top_session();
        let boundary = sessions[&session_id].history_boundary;
        let (prompt_str, armed_prompt_str) = {
            let session = &sessions[&session_id];
            if session.buffer.is_empty() {
                (prompt::render(&session.shell), prompt::render_command_armed(&session.shell))
            } else {
                (prompt::continuation(), prompt::continuation_armed())
            }
        };

        match editor::read_line(&prompt_str, &armed_prompt_str, &history, boundary, false, None) {
            Ok(ReadOutcome::Eof) => {
                // Whether closing *this* (window, top-frame) reference
                // would leave the session with no reference anywhere
                // (any window's stack, any depth) -- since `window fg`
                // lets the same session be the top of more than one
                // window's stack, or sit buried under another frame,
                // EOF closing just one of those references shouldn't
                // fire the exit trap for a session that's still alive
                // and reachable elsewhere.
                let will_orphan = !session_referenced_elsewhere(&windows, current_window, session_id);
                let session = sessions.get_mut(&session_id).unwrap();
                if !session.buffer.is_empty() {
                    session.shell.sink_err("bish: syntax error: unexpected end of input\n");
                }
                if will_orphan {
                    session.shell.run_exit_trap();
                }
                if windows.len() == 1 && windows[current_window].stack.len() == 1 {
                    // Last window, last frame anywhere: really exit.
                    // Restore the normal screen buffer if promotion ever
                    // switched us to the alternate one -- exiting every
                    // active session is currently the only way out of
                    // promoted mode.
                    if session.shell.is_promoted() {
                        // Direct, not session.shell.sink_out: this is a
                        // whole-terminal action (leaving the alternate
                        // screen buffer), the same category as
                        // compositor_redraw/apply_window_action's messages
                        // below -- always meant for the real screen, never
                        // a session's own captured output.
                        print!("\x1b[?1049l");
                        let _ = io::stdout().flush();
                    }
                    break;
                }
                // Otherwise: EOF on a window's top frame pops/closes it,
                // same as `window close` would.
                apply_window_action(
                    WindowAction::Close,
                    &mut sessions,
                    &mut windows,
                    &mut current_window,
                    &mut next_session_id,
                    &mut next_window_id,
                    &history,
                    &mut sinks_are_grid,
                    term_rows,
                    term_cols,
                );
            }
            Ok(ReadOutcome::Interrupted) => {
                // Ctrl-C abandons whatever multi-line construct was
                // pending, same as bash, and starts fresh at a new prompt.
                sessions.get_mut(&session_id).unwrap().buffer.clear();
            }
            Ok(ReadOutcome::CommandMode(pending)) => {
                let action = {
                    let session = sessions.get_mut(&session_id).unwrap();
                    run_command_mode(&mut session.shell, &mut cmd_history, pending)
                };
                if let Some(action) = action {
                    apply_window_action(
                        action,
                        &mut sessions,
                        &mut windows,
                        &mut current_window,
                        &mut next_session_id,
                        &mut next_window_id,
                        &history,
                        &mut sinks_are_grid,
                        term_rows,
                        term_cols,
                    );
                } else if sinks_are_grid {
                    // No window action, but command mode may still have
                    // run ordinary builtins whose output landed in this
                    // session's grid (e.g. `:pwd`) -- without this, that
                    // output would sit captured but never actually drawn.
                    compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                }
            }
            Ok(ReadOutcome::Line(line)) => {
                let mut window_action = None;
                let mut fg_pending = false;
                {
                    let session = sessions.get_mut(&session_id).unwrap();
                    if !session.buffer.is_empty() {
                        session.buffer.push('\n');
                    }
                    session.buffer.push_str(&line);

                    if session.buffer.trim().is_empty() {
                        session.buffer.clear();
                        continue;
                    }

                    match Lexer::new(&session.buffer).tokenize() {
                        Ok(toks) => match Parser::new(toks).parse_program() {
                            Ok(prog) => {
                                // Recorded regardless of the exit status
                                // the command ends up with -- bash and
                                // fish both record what was typed, not
                                // what succeeded.
                                history.record(&session.buffer);
                                let result = session.shell.run_program(&prog);
                                session.buffer.clear();
                                match result {
                                    ExecResult::Window(action) => window_action = Some(action),
                                    ExecResult::Fg => fg_pending = true,
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                if !is_incomplete(&e) {
                                    session.shell.sink_err(&format!("bish: syntax error: {}\n", e));
                                    session.buffer.clear();
                                }
                            }
                        },
                        Err(e) => {
                            if !is_incomplete(&e) {
                                session.shell.sink_err(&format!("bish: syntax error: {}\n", e));
                                session.buffer.clear();
                            }
                        }
                    }
                }
                if let Some(action) = window_action {
                    apply_window_action(
                        action,
                        &mut sessions,
                        &mut windows,
                        &mut current_window,
                        &mut next_session_id,
                        &mut next_window_id,
                        &history,
                        &mut sinks_are_grid,
                        term_rows,
                        term_cols,
                    );
                } else if fg_pending {
                    // A pty-attached background job was fg'd (see
                    // ExecResult::Fg's doc comment in exec.rs). The job
                    // and its pty live entirely inside the focused
                    // session's Shell (drive_pending_fg needs `&mut
                    // self` to pump it) -- but the redraw callback it
                    // calls needs `&sessions` for the tab bar, which
                    // can't be borrowed at the same time as `&mut
                    // session.shell` if `session` is a live borrow *of*
                    // `sessions`. Snapshotting what the callback needs
                    // (an Rc clone of the focused grid, a one-shot tab
                    // bar string -- neither changes mid-fg, since no
                    // other session can run while this one blocks
                    // pumping the job) sidesteps that entirely: the
                    // closure below holds no borrow of `sessions` at all.
                    let focused_screen = sessions[&session_id].screen.clone();
                    let tab_bar = tab_bar_line(&sessions, &windows, current_window);
                    let status = sessions.get_mut(&session_id).unwrap().shell.drive_pending_fg(|| {
                        render_compositor_frame(&focused_screen, &tab_bar, term_rows);
                    });
                    sessions.get_mut(&session_id).unwrap().shell.last_status = status;
                } else if sinks_are_grid {
                    // The common case once promoted: a normal command ran
                    // in the focused session and its output (if any)
                    // landed in that session's grid via its sink -- make
                    // it visible. Nothing streams live mid-command outside
                    // of a poll-driven `fg` (the branch above -- this is
                    // every *other* command); the redraw happens once the
                    // command has fully finished, same as every other
                    // redraw trigger here.
                    compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                }
            }
            Err(e) => {
                sessions.get(&session_id).unwrap().shell.sink_err(&format!("bish: error reading input: {}\n", e));
                break;
            }
        }
        let _ = io::stdout().flush();
    }
}

// Performs a `window`-family action against the real session/window
// state repl.rs owns directly (see ExecResult::Window's doc comment in
// exec.rs for why this can't live inside Shell itself). Redraws the
// compositor afterward for every action, including the rejected-close
// case (its error message needs to actually become visible once
// promoted, same as any other captured output).
#[allow(clippy::too_many_arguments)]
fn apply_window_action(
    action: WindowAction,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: &mut usize,
    next_session_id: &mut SessionId,
    next_window_id: &mut u32,
    history: &History,
    sinks_are_grid: &mut bool,
    term_rows: usize,
    term_cols: usize,
) {
    // One-time transition: every window-family command promotes before
    // repl.rs ever sees the resulting action (see run_window/
    // promote_if_needed in exec.rs), so by the time we get here the
    // terminal is always already in the alternate screen buffer. Flip
    // every existing session (in practice just session 0 the very first
    // time, since any session created via `new` afterward is always
    // created post-promotion and gets a Grid sink from birth below) from
    // writing straight to the real terminal to capturing into its own
    // grid instead.
    if !*sinks_are_grid {
        for s in sessions.values_mut() {
            let screen = s.screen.clone();
            s.shell.set_sink_grid(screen);
        }
        *sinks_are_grid = true;
    }

    match action {
        WindowAction::Next => {
            *current_window = (*current_window + 1) % windows.len();
        }
        WindowAction::Previous => {
            *current_window = (*current_window + windows.len() - 1) % windows.len();
        }
        WindowAction::New => {
            let parent_id = windows[*current_window].top_session();
            let mut child_shell = sessions[&parent_id].shell.new_virtual_child();
            let screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(term_rows), term_cols)));
            child_shell.set_sink_grid(screen.clone());
            let sid = *next_session_id;
            *next_session_id += 1;
            sessions.insert(sid, SessionState { shell: child_shell, buffer: String::new(), history_boundary: history.boundary(), screen });
            let wid = *next_window_id;
            *next_window_id += 1;
            windows.push(WindowEntry { id: wid, stack: vec![Frame::Session(sid)] });
            *current_window = windows.len() - 1;
        }
        WindowAction::Close => {
            if windows[*current_window].stack.len() > 1 {
                // Popping a frame off this window's own stack, not
                // closing the window -- always fine regardless of how
                // many windows exist, and never orphans a session since
                // what's revealed underneath was already a live frame.
                windows[*current_window].stack.pop();
            } else if windows.len() == 1 {
                let sid = windows[*current_window].top_session();
                sessions[&sid].shell.sink_err("bish: window close: cannot close the last window -- exit the shell instead\n");
                compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
                return;
            } else {
                windows.remove(*current_window);
                if *current_window >= windows.len() {
                    *current_window = windows.len() - 1;
                }
                close_orphaned_sessions(sessions, windows);
            }
        }
        WindowAction::FgSession(target_id) => {
            let target_frame = windows.iter().find(|w| w.id == target_id).map(|w| *w.stack.last().unwrap());
            match target_frame {
                Some(frame) => windows[*current_window].stack.push(frame),
                None => {
                    let cur_sid = windows[*current_window].top_session();
                    sessions[&cur_sid].shell.sink_err(&format!("bish: window: fg: no such window: {}\n", target_id));
                }
            }
        }
    }
    compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
}

// A session stops being referenced when the last window whose stack
// contained it (at any depth, not just the top -- `window fg` can leave
// it buried under another frame in a different window) closes. Called
// only from the "window itself is removed" branch of Close, above --
// no other action can ever orphan a session (New/FgSession only add
// references; Next/Previous don't touch the stack at all).
fn close_orphaned_sessions(sessions: &mut HashMap<SessionId, SessionState>, windows: &[WindowEntry]) {
    let referenced: std::collections::HashSet<SessionId> =
        windows.iter().flat_map(|w| w.stack.iter().map(|f| match f { Frame::Session(id) => *id })).collect();
    sessions.retain(|id, _| referenced.contains(id));
}

// Full redraw of the currently-focused window's grid plus the tab bar.
// Always a full clear+redraw rather than a cell/row-level diff: M9a's
// discrete-event redraws (a command finished, a window switch, a resize)
// are infrequent enough that flicker isn't a real concern, and a full
// redraw is trivially correct (self-healing against anything that wrote
// to the real terminal directly in between, like editor.rs's Ctrl-L).
// M9b's poll-driven `fg` (see drive_pending_fg below) is the first truly
// continuous redraw loop this codebase has -- still full-redraw for now,
// since a real cell/row diff is an optimization, not a correctness
// requirement, and this keeps both redraw paths sharing one
// implementation (render_compositor_frame) rather than risking them
// drifting apart.
fn compositor_redraw(sessions: &HashMap<SessionId, SessionState>, windows: &[WindowEntry], current_window: usize, term_rows: usize, _term_cols: usize) {
    let session_id = windows[current_window].top_session();
    let tab_bar = tab_bar_line(sessions, windows, current_window);
    render_compositor_frame(&sessions[&session_id].screen, &tab_bar, term_rows);
}

// The actual drawing: shared by compositor_redraw (reads the tab bar
// live from `sessions`) and drive_pending_fg's redraw callback (which
// can't hold a live borrow of `sessions` for its whole poll loop -- see
// that call site's comment -- so it passes a tab bar string snapshotted
// once, up front, instead).
fn render_compositor_frame(screen: &Rc<RefCell<vt100::Screen>>, tab_bar: &str, term_rows: usize) {
    let screen = screen.borrow();
    let (rows, cols) = screen.size();

    let mut out = String::new();
    out.push_str("\x1b[2J\x1b[H");
    for r in 0..rows {
        render_row(&mut out, &screen, r, cols);
        out.push_str("\r\n");
    }

    // Tab bar pinned to the terminal's real last row.
    out.push_str(&format!("\x1b[{};1H\x1b[K", term_rows));
    out.push_str(tab_bar);

    let (cur_row, cur_col) = screen.cursor();
    out.push_str(&format!("\x1b[{};{}H", cur_row + 1, cur_col + 1));
    out.push_str(if screen.cursor_visible { "\x1b[?25h" } else { "\x1b[?25l" });

    print!("{}", out);
    let _ = io::stdout().flush();
}

fn render_row(out: &mut String, screen: &vt100::Screen, row: usize, cols: usize) {
    let mut last: Option<(vt100::Color, vt100::Color, vt100::CellAttrs)> = None;
    for c in 0..cols {
        let cell = screen.cell(row, c);
        let key = (cell.fg, cell.bg, cell.attrs);
        if last != Some(key) {
            out.push_str(&sgr_codes(cell.fg, cell.bg, cell.attrs));
            last = Some(key);
        }
        out.push(cell.ch);
    }
    out.push_str("\x1b[0m");
}

// The inverse of vt100::Screen's own SGR parsing: turns a cell's resolved
// color/attrs back into the ANSI codes that reproduce them, so the real
// terminal ends up showing the same thing the grid recorded.
fn sgr_codes(fg: vt100::Color, bg: vt100::Color, attrs: vt100::CellAttrs) -> String {
    let mut codes: Vec<String> = vec!["0".to_string()];
    if attrs.bold {
        codes.push("1".to_string());
    }
    if attrs.dim {
        codes.push("2".to_string());
    }
    if attrs.italic {
        codes.push("3".to_string());
    }
    if attrs.underline {
        codes.push("4".to_string());
    }
    if attrs.reverse {
        codes.push("7".to_string());
    }
    if attrs.strikethrough {
        codes.push("9".to_string());
    }
    match fg {
        vt100::Color::Default => {}
        vt100::Color::Indexed(i) if i < 8 => codes.push(format!("{}", 30 + i)),
        vt100::Color::Indexed(i) if i < 16 => codes.push(format!("{}", 90 + (i - 8))),
        vt100::Color::Indexed(i) => codes.push(format!("38;5;{}", i)),
        vt100::Color::Rgb(r, g, b) => codes.push(format!("38;2;{};{};{}", r, g, b)),
    }
    match bg {
        vt100::Color::Default => {}
        vt100::Color::Indexed(i) if i < 8 => codes.push(format!("{}", 40 + i)),
        vt100::Color::Indexed(i) if i < 16 => codes.push(format!("{}", 100 + (i - 8))),
        vt100::Color::Indexed(i) => codes.push(format!("48;5;{}", i)),
        vt100::Color::Rgb(r, g, b) => codes.push(format!("48;2;{};{};{}", r, g, b)),
    }
    format!("\x1b[{}m", codes.join(";"))
}

// (window_id, is_the_current_window, that window's session's cwd) -- an
// owned snapshot so drive_pending_fg's redraw callback can build a tab
// bar without holding a live borrow of `sessions` for its whole poll
// loop (see that call site's comment for why it can't).
fn tab_bar_snapshot(sessions: &HashMap<SessionId, SessionState>, windows: &[WindowEntry], current_window: usize) -> Vec<(u32, bool, String)> {
    windows
        .iter()
        .enumerate()
        .map(|(i, w)| (w.id, i == current_window, sessions[&w.top_session()].shell.cwd.display().to_string()))
        .collect()
}

fn tab_bar_line(sessions: &HashMap<SessionId, SessionState>, windows: &[WindowEntry], current_window: usize) -> String {
    render_tab_bar(&tab_bar_snapshot(sessions, windows, current_window))
}

fn render_tab_bar(snapshot: &[(u32, bool, String)]) -> String {
    let mut line = String::new();
    for (id, current, cwd) in snapshot {
        if *current {
            line.push_str(&format!("\x1b[7m [{}] {} \x1b[0m ", id, cwd));
        } else {
            line.push_str(&format!(" [{}] {} ", id, cwd));
        }
    }
    line
}

// Distinguishes "needs more lines" (unterminated quote/paren, or the parser
// ran out of tokens expecting a closing keyword like `fi`/`done`) from a
// genuine syntax error, by checking for the exact phrasing this crate's own
// lexer/parser error messages use for those cases. Every parser error that
// stems from running out of tokens ends in "None" (`format!("{:?}", other)`
// on an `Option<Tok>` that was `None`), whether it came through the
// `expect()` helper ("expected KwDo, got None") or parse_list_until's own
// message ("...expected one of [...]" -- ends with the debug-printed stop
// list, not "None", hence the separate check).
fn is_incomplete(err: &str) -> bool {
    err.starts_with("unterminated") || err.contains("unexpected end of input") || err.ends_with("None")
}

// Command mode: entered via ':' at an empty insert-mode prompt (see
// editor.rs's ReadOutcome::CommandMode). Has its own history, separate
// from the shell's, and only ever runs builtins directly -- `command NAME`
// is the escape hatch for externals (see restrict_to_builtins in exec.rs).
// `initial_char` is the character (if any) that committed entry into
// command mode -- e.g. typing ":w new" commits on 'w', which must be
// command mode's own first typed character, not lost (see
// editor::ReadOutcome::CommandMode's doc comment).
// Renders its own prompt via prompt::render_command_armed -- the exact
// same "user@host:path" prefix the normal prompt uses, just with the
// ':' terminator -- rather than some visually distinct "you are now in
// a special mode" prompt: switching into command mode should read as
// nothing more than that one terminator character changing, seamlessly
// continuing the same line editor.rs was already drawing (see
// read_line's arming-commit branch, which deliberately never prints a
// newline on the way in).
// One-shot, matching vim's ':' Ex command line: successfully running one
// command drops straight back to the normal shell prompt rather than
// looping for another (see the `_ => return None` below). An empty line,
// Ctrl-C, Ctrl-D, or Esc (regardless of what's been typed -- see
// read_line's esc_cancels parameter) all cancel out the same way, back to
// insert mode with nothing run. Returns a WindowAction if the command
// that ran was a `window`-family one, for the caller to apply against the
// real session/window state (which run_command_mode itself has no access
// to).
fn run_command_mode(shell: &mut Shell, history: &mut History, initial_char: Option<char>) -> Option<WindowAction> {
    let mut buffer = String::new();
    let mut prefill = initial_char;
    loop {
        let prompt_str = if buffer.is_empty() { prompt::render_command_armed(shell) } else { prompt::continuation() };

        // Already fully inside command mode, so there's nothing
        // meaningful to swap to if the ':'-arming mechanic re-triggers
        // here (see editor::read_line's doc comment) -- same string for
        // both, making it visually a no-op. esc_cancels: true -- like a
        // vim ':' command line, Esc should back out of command mode the
        // same as Ctrl-C, regardless of what's been typed. prefill.take():
        // only the very first iteration (if entry carried a character)
        // seeds the buffer; every iteration after that starts empty.
        match editor::read_line(&prompt_str, &prompt_str, history, 0, true, prefill.take()) {
            Ok(ReadOutcome::Eof) | Ok(ReadOutcome::Interrupted) => return None,
            // ':' at an empty command-mode prompt too -- nothing
            // meaningful to switch to since we're already here; just
            // carry forward whatever character (if any) committed it as
            // the next iteration's prefill, same as the outer entry.
            Ok(ReadOutcome::CommandMode(pending)) => {
                prefill = pending;
            }
            Ok(ReadOutcome::Line(line)) => {
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(&line);

                if buffer.trim().is_empty() {
                    return None;
                }

                match Lexer::new(&buffer).tokenize() {
                    Ok(toks) => match Parser::new(toks).parse_program() {
                        Ok(prog) => {
                            if let Some(msg) = command_mode_violation(&prog) {
                                shell.sink_err(&format!("bish: {}\n", msg));
                                buffer.clear();
                            } else {
                                history.record(&buffer);
                                shell.restrict_to_builtins = true;
                                let result = shell.run_program(&prog);
                                shell.restrict_to_builtins = false;
                                buffer.clear();
                                // One-shot: command mode exists to run a
                                // single command, then drop straight back
                                // to the normal shell prompt (matching
                                // vim's ':' Ex command line) -- every
                                // successful-execution path below returns
                                // rather than looping for another command.
                                // A rejected/errored attempt (the
                                // command_mode_violation and syntax-error
                                // branches, above and below this one)
                                // deliberately does NOT return, so a typo
                                // can be retried without re-entering
                                // command mode from scratch.
                                match result {
                                    ExecResult::Window(action) => return Some(action),
                                    // `fg`'s poll loop needs repl.rs's own
                                    // compositor state, which this nested
                                    // read-eval loop has no access to (see
                                    // Shell::discard_pending_fg's doc
                                    // comment) -- reject it here instead
                                    // of silently leaving the job stashed
                                    // and never driven.
                                    ExecResult::Fg => {
                                        shell.sink_err("bish: fg: not supported in command mode -- use it from the normal shell prompt\n");
                                        shell.discard_pending_fg();
                                        return None;
                                    }
                                    _ => return None,
                                }
                            }
                        }
                        Err(e) => {
                            if !is_incomplete(&e) {
                                shell.sink_err(&format!("bish: syntax error: {}\n", e));
                                buffer.clear();
                            }
                        }
                    },
                    Err(e) => {
                        if !is_incomplete(&e) {
                            shell.sink_err(&format!("bish: syntax error: {}\n", e));
                            buffer.clear();
                        }
                    }
                }
            }
            Err(e) => {
                shell.sink_err(&format!("bish: error reading input: {}\n", e));
                return None;
            }
        }
        let _ = io::stdout().flush();
    }
}

// Command mode allows full control-flow syntax (if/while/for/etc -- every
// leaf command still funnels through the same restrict_to_builtins gate
// regardless of nesting) but not `(...)` subshells, coproc, function
// definitions, or multi-stage `|` pipelines: the first three self-exec (or
// register something persistent) rather than running through that gate,
// and would bypass the restriction entirely; function definitions would
// leak a callable function out into normal shell mode later. Walked
// recursively since these can be nested inside a control-flow body.
fn command_mode_violation(prog: &Program) -> Option<&'static str> {
    prog.iter().find_map(|item| and_or_violation(&item.and_or))
}

fn and_or_violation(ao: &AndOr) -> Option<&'static str> {
    pipeline_violation(&ao.first).or_else(|| ao.rest.iter().find_map(|(_, p)| pipeline_violation(p)))
}

fn pipeline_violation(p: &Pipeline) -> Option<&'static str> {
    if p.commands.len() > 1 {
        return Some("multi-stage pipelines ('|') aren't allowed in command mode");
    }
    p.commands.iter().find_map(command_violation)
}

fn command_violation(c: &Command) -> Option<&'static str> {
    match c {
        Command::Subshell(..) => Some("subshells ('(...)') aren't allowed in command mode"),
        Command::Coproc { .. } => Some("coproc isn't allowed in command mode"),
        Command::FuncDef { .. } => Some("function definitions aren't allowed in command mode"),
        Command::Simple(_) | Command::Arith(..) | Command::Test(..) => None,
        Command::If { branches, else_branch, .. } => branches
            .iter()
            .find_map(|(cond, body)| command_mode_violation(cond).or_else(|| command_mode_violation(body)))
            .or_else(|| else_branch.as_ref().and_then(|b| command_mode_violation(b))),
        Command::While { cond, body, .. } => command_mode_violation(cond).or_else(|| command_mode_violation(body)),
        Command::For { body, .. } | Command::Select { body, .. } | Command::CFor { body, .. } => command_mode_violation(body),
        Command::Case { arms, .. } => arms.iter().find_map(|(_, body, _)| command_mode_violation(body)),
        Command::Group(body, _) => command_mode_violation(body),
    }
}
