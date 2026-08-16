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

use crate::bishedit::highlight::{self, BashHighlighter, HighlightContext, Highlighter, StyledSpan};
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

// What the status line (`mode_label`) and `build_editor_frame`'s own
// Visual-highlight gating need to know about the current mode -- `R`'s
// own addition to what used to be a plain `insert_mode: bool` (Replace
// behaves like Insert for both of those: no Visual selections shown,
// `-- REPLACE --` instead of `-- INSERT --`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorMode {
    Normal,
    Insert,
    Replace,
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
    set_last_filename(session, registers);
    render_editor_frame(&session.buffer, &session.vk, EditorMode::Normal, rect);
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
                render_editor_frame(&session.buffer, &session.vk, EditorMode::Normal, rect);
                continue;
            }
            Key::Char('y') if session.vk.is_idle() && (session.vk.is_visual() || !session.buffer.selections.is_empty()) => {
                commit_active_selection(session);
                let end_cursor = session.buffer.cursor();
                let register = session.vk.take_pending_register();
                session.buffer.yank_selections(registers, register);
                session.buffer.selections.clear();
                session.vk.end_visual(end_cursor);
                render_editor_frame(&session.buffer, &session.vk, EditorMode::Normal, rect);
                continue;
            }
            Key::Char('d') if session.vk.is_idle() && (session.vk.is_visual() || !session.buffer.selections.is_empty()) => {
                commit_active_selection(session);
                let end_cursor = session.buffer.cursor();
                let register = session.vk.take_pending_register();
                session.buffer.delete_selections(registers, register);
                session.buffer.selections.clear();
                session.vk.end_visual(end_cursor);
                render_editor_frame(&session.buffer, &session.vk, EditorMode::Normal, rect);
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
                    match run_insert_mode(session, rect, registers, on_idle, false)? {
                        InsertOutcome::Detached => return Ok(EditOutcome::Detached),
                        InsertOutcome::Done => {}
                    }
                }
                render_editor_frame(&session.buffer, &session.vk, EditorMode::Normal, rect);
                continue;
            }
            Key::Char('p') | Key::Char('P') if session.vk.is_idle() && (session.vk.is_visual() || !session.buffer.selections.is_empty()) => {
                commit_active_selection(session);
                let end_cursor = session.buffer.cursor();
                let register = session.vk.take_pending_register();
                session.buffer.put_over_selections(registers, register);
                session.buffer.selections.clear();
                session.vk.end_visual(end_cursor);
                render_editor_frame(&session.buffer, &session.vk, EditorMode::Normal, rect);
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
                render_editor_frame(&session.buffer, &session.vk, EditorMode::Normal, rect);
                continue;
            }
            // `Ctrl-C` is a plain alias for `Escape` everywhere in this
            // editor (here, and `run_insert_mode`'s own identical arm) --
            // unlike the shell prompt, where Ctrl-C's real SIGINT-adjacent
            // meaning (clear the current line) is still load-bearing, an
            // open `e` session has no such competing use for it, so it's
            // free to also mean "back to Normal," matching vim's own
            // Ctrl-C-also-leaves-Insert/Visual convention.
            Key::Escape | Key::CtrlC if session.vk.is_idle() && (session.vk.is_visual() || !session.buffer.selections.is_empty()) => {
                let end_cursor = session.buffer.cursor();
                session.vk.end_visual(end_cursor);
                session.buffer.selections.clear();
                render_editor_frame(&session.buffer, &session.vk, EditorMode::Normal, rect);
                continue;
            }
            Key::Char(':') if session.vk.is_idle() => {
                if let ExOutcome::Quit = run_ex_command(session, rect, registers, on_idle)? {
                    return Ok(EditOutcome::Quit);
                }
                render_editor_frame(&session.buffer, &session.vk, EditorMode::Normal, rect);
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
                match run_insert_mode(session, rect, registers, on_idle, false)? {
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
                        match run_insert_mode(session, rect, registers, on_idle, false)? {
                            InsertOutcome::Detached => return Ok(EditOutcome::Detached),
                            InsertOutcome::Done => {}
                        }
                    }
                }
                Op::Lowercase | Op::Uppercase | Op::CaseToggle => case_operator_motion(&mut session.buffer, m, count, case_kind_for_op(op)),
            },
            KeyOutcome::OperatorLines(op, count, register) => match op {
                Op::Yank => editor::yank_lines(&session.buffer, registers, count, register),
                Op::Delete => delete_lines(&mut session.buffer, registers, count, register),
                Op::Change => {
                    delete_lines(&mut session.buffer, registers, count, register);
                    match run_insert_mode(session, rect, registers, on_idle, false)? {
                        InsertOutcome::Detached => return Ok(EditOutcome::Detached),
                        InsertOutcome::Done => {}
                    }
                }
                Op::Lowercase | Op::Uppercase | Op::CaseToggle => case_operator_lines(&mut session.buffer, count, case_kind_for_op(op)),
            },
            KeyOutcome::Put { before, count, register } => put(&mut session.buffer, registers, before, count, register),
            KeyOutcome::DeleteCharForward { count, register } => delete_char_forward(&mut session.buffer, registers, count, register),
            KeyOutcome::Join { count, with_space } => {
                session.buffer.join_lines(count.unwrap_or(1).max(1), with_space);
            }
            KeyOutcome::AddSurround { target, ch } => add_surround(&mut session.buffer, target, ch),
            KeyOutcome::DeleteSurround { ch } => delete_surround(&mut session.buffer, ch),
            KeyOutcome::ChangeSurround { ch, replacement } => change_surround(&mut session.buffer, ch, replacement),
            KeyOutcome::ReplaceChar { ch, count } => replace_char(&mut session.buffer, ch, count.unwrap_or(1).max(1)),
            KeyOutcome::EnterReplace => match run_insert_mode(session, rect, registers, on_idle, true)? {
                InsertOutcome::Detached => return Ok(EditOutcome::Detached),
                InsertOutcome::Done => {}
            },
            KeyOutcome::OpenLine { above } => {
                open_line(&mut session.buffer, above);
                match run_insert_mode(session, rect, registers, on_idle, false)? {
                    InsertOutcome::Detached => return Ok(EditOutcome::Detached),
                    InsertOutcome::Done => {}
                }
            }
            KeyOutcome::ToggleCase { count } => toggle_case(&mut session.buffer, count.unwrap_or(1).max(1)),
            KeyOutcome::AdjustNumber { delta } => adjust_number(&mut session.buffer, delta),
            // <C-w> is still vimkeys' own window-leader prefix here too
            // -- a harmless no-op, same reasoning as editor.rs's own
            // LineBuffer contexts: there's no window state to act on
            // directly from inside this loop. Detaching first (Ctrl+
            // Space) and using it from normal-mode navigation is how
            // window commands actually reach this pane while `e` is
            // open.
            KeyOutcome::Window(..) | KeyOutcome::Pending | KeyOutcome::None => {}
        }
        render_editor_frame(&session.buffer, &session.vk, EditorMode::Normal, rect);
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
    registers.record_delete(register, RegisterValue { text, shape });
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
    registers.record_delete(register, RegisterValue { text, shape: RegisterShape::Line });
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
        registers.record_delete(register, RegisterValue { text: deleted, shape: RegisterShape::Char });
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

