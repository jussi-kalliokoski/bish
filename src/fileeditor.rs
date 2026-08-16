// `e [file]`: a builtin modal text editor built entirely on bishedit's
// existing vim vocabulary (motion.rs/vimkeys.rs/registers.rs) plus the
// new mutable multi-line buffer (bishedit::textbuffer::TextBuffer). A
// top-level module, the same tier as editor.rs/repl.rs, not part of
// bishedit itself -- this is a concrete interactive consumer, not
// reusable headless logic.
//
// Reached via `exec::ExecResult::Edit`, exactly the way `window`
// (`ExecResult::Window`) and `fg` (`ExecResult::Fg`) already have to
// bubble up to repl.rs: a builtin has no raw-mode/keystroke/rendering
// access of its own, and `Registers` lives in `repl::run`'s own locals,
// not in `Shell` at all. repl.rs owns detach/resume (`Frame::Edit`,
// mirroring `Frame::Job`) -- this module only drives one already-open
// session for as long as it's focused.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

use crate::bishedit::motion;
use crate::bishedit::registers::{RegisterShape, RegisterValue, Registers};
use crate::bishedit::textbuffer::TextBuffer;
use crate::bishedit::vimkeys::{InsertCmd, KeyOutcome, Op, SurroundTarget, VimKeys};
use crate::bishedit::Buffer;
use crate::editor::{self, Key};
use crate::repl::Rect;
use crate::term;
use crate::vt100;

// What `repl.rs`'s `edit_frames` side table actually holds -- not just a
// bare `TextBuffer`, so a mid-typed count/prefix or an in-progress
// Visual selection survives a detach, matching how a real `Frame::Job`'s
// own live state already does the same across Ctrl+Space.
pub struct EditSession {
    pub buffer: TextBuffer,
    pub vk: VimKeys,
}

impl EditSession {
    // `path`: `None` opens a fresh unnamed buffer; `Some` opens (or, for
    // a nonexistent path, prepares to create -- see `TextBuffer::open`'s
    // own doc comment) that file.
    pub fn open(path: Option<&str>, vheight: usize) -> io::Result<EditSession> {
        let buffer = match path {
            Some(p) => TextBuffer::open(std::path::Path::new(p), vheight)?,
            None => TextBuffer::new_unnamed(vheight),
        };
        Ok(EditSession { buffer, vk: VimKeys::new() })
    }
}

// Mirrors repl.rs's own (private) `FgOutcome`, but only two cases: no
// `Stopped`, since there's no external process here to suspend.
pub enum EditOutcome {
    Quit,
    Detached,
}