// `r{ch}`: replaces `count` characters starting at the cursor with `ch`
// each -- see editor.rs's own identical-in-spirit `replace_char` for the
// "never crosses a line break, refuses if not enough characters remain"
// rule. Built on `delete_range`/`insert_text` (no in-place char mutation
// exists on `TextBuffer`) rather than that one's direct `Vec<char>`
// splice, same reasoning `change_surround`'s own doc comment gives.
fn replace_char(buf: &mut TextBuffer, ch: char, count: usize) {
    let (row, col) = buf.cursor();
    if count == 0 || col + count > buf.line_len(row) {
        return;
    }
    let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (row, col), to: (row, col + count - 1) };
    buf.delete_range(&range);
    let text: String = std::iter::repeat_n(ch, count).collect();
    buf.insert_text((row, col), &text);
    buf.set_cursor(row, col + count - 1);
}

// `Ctrl-A`/`Ctrl-X`: see editor.rs's own identical-in-spirit
// `adjust_number`.
fn adjust_number(buf: &mut TextBuffer, delta: i64) {
    let Some(m) = motion::find_number(buf, buf.cursor()) else {
        return;
    };
    let replacement = motion::apply_number_delta(&m, delta);
    let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: m.from, to: m.to };
    buf.delete_range(&range);
    buf.insert_text(m.from, &replacement);
    buf.set_cursor(m.from.0, m.from.1 + replacement.chars().count() - 1);
}