// The actual blocking loop -- called by repl.rs's `run_edit_frame`
// whenever this pane's top frame is a `Frame::Edit`, whether that's a
// freshly-opened session or one resumed after an earlier detach. Renders
// directly (no callback indirection the way `drive_fg_job` uses one --
// that one needs it because a job's own output arrives independently of
// user input; this loop only ever changes in response to a key it just
// read, so it can just render itself after each one). `on_idle` is
// passed straight to `editor::read_key_idle`, same as every other
// blocking key-reading loop in this codebase, so other windows'
// background jobs keep progressing while this one owns the terminal.
pub fn drive(session: &mut EditSession, rect: Rect, registers: &mut Registers, on_idle: &mut dyn FnMut()) -> io::Result<EditOutcome> {
    // Without this, stdin stays in cooked/canonical mode (line-buffered,
    // locally echoed by the tty driver itself) -- every other blocking
    // key-reading loop in this codebase (run_normal_mode_navigation,
    // run_line_normal_mode, ...) enables raw mode for exactly this
    // reason, right before its own loop starts. Held for this whole call
    // (covering run_insert_mode/run_ex_command too, both nested within
    // it), not just re-acquired per sub-loop.
    let _guard = term::RawGuard::enable(0)?;
    render_editor_frame(&session.buffer, &session.vk, false, rect);
    loop {
        let key = match editor::read_key_idle(on_idle)? {
            Some(k) => k,
            // EOF (stdin closed): nothing sensible to resume into --
            // same "just stop" treatment repl.rs's own read_line-driven
            // loops give an EOF they can't otherwise act on.
            None => return Ok(EditOutcome::Quit),
        };

        if key == Key::CtrlSpace && session.vk.is_idle() {
            return Ok(EditOutcome::Detached);
        }

        // Visual mode's own `Z`/`y`/`d`/`c`/`p`/`P`/Escape -- intercepted
        // here, ahead of `vk.feed`, for the same reason editor.rs's own
        // identical arms (`run_line_normal_mode`) already are: "is there
        // a selection to act on" is buffer-owned state vimkeys.rs
        // deliberately never sees. `vk.is_idle()` guards all of them:
        // mid a sub-prefix (`f`, a count, ...) these keys keep their
        // ordinary meaning instead.
        match key {
            Key::Char('Z') if session.vk.is_idle() && session.vk.is_visual() => {
                commit_active_selection(session);
                let end_cursor = session.buffer.cursor();
                session.vk.end_visual(end_cursor);
                render_editor_frame(&session.buffer, &session.vk, false, rect);
                continue;
            }
            Key::Char('y') if session.vk.is_idle() && (session.vk.is_visual() || !session.buffer.selections.is_empty()) => {
                commit_active_selection(session);
                let end_cursor = session.buffer.cursor();
                let register = session.vk.take_pending_register();
                session.buffer.yank_selections(registers, register);
                session.buffer.selections.clear();
                session.vk.end_visual(end_cursor);
                render_editor_frame(&session.buffer, &session.vk, false, rect);
                continue;
            }
            Key::Char('d') if session.vk.is_idle() && (session.vk.is_visual() || !session.buffer.selections.is_empty()) => {
                commit_active_selection(session);
                let end_cursor = session.buffer.cursor();
                let register = session.vk.take_pending_register();
                session.buffer.delete_selections(registers, register);
                session.buffer.selections.clear();
                session.vk.end_visual(end_cursor);
                render_editor_frame(&session.buffer, &session.vk, false, rect);
                continue;
            }
            Key::Char('c') if session.vk.is_idle() && (session.vk.is_visual() || !session.buffer.selections.is_empty()) => {
                commit_active_selection(session);
                let end_cursor = session.buffer.cursor();
                let register = session.vk.take_pending_register();
                let deleted = session.buffer.delete_selections(registers, register);
                session.buffer.selections.clear();
                session.vk.end_visual(end_cursor);
                if deleted {
                    match run_insert_mode(session, rect, on_idle)? {
                        InsertOutcome::Detached => return Ok(EditOutcome::Detached),
                        InsertOutcome::Done => {}
                    }
                }
                render_editor_frame(&session.buffer, &session.vk, false, rect);
                continue;
            }
            Key::Char('p') | Key::Char('P') if session.vk.is_idle() && (session.vk.is_visual() || !session.buffer.selections.is_empty()) => {
                commit_active_selection(session);
                let end_cursor = session.buffer.cursor();
                let register = session.vk.take_pending_register();
                session.buffer.put_over_selections(registers, register);
                session.buffer.selections.clear();
                session.vk.end_visual(end_cursor);
                render_editor_frame(&session.buffer, &session.vk, false, rect);
                continue;
            }
            // `S`: vim-surround's own "wrap the selection" -- reads one
            // more raw key directly (the delimiter character), same as
            // editor.rs's own identical arm (see its doc comment).
            Key::Char('S') if session.vk.is_idle() && (session.vk.is_visual() || !session.buffer.selections.is_empty()) => {
                commit_active_selection(session);
                let end_cursor = session.buffer.cursor();
                if let Some(Key::Char(ch)) = editor::read_key_idle(on_idle)? {
                    surround_selections(&mut session.buffer, ch);
                }
                session.buffer.selections.clear();
                session.vk.end_visual(end_cursor);
                render_editor_frame(&session.buffer, &session.vk, false, rect);
                continue;
            }
            Key::Escape if session.vk.is_idle() && (session.vk.is_visual() || !session.buffer.selections.is_empty()) => {
                let end_cursor = session.buffer.cursor();
                session.vk.end_visual(end_cursor);
                session.buffer.selections.clear();
                render_editor_frame(&session.buffer, &session.vk, false, rect);
                continue;
            }
            Key::Char(':') if session.vk.is_idle() => {
                if let ExOutcome::Quit = run_ex_command(session, rect, on_idle)? {
                    return Ok(EditOutcome::Quit);
                }
                render_editor_frame(&session.buffer, &session.vk, false, rect);
                continue;
            }
            _ => {}
        }

        match session.vk.feed(key) {
            KeyOutcome::Motion(m, count) => {
                editor::apply_motion_or_reselect(&mut session.vk, &mut session.buffer, m, count);
                scroll_to_show_cursor(&mut session.buffer);
            }
            KeyOutcome::EnterVisual(shape) => {
                session.vk.begin_visual(shape, session.buffer.cursor());
            }
            KeyOutcome::ReselectVisual => {
                if let Some((shape, anchor, cursor)) = session.vk.last_visual() {
                    session.buffer.set_cursor(cursor.0, cursor.1);
                    session.vk.begin_visual(shape, anchor);
                }
            }
            KeyOutcome::Jump { forward } => {
                let current = session.buffer.cursor();
                let target = if forward { session.vk.jump_forward(current) } else { session.vk.jump_back(current) };
                if let Some((row, col)) = target {
                    let row = row.min(session.buffer.line_count() - 1);
                    let col = col.min(session.buffer.line_len(row));
                    session.buffer.set_cursor(row, col);
                    scroll_to_show_cursor(&mut session.buffer);
                }
            }
            KeyOutcome::EnterInsert(cmd) => {
                resolve_insert_start(&mut session.buffer, cmd);
                match run_insert_mode(session, rect, on_idle)? {
                    InsertOutcome::Detached => return Ok(EditOutcome::Detached),
                    InsertOutcome::Done => {}
                }
            }
            KeyOutcome::Operator(op, m, count, register) => match op {
                Op::Yank => editor::yank_motion(&mut session.buffer, registers, m, count, register),
                Op::Delete => {
                    delete_motion(&mut session.buffer, registers, m, count, register);
                }
                Op::Change => {
                    let m = redirect_cw_to_ce(&session.buffer, &m);
                    if delete_motion(&mut session.buffer, registers, m, count, register) {
                        match run_insert_mode(session, rect, on_idle)? {
                            InsertOutcome::Detached => return Ok(EditOutcome::Detached),
                            InsertOutcome::Done => {}
                        }
                    }
                }
            },
            KeyOutcome::OperatorLines(op, count, register) => match op {
                Op::Yank => editor::yank_lines(&session.buffer, registers, count, register),
                Op::Delete => delete_lines(&mut session.buffer, registers, count, register),
                Op::Change => {
                    delete_lines(&mut session.buffer, registers, count, register);
                    match run_insert_mode(session, rect, on_idle)? {
                        InsertOutcome::Detached => return Ok(EditOutcome::Detached),
                        InsertOutcome::Done => {}
                    }
                }
            },
            KeyOutcome::Put { before, count, register } => put(&mut session.buffer, registers, before, count, register),
            KeyOutcome::DeleteCharForward { count, register } => delete_char_forward(&mut session.buffer, registers, count, register),
            KeyOutcome::Join { count, with_space } => {
                session.buffer.join_lines(count.unwrap_or(1).max(1), with_space);
            }
            KeyOutcome::AddSurround { target, ch } => add_surround(&mut session.buffer, target, ch),
            KeyOutcome::DeleteSurround { ch } => delete_surround(&mut session.buffer, ch),
            KeyOutcome::ChangeSurround { ch, replacement } => change_surround(&mut session.buffer, ch, replacement),
            // <C-w> is still vimkeys' own window-leader prefix here too
            // -- a harmless no-op, same reasoning as editor.rs's own
            // LineBuffer contexts: there's no window state to act on
            // directly from inside this loop. Detaching first (Ctrl+
            // Space) and using it from normal-mode navigation is how
            // window commands actually reach this pane while `e` is
            // open.
            KeyOutcome::Window(..) | KeyOutcome::Pending | KeyOutcome::None => {}
        }
        render_editor_frame(&session.buffer, &session.vk, false, rect);
    }
}