fn case_kind_for_op(op: Op) -> motion::CaseKind {
    match op {
        Op::Lowercase => motion::CaseKind::Lower,
        Op::Uppercase => motion::CaseKind::Upper,
        Op::CaseToggle => motion::CaseKind::Toggle,
        Op::Yank | Op::Delete | Op::Change => unreachable!("case_kind_for_op is only ever called for Op::Lowercase/Uppercase/CaseToggle"),
    }
}

// `gu{motion}`/`gU{motion}`/`g~{motion}`'s shared range transform, and
// `guu`/`gUU`/`g~~`'s own whole-line shorthand (via a `Linewise` range
// built the same way `delete_lines`'s own doc comment describes).
// `Linewise` is handled one row at a time -- each row's own content is
// replaced in place (delete then reinsert *without* any embedded `\n`,
// so line count is never at risk) -- rather than reusing the
// `Inclusive`/`Exclusive` branch's single `extract_text`/`delete_range`/
// `insert_text` round trip: that branch's text can safely carry the
// embedded `\n`s it already had (never changes line count, since
// `motion::extract_text`'s own boundary-crossing `\n`s exactly mirror
// what's being put back), but `Linewise`'s own trailing `\n` after the
// *last* line would, reinserted as-is, split off a spurious extra blank
// line where the following content should have reattached instead
// (`put`'s own Linewise branch sidesteps this differently, by wrapping a
// stripped body in a fresh leading/trailing `\n` itself rather than
// reusing one already baked into extracted text -- not reusable here
// since this needs an exact in-place replace, not a shift-everything-
// down paste).
fn case_operator_range(buf: &mut TextBuffer, range: &motion::MotionRange, kind: motion::CaseKind) {
    if range.shape == motion::MotionShape::Linewise {
        for row in range.from.0..=range.to.0 {
            let len = buf.line_len(row);
            if len == 0 {
                continue;
            }
            // `Inclusive`/`len - 1` (the line's own last real character),
            // not `Exclusive`/`len` (one past it): `motion::extract_text`
            // walks forward via `step_forward`, which never lands on
            // that virtual past-the-end column at all (see its own doc
            // comment) -- an `Exclusive` range built to stop *there*
            // never actually satisfies the walk's own "reached `to`"
            // check, so it silently keeps walking into the *next* line's
            // content instead. `delete_range`'s own splice is unaffected
            // (it's bounded by row indices, not this walk), but the text
            // it returns would be, and this function reinserts exactly
            // that text -- unlike most other callers, which only care
            // about the splice succeeding, not the returned text being
            // pixel-perfect.
            let line_range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (row, 0), to: (row, len - 1) };
            let text = buf.delete_range(&line_range);
            let transformed: String = text.chars().map(|c| motion::case_transform(c, kind)).collect();
            buf.insert_text((row, 0), &transformed);
        }
        buf.set_cursor(range.from.0, 0);
        return;
    }
    let text = buf.delete_range(range);
    let transformed: String = text.chars().map(|c| motion::case_transform(c, kind)).collect();
    buf.insert_text(range.from, &transformed);
    buf.set_cursor(range.from.0, range.from.1);
}