fn commit_active_selection(session: &mut EditSession) {
    if let Some(range) = active_visual_range(&session.vk, &session.buffer) {
        session.buffer.selections.push(range);
    }
}

// Visual mode's own active (not yet committed via `Z`) selection, if any
// -- mirrors repl.rs's own `active_visual_range`/editor.rs's own
// `active_visual_range_line` exactly, just typed to `TextBuffer`.
fn active_visual_range(vk: &VimKeys, buf: &TextBuffer) -> Option<motion::MotionRange> {
    let (shape, anchor) = vk.visual_anchor()?;
    let cursor = buf.cursor();
    let motion_shape = if shape == RegisterShape::Line { motion::MotionShape::Linewise } else { motion::MotionShape::Inclusive };
    let (from, to) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
    Some(motion::MotionRange { shape: motion_shape, from, to })
}

fn scroll_to_show_cursor(buf: &mut TextBuffer) {
    let (line, _) = buf.cursor();
    let height = buf.viewport_height();
    if line < buf.viewport_top() {
        buf.set_viewport_top(line);
    } else if line >= buf.viewport_top() + height {
        buf.set_viewport_top(line + 1 - height);
    }
}

// `d{motion}`/`c{motion}`: resolves `motion` against the buffer's own
// current cursor, removes that range (`TextBuffer::delete_range` already
// does both the extraction *and* the removal in one call -- simpler than
// editor.rs's own LineBuffer-specific version, which had to do them
// separately), writes it to a register. Returns whether anything was
// actually deleted, same as `editor.rs`'s own `delete_motion` -- `Change`
// uses this to decide whether to enter insert mode at all.
fn delete_motion(buf: &mut TextBuffer, registers: &mut Registers, m: motion::Motion, count: Option<usize>, register: Option<char>) -> bool {
    let Some(range) = motion::motion_range(buf, m, count) else {
        return false;
    };
    let shape = if range.shape == motion::MotionShape::Linewise { RegisterShape::Line } else { RegisterShape::Char };
    let text = buf.delete_range(&range);
    registers.write(register, RegisterValue { text, shape });
    true
}

// `dd`/`cc`'s own whole-line shorthand -- deliberately *not* editor.rs's
// own `delete_lines` (that one flattens "linewise" to "the whole buffer",
// only ever correct because `LineBuffer` never has more than one line).
// Scopes to exactly the `count` lines starting at the cursor, same
// clamping `motion::whole_lines` itself already applies, so the yanked
// register text and the removed range always agree on which lines were
// affected.
fn delete_lines(buf: &mut TextBuffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
    let count = count.unwrap_or(1).max(1);
    let text = motion::whole_lines(buf, count);
    registers.write(register, RegisterValue { text, shape: RegisterShape::Line });
    let (row, _) = buf.cursor();
    let last = (row + count - 1).min(buf.line_count().saturating_sub(1));
    let range = motion::MotionRange { shape: motion::MotionShape::Linewise, from: (row, 0), to: (last, 0) };
    buf.delete_range(&range);
}

// `x`: deletes up to `count` characters starting at the cursor, clamped
// to the end of the line -- vim's own primitive (see `vimkeys::
// apply_delete_forward`'s own doc comment on why this isn't quite
// reducible to `d{count}l`).
fn delete_char_forward(buf: &mut TextBuffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
    let (row, col) = buf.cursor();
    let len = buf.line_len(row);
    if len == 0 {
        return;
    }
    let start = col.min(len - 1);
    let end = (start + count.unwrap_or(1).max(1)).min(len);
    let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, start), to: (row, end) };
    let deleted = buf.delete_range(&range);
    if !deleted.is_empty() {
        registers.write(register, RegisterValue { text: deleted, shape: RegisterShape::Char });
    }
}

// `ys{motion}`/`yss`'s own target resolution: a motion resolves exactly
// like any other operator target (`None` on a failed/empty one, same
// silent no-op `delete_motion` already gives); `yss`'s own current-line
// shorthand spans `count` lines starting at the cursor -- the same "this
// operator, this line, `count` of them" shape `delete_lines`'s own doc
// comment already establishes for `dd`/`cc`, unlike `editor.rs`'s own
// single-line `LineBuffer` version of this same target, where `count`
// has nothing further to extend into.
fn resolve_surround_target(buf: &mut TextBuffer, target: &SurroundTarget) -> Option<motion::MotionRange> {
    match target {
        SurroundTarget::Motion(m, count) => motion::motion_range(buf, m.clone(), *count),
        SurroundTarget::Line(count) => {
            let count = count.unwrap_or(1).max(1);
            let (row, _) = buf.cursor();
            let last = (row + count - 1).min(buf.line_count().saturating_sub(1));
            Some(motion::MotionRange { shape: motion::MotionShape::Linewise, from: (row, 0), to: (last, 0) })
        }
    }
}

// `ys{motion}{ch}`/`yss{ch}`: wraps `target`'s resolved range in `ch`'s
// own delimiter pair. Inserts the close delimiter first, then the open
// one -- inserting at/after `close_at` can never shift `open_at`'s own
// position, so no further adjustment is needed regardless of shape (see
// `motion::surround_insert_points`'s own doc comment). Cursor lands on
// the inserted open delimiter's own first character, matching
// vim-surround.
fn add_surround(buf: &mut TextBuffer, target: SurroundTarget, ch: char) {
    let Some(range) = resolve_surround_target(buf, &target) else {
        return;
    };
    let Some((open, close)) = motion::surround_delims(ch) else {
        return;
    };
    let (open_at, close_at) = motion::surround_insert_points(buf, &range);
    buf.insert_text(close_at, &close);
    buf.insert_text(open_at, &open);
    buf.set_cursor(open_at.0, open_at.1);
}

// `ds{ch}`: removes the nearest enclosing pair named by `ch`, plus any
// padding `motion::surround_delete_spans` decides to strip -- close side
// first, so removing it can never shift the open side's own position. A
// no-op if `ch` doesn't name a valid target or no such pair encloses the
// cursor.
fn delete_surround(buf: &mut TextBuffer, ch: char) {
    let Some(kind) = motion::surround_target_kind(ch) else {
        return;
    };
    let Some((open_pos, close_pos)) = motion::surround_pair_positions(buf, kind) else {
        return;
    };
    let (open_range, close_range) = motion::surround_delete_spans(buf, kind, open_pos, close_pos);
    buf.delete_range(&close_range);
    buf.delete_range(&open_range);
    buf.set_cursor(open_range.from.0, open_range.from.1);
}

// `cs{ch}{replacement}`: like `delete_surround`, but replaces the found
// pair's own two delimiter characters with `replacement`'s pair instead
// of removing them -- never touches any padding around them (unlike
// `ds`). Close side first, same reasoning as `add_surround`/
// `delete_surround`.
fn change_surround(buf: &mut TextBuffer, ch: char, replacement: char) {
    let Some(kind) = motion::surround_target_kind(ch) else {
        return;
    };
    let Some((open_pos, close_pos)) = motion::surround_pair_positions(buf, kind) else {
        return;
    };
    let Some((open, close)) = motion::surround_delims(replacement) else {
        return;
    };
    let close_range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: close_pos, to: close_pos };
    buf.delete_range(&close_range);
    buf.insert_text(close_pos, &close);
    let open_range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: open_pos, to: open_pos };
    buf.delete_range(&open_range);
    buf.insert_text(open_pos, &open);
    buf.set_cursor(open_pos.0, open_pos.1);
}