fn case_operator_motion(buf: &mut TextBuffer, m: motion::Motion, count: Option<usize>, kind: motion::CaseKind) {
    let Some(range) = motion::motion_range(buf, m, count) else {
        return;
    };
    case_operator_range(buf, &range, kind);
}

fn case_operator_lines(buf: &mut TextBuffer, count: Option<usize>, kind: motion::CaseKind) {
    let count = count.unwrap_or(1).max(1);
    let (row, _) = buf.cursor();
    let last = (row + count - 1).min(buf.line_count().saturating_sub(1));
    let range = motion::MotionRange { shape: motion::MotionShape::Linewise, from: (row, 0), to: (last, 0) };
    case_operator_range(buf, &range, kind);
}

// `~`: toggles the case of `count` characters starting at the cursor,
// then advances the cursor to just past the last one toggled -- see
// editor.rs's own identical-in-spirit `toggle_case`. Builds the whole
// toggled run as one string and swaps it in via a single `delete_range`/
// `insert_text` pair rather than `TextBuffer` having any in-place
// per-character mutation to loop over.
fn toggle_case(buf: &mut TextBuffer, count: usize) {
    let (row, col) = buf.cursor();
    let len = buf.line_len(row);
    if col >= len {
        return;
    }
    let end_col = (col + count).min(len);
    let text: String = (col..end_col).map(|c| motion::case_transform(buf.char_at(row, c).unwrap(), motion::CaseKind::Toggle)).collect();
    let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, col), to: (row, end_col) };
    buf.delete_range(&range);
    buf.insert_text((row, col), &text);
    buf.set_cursor(row, end_col.min(buf.line_len(row).saturating_sub(1)));
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
// `o`/`O`: splices a bare newline in at the end of the current line
// (`above: false`) or the start of it (`above: true`) via `insert_text`,
// which does the actual line-splitting. For `o`, `insert_text`'s own
// returned cursor already lands exactly right (row + 1, col 0 -- the
// fresh empty line below). For `O` it doesn't: inserting at column 0
// pushes the *original* content down to row + 1 and leaves the new empty
// line at the original `row`, but `insert_text`'s "cursor right after
// what was just inserted" convention still reports (row + 1, 0) -- the
// pushed-down line, not the new blank one -- so this repositions
// explicitly for that case.
fn open_line(buf: &mut TextBuffer, above: bool) {
    let (row, _) = buf.cursor();
    if above {
        buf.insert_text((row, 0), "\n");
        buf.set_cursor(row, 0);
    } else {
        let len = buf.line_len(row);
        buf.insert_text((row, len), "\n");
    }
}

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
//
// `replace` (`R` -- see `KeyOutcome::EnterReplace`'s own doc comment)
// swaps two of this loop's own arms (`Key::Char`/`Key::Backspace`) from
// their ordinary Insert-mode behavior to overtype instead -- everything
// else (motion keys, Enter, detach/exit) behaves identically either way,
// which is why this is a flag on the one shared loop rather than a
// second copy of it.
fn run_insert_mode(session: &mut EditSession, rect: Rect, registers: &mut Registers, on_idle: &mut dyn FnMut(), replace: bool) -> io::Result<InsertOutcome> {
    let mode = if replace { EditorMode::Replace } else { EditorMode::Insert };
    render_editor_frame(&session.buffer, &session.vk, mode, rect);
    // `"."`'s own accumulator for this session -- see `Registers::
    // set_last_insert`'s own doc comment. Best-effort: a Backspace just
    // pops the most recently accumulated character regardless of whether
    // it's actually erasing something typed *this* session or older
    // pre-existing text it backed into -- real vim tracks that
    // distinction precisely; this doesn't.
    let mut inserted = String::new();
    loop {
        let key = match editor::read_key_idle(on_idle)? {
            Some(k) => k,
            None => {
                session.buffer.set_mark('^', session.buffer.cursor());
                registers.set_last_insert(inserted);
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
                registers.set_last_insert(inserted);
                return Ok(InsertOutcome::Detached);
            }
            // See the identical `Escape | CtrlC` arm in `drive`'s own
            // Visual-mode handling for why Ctrl-C is a plain alias for
            // Escape throughout this editor.
            Key::Escape | Key::CtrlC => {
                session.buffer.set_mark('^', session.buffer.cursor());
                registers.set_last_insert(inserted);
                return Ok(InsertOutcome::Done);
            }
            Key::Enter => {
                let (row, col) = session.buffer.cursor();
                session.buffer.insert_text((row, col), "\n");
                inserted.push('\n');
            }
            // Replace mode's own Backspace: known simplification -- steps
            // the cursor back without restoring the character it walks
            // back over (real vim remembers and restores each one) and
            // never crosses a line boundary backward, unlike ordinary
            // Insert mode's own version just below.
            Key::Backspace if replace => {
                let (row, col) = session.buffer.cursor();
                if col > 0 {
                    session.buffer.set_cursor(row, col - 1);
                }
                inserted.pop();
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
                inserted.pop();
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
                // Replace mode overwrites the character already at the
                // cursor, if there is one -- deleting it first, then
                // inserting, naturally extends the line once the cursor
                // reaches its end (nothing left there to overwrite),
                // matching real vim's own `R` behavior at end of line.
                if replace && col < session.buffer.line_len(row) {
                    let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (row, col), to: (row, col) };
                    session.buffer.delete_range(&range);
                }
                let mut b = [0u8; 4];
                session.buffer.insert_text((row, col), c.encode_utf8(&mut b));
                inserted.push(c);
            }
            _ => {}
        }
        scroll_to_show_cursor(&mut session.buffer);
        render_editor_frame(&session.buffer, &session.vk, mode, rect);
    }
}