// Visual mode's own `S{ch}` -- wraps every committed selection plus the
// active one in `ch`'s own delimiter pair, mirroring editor.rs's own
// `surround_selections` (see its doc comment) generalized to a
// `TextBuffer`'s multi-line selections/`insert_text`.
fn surround_selections(buf: &mut TextBuffer, ch: char) {
    if buf.selections.is_empty() {
        return;
    }
    let Some((open, close)) = motion::surround_delims(ch) else {
        return;
    };
    let mut ranges = buf.selections.clone();
    ranges.sort_by_key(|r| std::cmp::Reverse(r.from));
    let mut leftmost_open_at = (0, 0);
    for range in &ranges {
        let (open_at, close_at) = motion::surround_insert_points(buf, range);
        buf.insert_text(close_at, &close);
        buf.insert_text(open_at, &open);
        leftmost_open_at = open_at;
    }
    buf.set_cursor(leftmost_open_at.0, leftmost_open_at.1);
}

// `p`/`P`: a linewise register (`yy`, `dd`, ...) pastes as whole new
// line(s) below (`p`)/above (`P`) the cursor's own line; a charwise one
// pastes inline. Built entirely on `insert_text` (which already
// understands embedded newlines) rather than a dedicated line-splice
// primitive -- see this function's own two branches for exactly how
// bracketing the pasted text with a leading/trailing "\n" and choosing
// the insertion column makes `insert_text`'s ordinary line-splitting
// logic land the result in the right place either way.
fn put(buf: &mut TextBuffer, registers: &mut Registers, before: bool, count: Option<usize>, register: Option<char>) {
    let value = registers.read(register);
    if value.text.is_empty() {
        return;
    }
    let count = count.unwrap_or(1).max(1);
    let (row, col) = buf.cursor();
    let repeated: String = value.text.repeat(count);
    match value.shape {
        RegisterShape::Line => {
            let body = repeated.strip_suffix('\n').unwrap_or(&repeated);
            if before {
                buf.insert_text((row, 0), &format!("{body}\n"));
                buf.set_cursor(row, 0);
            } else {
                let end = buf.line_len(row);
                buf.insert_text((row, end), &format!("\n{body}"));
                buf.set_cursor(row + 1, 0);
            }
        }
        RegisterShape::Char => {
            let insert_col = if before { col } else { (col + 1).min(buf.line_len(row)) };
            let new_cursor = buf.insert_text((row, insert_col), &repeated);
            // Cursor ends on the last inserted character -- same rule
            // vimkeys::apply_put's own doc comment establishes for the
            // single-line case.
            buf.set_cursor(new_cursor.0, new_cursor.1.saturating_sub(1));
        }
    }
}

// vim's own "`cw`/`cW` act like `ce`/`cE`" rule -- see editor.rs's own
// `redirect_cw_to_ce` for the full reasoning (identical here, just
// against a `TextBuffer` cursor instead of a `LineEditor` one).
fn redirect_cw_to_ce(buf: &TextBuffer, m: &motion::Motion) -> motion::Motion {
    let (row, col) = buf.cursor();
    let on_word_char = matches!(buf.char_at(row, col), Some(c) if !c.is_whitespace());
    match m {
        motion::Motion::WordForward if on_word_char => motion::Motion::WordEnd,
        motion::Motion::WordForwardBig if on_word_char => motion::Motion::WordEndBig,
        other => other.clone(),
    }
}

// Positions the cursor (and, for `s`/`S`/`C`, removes text first -- same
// "delete always yanks" rule any other delete gets, matching real vim)
// for one `InsertCmd`, *before* `run_insert_mode`'s own typing loop
// starts. Deliberately not `vimkeys::apply_insert_cmd` (that one's
// `Vec<char>`-based, single-line only) -- this is TextBuffer's own
// multi-line equivalent. `SubstituteLine` clears just the *current*
// line's own content (an in-place `delete_range` from column 0 to end,
// same-line so nothing joins) -- never the whole buffer the way
// editor.rs's own single-line `SubstituteLine` handling does, since
// unlike `LineBuffer`, "the current line" and "the whole buffer" are not
// the same thing here.
fn resolve_insert_start(buf: &mut TextBuffer, cmd: InsertCmd) {
    let (row, col) = buf.cursor();
    match cmd {
        InsertCmd::Before => {}
        InsertCmd::After => buf.set_cursor(row, (col + 1).min(buf.line_len(row))),
        InsertCmd::LineStart => buf.set_cursor(row, 0),
        InsertCmd::LineEnd => {
            let len = buf.line_len(row);
            buf.set_cursor(row, len);
        }
        InsertCmd::SubstituteChar => {
            if col < buf.line_len(row) {
                let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (row, col), to: (row, col) };
                buf.delete_range(&range);
            }
        }
        InsertCmd::SubstituteLine => {
            let len = buf.line_len(row);
            let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, 0), to: (row, len) };
            buf.delete_range(&range);
        }
        InsertCmd::ChangeToEnd => {
            let len = buf.line_len(row);
            if col < len {
                let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, col), to: (row, len) };
                buf.delete_range(&range);
            }
        }
        // `gi`: the `^` mark `run_insert_mode` sets every time Insert mode
        // ends, clamped in case the buffer's shrunk since. Falls back to
        // the cursor's own current position if Insert mode has never run
        // yet this session (no mark set) -- matches vim's own "gi with no
        // prior insert behaves like i".
        InsertCmd::LastInsertPos => {
            let (target_row, target_col) = buf.get_mark('^').unwrap_or((row, col));
            let target_row = target_row.min(buf.line_count() - 1);
            let target_col = target_col.min(buf.line_len(target_row));
            buf.set_cursor(target_row, target_col);
        }
    }
}