enum ExOutcome {
    Continue,
    Quit,
}

// `"%"`: vim's own current-filename register -- refreshed here (a no-op
// if there's no path, e.g. an unnamed buffer that still hasn't been
// written anywhere) whenever the buffer's own path could plausibly have
// just changed (`drive`'s own initial open, and every successful `:w`/
// `:wq`/`:x` here, since `:w newname` can name a previously-unnamed
// buffer).
fn set_last_filename(session: &EditSession, registers: &mut Registers) {
    if let Some(path) = session.buffer.path() {
        registers.set_last_filename(path.to_string_lossy().into_owned());
    }
}

// `:w`, `:w <path>`, `:wq`/`:x`, `:q`, `:q!` -- deliberately not
// `editor::read_line` (which every other `:`/`/`-style prompt in this
// codebase reuses): that needs a real `History` to browse, which nothing
// here has a sensible one to offer (no persisted Ex-command history is
// in scope for this pass), so this is its own minimal reader instead --
// print the prompt, read raw keys until Enter/Escape, nothing else
// `read_line` offers (completion, suggestions, browsing) is needed for a
// one-line Ex command anyway.
fn run_ex_command(session: &mut EditSession, rect: Rect, registers: &mut Registers, on_idle: &mut dyn FnMut()) -> io::Result<ExOutcome> {
    let Some(line) = read_ex_command_line(rect, on_idle)? else {
        return Ok(ExOutcome::Continue);
    };
    let line = line.trim();
    // `":"`: recorded regardless of what the command turns out to be
    // (matching vim: even a failed `:nonsense` becomes the new `":`).
    registers.set_last_ex_command(line.to_string());
    let (cmd, arg) = match line.split_once(' ') {
        Some((c, a)) => (c, Some(a.trim()).filter(|a| !a.is_empty())),
        None => (line, None),
    };
    match cmd {
        "" => Ok(ExOutcome::Continue),
        "w" | "write" => {
            match session.buffer.save(arg.map(std::path::Path::new)) {
                Ok(()) => set_last_filename(session, registers),
                Err(e) => flash_status(&format!("E212: Can't open file for writing: {e}"), rect, on_idle)?,
            }
            Ok(ExOutcome::Continue)
        }
        "wq" | "x" => match session.buffer.save(arg.map(std::path::Path::new)) {
            Ok(()) => {
                set_last_filename(session, registers);
                Ok(ExOutcome::Quit)
            }
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

// "-- NORMAL --"/"-- VISUAL --"/"-- VISUAL LINE --"/"-- INSERT --"/
// "-- REPLACE --", `[+]` while the buffer has unsaved changes -- vim's
// own mode-line convention, mirroring repl.rs's own `mode_label` plus
// the dirty flag that context has no equivalent of.
fn mode_label(vk: &VimKeys, mode: EditorMode) -> &'static str {
    match mode {
        EditorMode::Insert => return "-- INSERT --",
        EditorMode::Replace => return "-- REPLACE --",
        EditorMode::Normal => {}
    }
    match vk.visual_anchor() {
        Some((RegisterShape::Char, _)) => "-- VISUAL --",
        Some((RegisterShape::Line, _)) => "-- VISUAL LINE --",
        None => "-- NORMAL --",
    }
}

fn status_text(buf: &TextBuffer, vk: &VimKeys, mode: EditorMode, cols: usize) -> String {
    let label = mode_label(vk, mode);
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

// One column of the gutter drawn to the left of a line's own content.
// Line numbers are the only column today, but the shape anticipates the
// same slot holding blame/coverage/diff-marker columns later -- each is
// just another (width, render) pair appended to GUTTER_COLUMNS, with no
// change needed anywhere else: build_editor_frame only ever asks "how
// wide is the whole gutter" and "render line N's gutter cells", never
// which columns exist. `render` returns the *fully styled* cell text
// (its own SGR codes, if it wants color/dim) so each column stays free
// to look however it needs to -- a future diff column wants red/green,
// not the line-number column's dim gray -- rather than this shared
// machinery imposing one style on all of them. `None` means "blank,
// unstyled" -- used for filler rows past the buffer's own last line,
// matching how render_row's own caller already leaves those rows'
// content blank too.
struct GutterColumn {
    width: fn(&TextBuffer) -> usize,
    render: fn(buf: &TextBuffer, line: usize, width: usize) -> Option<String>,
}

static GUTTER_COLUMNS: &[GutterColumn] = &[GutterColumn { width: line_number_width, render: render_line_number_cell }];

fn total_gutter_width(buf: &TextBuffer) -> usize {
    GUTTER_COLUMNS.iter().map(|col| (col.width)(buf)).sum()
}

fn render_gutter(out: &mut String, buf: &TextBuffer, line: usize) {
    for col in GUTTER_COLUMNS {
        let width = (col.width)(buf);
        match (col.render)(buf, line, width) {
            Some(cell) => out.push_str(&cell),
            None => out.push_str(&" ".repeat(width)),
        }
    }
}

// Vim's own gutter-width convention: as many digits as the buffer's last
// line number needs, plus one trailing space of padding before the
// buffer's own content starts. Grows dynamically as the buffer gains
// lines (matching vim), rather than reserving a fixed width up front.
fn line_number_width(buf: &TextBuffer) -> usize {
    buf.line_count().to_string().len() + 1
}

fn render_line_number_cell(buf: &TextBuffer, line: usize, width: usize) -> Option<String> {
    if line >= buf.line_count() {
        return None;
    }
    Some(format!("\x1b[2m{:>pad$} \x1b[0m", line + 1, pad = width.saturating_sub(1)))
}

// Language detection, v1: a bare extension check, not a content sniff --
// `.bash` is the only recognized language today (per the feature
// request this shipped under); anything else (an unnamed buffer, no
// extension, a different one) renders as plain text, same as before
// this existed. A real "detect from shebang/content" fallback, and more
// extensions/languages, are natural follow-ups once there's more than
// one Highlighter implementor to dispatch to (see Highlighter's own doc
// comment -- BashHighlighter is still the only one).
fn is_bash_file(buf: &TextBuffer) -> bool {
    buf.path().and_then(|p| p.extension()).is_some_and(|ext| ext == "bash")
}

// The buffer's own text, lines joined by '\n' -- what BashHighlighter
// needs to see a construct that spans several physical lines (a
// multi-line double-quoted string, a heredoc body) as the single token
// it actually is, instead of each line's own lexer run finding a
// dangling, unterminated piece of it with no idea what line came
// before. Recomputed on every redraw, same as buffer_highlight_spans
// below -- see that function's own doc comment on why that's an
// accepted, not-yet-a-problem cost rather than something cached here.
fn buffer_text(buf: &TextBuffer) -> String {
    (0..buf.line_count())
        .map(|l| (0..buf.line_len(l)).map(|c| buf.char_at(l, c).unwrap_or(' ')).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

// The char offset `buffer_text`'s own joined string lands each line's
// first character at -- a running total of line_len(l) real chars plus
// one for the '\n' joining it to the next line. Computed once as a
// prefix-sum vector, indexed by line number, rather than summed fresh
// per row -- build_editor_frame's own loop calls this once and reuses
// it, not once per visible row.
fn line_starts(buf: &TextBuffer) -> Vec<usize> {
    let mut starts = Vec::with_capacity(buf.line_count());
    let mut pos = 0;
    for l in 0..buf.line_count() {
        starts.push(pos);
        pos += buf.line_len(l) + 1;
    }
    starts
}

// Runs BashHighlighter once against the *whole* buffer text (see
// buffer_text's own doc comment for why that, not one line at a time,
// is what actually fixes a multi-line construct), for `.bash` files
// only -- `is_bash_file`'s own doc comment covers everything else.
// Recomputed on every redraw (build_editor_frame's own single call
// site, once per keystroke) rather than cached on EditSession: this
// editor's target files are scripts/configs, not large codebases, and
// re-lexing one of those is well under a millisecond -- a real caching
// need would show up as noticeable input lag first, at which point it's
// a small, self-contained change (memoize on buf.is_dirty() going true)
// rather than one worth guessing at up front. HighlightContext::
// default() (no cwd, no known_functions): same "no context to offer"
// choice command mode's own colon-line already makes, since nothing
// here has a live Shell to pull those from -- Flag/Subcommand/Link/
// InvalidCommand refinements that need them simply don't fire, same as
// there.
//
// Not a full fix for every multi-line case: `next_span`'s own doc
// comment (this same module) documents a pre-existing lexer position-
// tracking gap for a heredoc body that itself contains a $VAR/$(...)
// expansion -- content *after* such a heredoc in the same buffer can
// still come out mis-highlighted. Narrower and pre-existing either way,
// not something this change introduces or could fix without touching
// lexer.rs's own heredoc-body capture.
fn buffer_highlight_spans(buf: &TextBuffer) -> Vec<StyledSpan> {
    if !is_bash_file(buf) {
        return Vec::new();
    }
    let text = buffer_text(buf);
    BashHighlighter
        .highlight(&text, HighlightContext::default())
        .into_iter()
        .map(|s| {
            let (fg, attrs) = highlight::default_style(s.kind);
            StyledSpan { start: s.start, end: s.end, fg, attrs }
        })
        .collect()
}

// Slices `spans` (whole-buffer char offsets, from buffer_highlight_spans)
// down to the ones actually touching [line_start, line_start + line_len)
// -- one line's own visible extent -- translated to offsets local to
// that line (0 = its own first character), which is what render_row's
// `chars` array (also one line long) can actually be indexed by. A span
// that starts on an earlier line and/or continues onto a later one
// (exactly the multi-line case this whole pass exists for) is clamped
// at both ends rather than dropped, so e.g. a multi-line string still
// shows its color on every line it passes through.
fn spans_for_line(spans: &[StyledSpan], line_start: usize, line_len: usize) -> Vec<StyledSpan> {
    let line_end = line_start + line_len;
    spans
        .iter()
        .filter(|s| s.start < line_end && s.end > line_start)
        .map(|s| StyledSpan { start: s.start.saturating_sub(line_start), end: (s.end - line_start).min(line_len), fg: s.fg, attrs: s.attrs })
        .collect()
}

// Reuses editor.rs's own compose_redraw pipeline (BashHighlighter ->
// StyledSpan -> highlight::compose -> highlight::render_styled), with
// this editor's own selection highlighting (`highlights`, already
// resolved to column ranges by build_editor_frame's own caller) as a
// second compose() layer instead of editor.rs's ghost-text/search-match
// ones -- exactly the extensibility compose's own doc comment describes
// ("adding a ... layer later is 'pass one more slice'"). Reverse-video,
// not a distinct color, for the same reason editor.rs's own Visual-mode
// selection layer uses it: applied *after* the syntax layer, so a
// selection reads clearly regardless of what color it's covering,
// matching vim's own selection-over-syntax convention. `line_styled` is
// this line's own slice of a whole-buffer highlight pass (see
// spans_for_line), computed once by build_editor_frame's own caller
// rather than per row.
fn render_row(out: &mut String, buf: &TextBuffer, line: usize, cols: usize, line_styled: &[StyledSpan], highlights: &[(usize, usize)]) {
    let chars: Vec<char> = (0..cols).map(|c| buf.char_at(line, c).unwrap_or(' ')).collect();

    let selected: Vec<StyledSpan> = highlights
        .iter()
        .map(|&(start, end)| StyledSpan { start, end, fg: vt100::Color::Default, attrs: vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() } })
        .collect();

    let cells = highlight::compose(&chars, &[line_styled, &selected]);
    out.push_str(&highlight::render_styled(&cells));
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
pub fn build_editor_frame(buf: &TextBuffer, vk: &VimKeys, mode: EditorMode, rect: Rect, row_origin: usize, col_origin: usize) -> String {
    let content_rows = editor_content_rows(rect);
    let total = buf.line_count();
    // Reserves at least one column for content even if the gutter would
    // otherwise want more than the whole pane -- only reachable in a
    // pathologically narrow split, but `content_cols` below would
    // underflow without this.
    let gutter_width = total_gutter_width(buf).min(rect.cols.saturating_sub(1));
    let content_cols = rect.cols - gutter_width;
    let active = if mode == EditorMode::Normal { active_visual_range(vk, buf) } else { None };
    // Computed once for the whole buffer, not per visible row -- see
    // buffer_highlight_spans's own doc comment for why a multi-line
    // construct needs that.
    let whole_styled = buffer_highlight_spans(buf);
    let starts = line_starts(buf);
    let mut out = String::new();
    for r in 0..content_rows {
        let line = buf.viewport_top() + r;
        out.push_str(&format!("\x1b[{};{}H\x1b[K", row_origin + r + 1, col_origin + 1));
        render_gutter(&mut out, buf, line);
        if line < total {
            let mut highlights = Vec::new();
            for range in buf.selections.iter().chain(active.iter()) {
                if let Some(cols) = selection_columns_in_line(range, line, content_cols) {
                    highlights.push(cols);
                }
            }
            let line_styled = spans_for_line(&whole_styled, starts[line], buf.line_len(line));
            render_row(&mut out, buf, line, content_cols, &line_styled, &highlights);
        }
    }

    out.push_str(&format!("\x1b[{};{}H\x1b[7m{}\x1b[0m", row_origin + content_rows + 1, col_origin + 1, status_text(buf, vk, mode, rect.cols)));

    let (cl, cc) = buf.cursor();
    let screen_row = cl.saturating_sub(buf.viewport_top()).min(content_rows.saturating_sub(1));
    let screen_col = gutter_width + cc.min(content_cols.saturating_sub(1));
    out.push_str(&format!("\x1b[{};{}H\x1b[?25h", row_origin + screen_row + 1, col_origin + screen_col + 1));
    out
}

pub fn render_editor_frame(buf: &TextBuffer, vk: &VimKeys, mode: EditorMode, rect: Rect) {
    print!("{}", build_editor_frame(buf, vk, mode, rect, rect.row, rect.col));
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
    let framed = build_editor_frame(buf, vk, EditorMode::Normal, rect, 0, 0);
    screen.borrow_mut().feed(framed.as_bytes());
}