enum InsertOutcome {
    Done,
    Detached,
}

// The typing loop once Insert mode has actually started (cursor already
// positioned by `resolve_insert_start`, or -- for `c{motion}`/`cc`/
// Visual `c` -- already sitting exactly where a delete just left it, no
// repositioning needed at all). Ctrl+Space works here too, not just from
// Normal mode -- detaching mid-insert leaves whatever was typed in place
// uncommitted (nothing lost, just not yet escaped back to Normal),
// matching every other mid-sequence Ctrl+Space snapshot elsewhere in
// this codebase.
fn run_insert_mode(session: &mut EditSession, rect: Rect, on_idle: &mut dyn FnMut()) -> io::Result<InsertOutcome> {
    render_editor_frame(&session.buffer, &session.vk, true, rect);
    loop {
        let key = match editor::read_key_idle(on_idle)? {
            Some(k) => k,
            None => {
                session.buffer.set_mark('^', session.buffer.cursor());
                return Ok(InsertOutcome::Done);
            }
        };
        match key {
            // `^`: vim's own name for this mark (`:help '^`) -- wherever
            // the cursor was the last time Insert mode ended, however it
            // ended (typed out via Escape/EOF, or a Ctrl+Space detach
            // mid-typing). `gi` reads it back (see resolve_insert_start's
            // own `LastInsertPos` arm).
            Key::CtrlSpace => {
                session.buffer.set_mark('^', session.buffer.cursor());
                return Ok(InsertOutcome::Detached);
            }
            Key::Escape => {
                session.buffer.set_mark('^', session.buffer.cursor());
                return Ok(InsertOutcome::Done);
            }
            Key::Enter => {
                let (row, col) = session.buffer.cursor();
                session.buffer.insert_text((row, col), "\n");
            }
            Key::Backspace => {
                let (row, col) = session.buffer.cursor();
                if col > 0 {
                    let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, col - 1), to: (row, col) };
                    session.buffer.delete_range(&range);
                } else if row > 0 {
                    let prev_len = session.buffer.line_len(row - 1);
                    let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row - 1, prev_len), to: (row, 0) };
                    session.buffer.delete_range(&range);
                }
            }
            Key::Left => motion::apply_motion(&mut session.buffer, motion::Motion::Left, None),
            Key::Right => {
                // `Motion::Right` clamps at the last real character (its
                // ordinary Normal-mode meaning); Insert mode's cursor is
                // allowed one column past that (where the next typed
                // char would land), so this moves it directly rather
                // than going through the clamped motion.
                let (row, col) = session.buffer.cursor();
                session.buffer.set_cursor(row, (col + 1).min(session.buffer.line_len(row)));
            }
            Key::Up => motion::apply_motion(&mut session.buffer, motion::Motion::Up, None),
            Key::Down => motion::apply_motion(&mut session.buffer, motion::Motion::Down, None),
            Key::Char(c) => {
                let (row, col) = session.buffer.cursor();
                let mut b = [0u8; 4];
                session.buffer.insert_text((row, col), c.encode_utf8(&mut b));
            }
            _ => {}
        }
        scroll_to_show_cursor(&mut session.buffer);
        render_editor_frame(&session.buffer, &session.vk, true, rect);
    }
}

enum ExOutcome {
    Continue,
    Quit,
}

// `:w`, `:w <path>`, `:wq`/`:x`, `:q`, `:q!` -- deliberately not
// `editor::read_line` (which every other `:`/`/`-style prompt in this
// codebase reuses): that needs a real `History` to browse, which nothing
// here has a sensible one to offer (no persisted Ex-command history is
// in scope for this pass), so this is its own minimal reader instead --
// print the prompt, read raw keys until Enter/Escape, nothing else
// `read_line` offers (completion, suggestions, browsing) is needed for a
// one-line Ex command anyway.
fn run_ex_command(session: &mut EditSession, rect: Rect, on_idle: &mut dyn FnMut()) -> io::Result<ExOutcome> {
    let Some(line) = read_ex_command_line(rect, on_idle)? else {
        return Ok(ExOutcome::Continue);
    };
    let line = line.trim();
    let (cmd, arg) = match line.split_once(' ') {
        Some((c, a)) => (c, Some(a.trim()).filter(|a| !a.is_empty())),
        None => (line, None),
    };
    match cmd {
        "" => Ok(ExOutcome::Continue),
        "w" | "write" => {
            if let Err(e) = session.buffer.save(arg.map(std::path::Path::new)) {
                flash_status(&format!("E212: Can't open file for writing: {e}"), rect, on_idle)?;
            }
            Ok(ExOutcome::Continue)
        }
        "wq" | "x" => match session.buffer.save(arg.map(std::path::Path::new)) {
            Ok(()) => Ok(ExOutcome::Quit),
            Err(e) => {
                flash_status(&format!("E212: Can't open file for writing: {e}"), rect, on_idle)?;
                Ok(ExOutcome::Continue)
            }
        },
        "q" => {
            if session.buffer.is_dirty() {
                flash_status("E37: No write since last change (add ! to override)", rect, on_idle)?;
                Ok(ExOutcome::Continue)
            } else {
                Ok(ExOutcome::Quit)
            }
        }
        "q!" => Ok(ExOutcome::Quit),
        other => {
            flash_status(&format!("E492: Not an editor command: {other}"), rect, on_idle)?;
            Ok(ExOutcome::Continue)
        }
    }
}

fn editor_content_rows(rect: Rect) -> usize {
    rect.rows.saturating_sub(1).max(1)
}

fn status_row(rect: Rect) -> usize {
    rect.row + editor_content_rows(rect)
}

fn read_ex_command_line(rect: Rect, on_idle: &mut dyn FnMut()) -> io::Result<Option<String>> {
    let row = status_row(rect);
    let mut buf = String::new();
    loop {
        print!("\x1b[{};{}H\x1b[K\x1b[7m:{}\x1b[0m", row + 1, rect.col + 1, buf);
        io::stdout().flush()?;
        let key = match editor::read_key_idle(on_idle)? {
            Some(k) => k,
            None => return Ok(None),
        };
        match key {
            Key::Enter => return Ok(Some(buf)),
            Key::Escape | Key::CtrlC => return Ok(None),
            Key::Backspace => {
                buf.pop();
            }
            Key::Char(c) => buf.push(c),
            _ => {}
        }
    }
}

// Shows `msg` in the status bar and waits for exactly one more keypress
// before returning -- vim's own "Press ENTER or type command to
// continue" convention, simplified to "any key clears it." Without this,
// an error from a failed `:w`/`:q` would be redrawn over by the very
// next `render_editor_frame` call before anyone could ever read it.
fn flash_status(msg: &str, rect: Rect, on_idle: &mut dyn FnMut()) -> io::Result<()> {
    let row = status_row(rect);
    let cols = rect.cols;
    let mut text: String = msg.chars().take(cols).collect();
    text.push_str(&" ".repeat(cols.saturating_sub(text.chars().count())));
    print!("\x1b[{};{}H\x1b[7m{}\x1b[0m", row + 1, rect.col + 1, text);
    io::stdout().flush()?;
    let _ = editor::read_key_idle(on_idle)?;
    Ok(())
}

// "-- NORMAL --"/"-- VISUAL --"/"-- VISUAL LINE --"/"-- INSERT --",
// `[+]` while the buffer has unsaved changes -- vim's own mode-line
// convention, mirroring repl.rs's own `mode_label` plus the dirty flag
// that context has no equivalent of.
fn mode_label(vk: &VimKeys, insert_mode: bool) -> &'static str {
    if insert_mode {
        return "-- INSERT --";
    }
    match vk.visual_anchor() {
        Some((RegisterShape::Char, _)) => "-- VISUAL --",
        Some((RegisterShape::Line, _)) => "-- VISUAL LINE --",
        None => "-- NORMAL --",
    }
}

fn status_text(buf: &TextBuffer, vk: &VimKeys, insert_mode: bool, cols: usize) -> String {
    let label = mode_label(vk, insert_mode);
    let pending = vk.pending_display();
    let mut left = if !pending.is_empty() {
        format!("{label} {pending}")
    } else {
        let last = vk.last_motion_display();
        if !last.is_empty() { format!("{label} [{last}]") } else { label.to_string() }
    };
    if buf.is_dirty() {
        left.push_str(" [+]");
    }
    let (line, col) = buf.cursor();
    let total = buf.line_count();
    let right = format!("{},{}  {}/{}", line + 1, col + 1, line + 1, total);

    let left_len = left.chars().count();
    let right_len = right.chars().count();
    if left_len + right_len < cols {
        left.push_str(&" ".repeat(cols - left_len - right_len));
        left.push_str(&right);
    }
    let text_len = left.chars().count();
    match text_len.cmp(&cols) {
        std::cmp::Ordering::Less => left.push_str(&" ".repeat(cols - text_len)),
        std::cmp::Ordering::Greater => left = left.chars().take(cols).collect(),
        std::cmp::Ordering::Equal => {}
    }
    left
}

// The (start, end) char-column range `range` covers on this one `line`,
// if any -- mirrors repl.rs's own `selection_columns_in_line` exactly
// (see its doc comment), just typed to `TextBuffer`.
fn selection_columns_in_line(range: &motion::MotionRange, line: usize, cols: usize) -> Option<(usize, usize)> {
    if line < range.from.0 || line > range.to.0 {
        return None;
    }
    if range.shape == motion::MotionShape::Linewise {
        return Some((0, cols));
    }
    let start = if line == range.from.0 { range.from.1 } else { 0 };
    let end = if line == range.to.0 { range.to.1 + 1 } else { cols };
    Some((start, end))
}

fn render_row(out: &mut String, buf: &TextBuffer, line: usize, cols: usize, highlights: &[(usize, usize)]) {
    for c in 0..cols {
        let ch = buf.char_at(line, c).unwrap_or(' ');
        if highlights.iter().any(|&(start, end)| c >= start && c < end) {
            out.push_str("\x1b[7m");
            out.push(ch);
            out.push_str("\x1b[0m");
        } else {
            out.push(ch);
        }
    }
}

// The actual rendering, factored out as a pure string-builder (build the
// whole escape-coded string first, print/feed it exactly once) --
// mirrors repl.rs's own `compose_redraw`/`render_compositor_frame`
// split. Content rows + one status row, real-cursor positioning at the
// end. Reimplemented here rather than shared with repl.rs's own
// `render_normal_mode_frame`: the two render different concrete `Buffer`
// types (that one reads a `ScreenBuffer`'s scrollback/live-grid split
// directly, not through the `Buffer` trait at all).
//
// `row_origin`/`col_origin`: where this pane's own row 0/col 0 actually
// lands for *this* target -- `rect.row`/`rect.col` (this pane's real
// absolute position) for the real terminal (`render_editor_frame`,
// below), but `0`/`0` for `freeze_editor_frame`'s own target instead:
// each session's own `vt100::Screen` is sized and addressed *pane-
// relative* (confirmed by render_compositor_frame's own per-pane loop,
// which reads row `r`, not `pane.rect.row + r`, from that pane's own
// screen) -- feeding this pane's *absolute* terminal position into it
// would land the content at completely the wrong cells, or off-grid
// entirely, in a split window. `rect` itself is still used for *size*
// (`rect.rows`/`rect.cols`) either way -- only the position origin
// changes.
pub fn build_editor_frame(buf: &TextBuffer, vk: &VimKeys, insert_mode: bool, rect: Rect, row_origin: usize, col_origin: usize) -> String {
    let content_rows = editor_content_rows(rect);
    let total = buf.line_count();
    let active = if insert_mode { None } else { active_visual_range(vk, buf) };
    let mut out = String::new();
    for r in 0..content_rows {
        let line = buf.viewport_top() + r;
        out.push_str(&format!("\x1b[{};{}H\x1b[K", row_origin + r + 1, col_origin + 1));
        if line < total {
            let mut highlights = Vec::new();
            for range in buf.selections.iter().chain(active.iter()) {
                if let Some(cols) = selection_columns_in_line(range, line, rect.cols) {
                    highlights.push(cols);
                }
            }
            render_row(&mut out, buf, line, rect.cols, &highlights);
        }
    }

    out.push_str(&format!("\x1b[{};{}H\x1b[7m{}\x1b[0m", row_origin + content_rows + 1, col_origin + 1, status_text(buf, vk, insert_mode, rect.cols)));

    let (cl, cc) = buf.cursor();
    let screen_row = cl.saturating_sub(buf.viewport_top()).min(content_rows.saturating_sub(1));
    let screen_col = cc.min(rect.cols.saturating_sub(1));
    out.push_str(&format!("\x1b[{};{}H\x1b[?25h", row_origin + screen_row + 1, col_origin + screen_col + 1));
    out
}

pub fn render_editor_frame(buf: &TextBuffer, vk: &VimKeys, insert_mode: bool, rect: Rect) {
    print!("{}", build_editor_frame(buf, vk, insert_mode, rect, rect.row, rect.col));
    let _ = io::stdout().flush();
}

// Feeds this pane's own current editor state into `screen` (see
// `build_editor_frame`'s own doc comment on why pane-relative addressing
// -- `row_origin`/`col_origin` both `0` -- is what belongs in a
// session's own grid, not this pane's absolute terminal position)
// instead of the real terminal, so a compositor redraw of a pane whose
// top frame is a *detached* `Frame::Edit` shows this editor's own last
// real state instead of stale/blank content. Mirrors `freeze_idle_
// prompt`/`freeze_input_with_text` exactly, just for `Frame::Edit`
// instead of `Frame::Session` and via `build_editor_frame`'s own
// already-absolute-within-the-grid positioning rather than those two
// functions' simpler `\r\x1b[K`-prefixed single-row convention (this
// content spans the pane's whole height, not just one row).
pub fn freeze_editor_frame(screen: &Rc<RefCell<vt100::Screen>>, buf: &TextBuffer, vk: &VimKeys, rect: Rect) {
    let framed = build_editor_frame(buf, vk, false, rect, 0, 0);
    screen.borrow_mut().feed(framed.as_bytes());
}
