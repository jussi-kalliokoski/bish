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

use crate::bishedit::format::BashFormatter;
use crate::bishedit::highlight::{self, BashHighlighter, HighlightContext, Highlighter, StyledSpan};
use crate::bishedit::lint::{self, BashLinter, Linter};
use crate::bishedit::motion;
use crate::bishedit::registers::{RegisterShape, RegisterValue, Registers};
use crate::bishedit::textbuffer::TextBuffer;
use crate::bishedit::unicode_width::{char_at_col, char_width, col_of};
use crate::bishedit::vimkeys::{InsertCmd, Op, SurroundTarget, VimKeys, INDENT_WIDTH};
use crate::bishedit::Buffer;
use crate::editor::{self, Key};
use crate::repl::Rect;
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

// `content_cols`: the pane's own current content width (post-gutter),
// same value `build_editor_frame` itself would compute for `rect` right
// now -- see `editor_content_cols`, which every caller of this function
// uses to get it. Passed in fresh rather than read back off `buf` (unlike
// `viewport_height`, a stored field with no resize hook -- see Buffer::
// viewport_left's own doc comment) so a pane resize is honored on the
// very next keystroke instead of only once `vheight` happens to get
// resynced some other way.
//
// `viewport_left` is a *display column*, not a char index (matching its
// own doc comment's original stated intent -- see bishedit::
// unicode_width's own doc comment for why those two aren't the same
// number once a line has any wide/zero-width chars before the cursor).
// The cursor's own char index is converted via `col_of` before the
// exact same threshold comparison this function always did.
pub(crate) fn scroll_to_show_cursor(buf: &mut TextBuffer, content_cols: usize) {
    let (line, col) = buf.cursor();
    let height = buf.viewport_height();
    if line < buf.viewport_top() {
        buf.set_viewport_top(line);
    } else if line >= buf.viewport_top() + height {
        buf.set_viewport_top(line + 1 - height);
    }
    let cursor_col = col_of(&buf.line_chars(line), col);
    let width = content_cols.max(1);
    if cursor_col < buf.viewport_left() {
        buf.set_viewport_left(cursor_col);
    } else if cursor_col >= buf.viewport_left() + width {
        buf.set_viewport_left(cursor_col + 1 - width);
    }
}

// How many columns are actually left for `buf`'s own text after its
// gutter (line numbers, diagnostic markers) -- the exact formula build_
// editor_frame itself uses for `content_cols`, factored out so scroll_
// to_show_cursor's callers (run_insert_mode, and repl.rs's own copy for
// NavBuffer navigation) can compute the same width without duplicating
// the gutter-clamping arithmetic.
pub(crate) fn editor_content_cols(buf: &TextBuffer, rect: Rect) -> usize {
    let gutter_width = total_gutter_width(buf).min(rect.cols.saturating_sub(1));
    rect.cols - gutter_width
}

// `K`-hover's own popup rendering, shared by debugger.rs's own read-only
// view and repl.rs's real file editor (see docs.rs's own hover_lines for
// the content half of this same sharing) -- no existing generic
// floating-popup primitive in this codebase to reuse otherwise (the
// closest precedents, the completion row and the command-output
// overlay, are both fixed-position, not cursor-relative). Anchored just
// below `cursor_row`/`cursor_col` (already screen-relative -- see each
// caller's own cursor_screen_pos-style helper), flipping above when
// there isn't room below, clamped to stay within `rect` so it never
// overlaps whatever sits below the source view (a status row, an output
// pane, ...).
pub(crate) fn render_hover_popup(lines: &[String], cursor_row: usize, cursor_col: usize, rect: Rect) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let max_width = rect.cols.saturating_sub(4).clamp(10, 60);
    let wrapped: Vec<String> = lines.iter().flat_map(|l| wrap_hover_line(l, max_width)).take(12).collect();
    if wrapped.is_empty() {
        return String::new();
    }
    let inner_width = wrapped.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(1);
    let box_width = (inner_width + 2).min(rect.cols.max(3));
    let box_height = wrapped.len() + 2;

    let bottom_limit = rect.row + rect.rows;
    let top = if cursor_row + 1 + box_height <= bottom_limit {
        cursor_row + 1
    } else {
        cursor_row.saturating_sub(box_height).max(rect.row)
    };
    let left = cursor_col.min((rect.col + rect.cols).saturating_sub(box_width));

    let mut out = String::new();
    out.push_str(&format!("\x1b[{};{}H\x1b[7m╭{}╮\x1b[0m", top + 1, left + 1, "─".repeat(box_width.saturating_sub(2))));
    for (i, line) in wrapped.iter().enumerate() {
        let padded = format!("{:<width$}", line, width = inner_width);
        out.push_str(&format!("\x1b[{};{}H\x1b[7m│{}│\x1b[0m", top + 2 + i, left + 1, padded));
    }
    out.push_str(&format!("\x1b[{};{}H\x1b[7m╰{}╯\x1b[0m", top + box_height, left + 1, "─".repeat(box_width.saturating_sub(2))));
    out
}

// Plain character-level wrap (not word-aware) for one hover line into
// however many `width`-wide rows it takes -- simple, and entirely
// adequate for the short doc comments/man snippets this actually
// displays.
fn wrap_hover_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(width).map(|c| c.iter().collect()).collect()
}

// `d{motion}`/`c{motion}`: resolves `motion` against the buffer's own
// current cursor, removes that range (`TextBuffer::delete_range` already
// does both the extraction *and* the removal in one call -- simpler than
// editor.rs's own LineBuffer-specific version, which had to do them
// separately), writes it to a register. Returns whether anything was
// actually deleted, same as `editor.rs`'s own `delete_motion` -- `Change`
// uses this to decide whether to enter insert mode at all.
pub(crate) fn delete_motion(buf: &mut TextBuffer, registers: &mut Registers, m: motion::Motion, count: Option<usize>, register: Option<char>) -> bool {
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
pub(crate) fn delete_lines(buf: &mut TextBuffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
    let count = count.unwrap_or(1).max(1);
    let text = motion::whole_lines(buf, count);
    registers.record_delete(register, RegisterValue { text, shape: RegisterShape::Line });
    let (row, _) = buf.cursor();
    let last = (row + count - 1).min(buf.line_count().saturating_sub(1));
    let range = motion::MotionRange { shape: motion::MotionShape::Linewise, from: (row, 0), to: (last, 0) };
    buf.delete_range(&range);
}

// `>{motion}`/`>>`/Visual `>`'s own shared row-range primitive: prepends
// INDENT_WIDTH spaces to every *non-empty* line in `from_row..=to_row` --
// vim's own rule that shifting right never adds trailing whitespace to a
// genuinely empty line. Callers are always some already-resolved whole-
// line range: `>`/`<` act linewise regardless of the motion/selection
// that produced it (see `vimkeys::Op::Indent`'s own doc comment), so
// there's no shape/column math left to do here, just the two row bounds.
fn indent_rows(buf: &mut TextBuffer, from_row: usize, to_row: usize) {
    for row in from_row..=to_row {
        if buf.line_len(row) == 0 {
            continue;
        }
        buf.insert_text((row, 0), &" ".repeat(INDENT_WIDTH));
    }
}

// `<{motion}`/`<<`/Visual `<`'s own counterpart: strips up to
// INDENT_WIDTH columns of leading whitespace from every line in range --
// vim's own "outdent removes at most one shiftwidth's worth" rule (a
// line indented less than that just loses whatever it has).
fn outdent_rows(buf: &mut TextBuffer, from_row: usize, to_row: usize) {
    for row in from_row..=to_row {
        let strip = (0..buf.line_len(row).min(INDENT_WIDTH)).take_while(|&c| matches!(buf.char_at(row, c), Some(' ') | Some('\t'))).count();
        if strip == 0 {
            continue;
        }
        let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, 0), to: (row, strip) };
        buf.delete_range(&range);
    }
}

// `>{motion}`/`<{motion}`: resolves `m` exactly like any other operator
// target (`motion::motion_range`'s own ordinary rules -- a failed/empty
// one is silently a no-op, same as `delete_motion`), but only ever reads
// its resolved row bounds, never its shape/columns -- `>`/`<` always act
// linewise (see `vimkeys::Op::Indent`'s own doc comment), unlike every
// other operator in this file. Known simplification: real vim also
// shortens an *exclusive* motion's own end row by one when it lands
// exactly on column 0 of the next line (so e.g. `>j` from a line's own
// start doesn't pull in a line the cursor only just barely touched) --
// not implemented here, so a motion landing exactly at another line's
// start currently includes that line too.
pub(crate) fn indent_operator_motion(buf: &mut TextBuffer, m: motion::Motion, count: Option<usize>) {
    let Some(range) = motion::motion_range(buf, m, count) else { return };
    indent_rows(buf, range.from.0, range.to.0);
    buf.set_cursor(range.from.0, 0);
}

pub(crate) fn outdent_operator_motion(buf: &mut TextBuffer, m: motion::Motion, count: Option<usize>) {
    let Some(range) = motion::motion_range(buf, m, count) else { return };
    outdent_rows(buf, range.from.0, range.to.0);
    buf.set_cursor(range.from.0, 0);
}

// `>>`/`<<`'s own whole-line shorthand -- same `count` lines starting at
// the cursor that `delete_lines`'s own doc comment already establishes
// for `dd`/`cc`. `count` only ever selects *how many lines* are shifted,
// never *by how much* -- `3>>` shifts 3 lines by one shiftwidth each,
// not one line by three, matching real vim.
pub(crate) fn indent_lines(buf: &mut TextBuffer, count: Option<usize>) {
    let count = count.unwrap_or(1).max(1);
    let (row, _) = buf.cursor();
    let last = (row + count - 1).min(buf.line_count().saturating_sub(1));
    indent_rows(buf, row, last);
    buf.set_cursor(row, 0);
}

pub(crate) fn outdent_lines(buf: &mut TextBuffer, count: Option<usize>) {
    let count = count.unwrap_or(1).max(1);
    let (row, _) = buf.cursor();
    let last = (row + count - 1).min(buf.line_count().saturating_sub(1));
    outdent_rows(buf, row, last);
    buf.set_cursor(row, 0);
}

// Visual mode's own `>`/`<` -- shifts every line any committed selection
// (plus the active one, already folded into `buf.selections` by the
// caller -- see `surround_selections`'s own doc comment for the same
// "iterate the committed set" shape) touches, whole-line, same linewise-
// regardless-of-shape rule as the Normal-mode operator forms above.
// Iterated directly rather than sorted/reversed the way `delete_
// selections` needs to: inserting/removing leading whitespace at column
// 0 never shifts any other line's own row index, so order can't matter
// here the way it does for a deletion that changes line count.
pub(crate) fn indent_selections(buf: &mut TextBuffer) {
    if buf.selections.is_empty() {
        return;
    }
    let leftmost_row = buf.selections.iter().map(|r| r.from.0).min().unwrap();
    for range in buf.selections.clone() {
        indent_rows(buf, range.from.0, range.to.0);
    }
    buf.set_cursor(leftmost_row, 0);
}

pub(crate) fn outdent_selections(buf: &mut TextBuffer) {
    if buf.selections.is_empty() {
        return;
    }
    let leftmost_row = buf.selections.iter().map(|r| r.from.0).min().unwrap();
    for range in buf.selections.clone() {
        outdent_rows(buf, range.from.0, range.to.0);
    }
    buf.set_cursor(leftmost_row, 0);
}

// `x`: deletes up to `count` characters starting at the cursor, clamped
// to the end of the line -- vim's own primitive (see `vimkeys::
// apply_delete_forward`'s own doc comment on why this isn't quite
// reducible to `d{count}l`).
pub(crate) fn delete_char_forward(buf: &mut TextBuffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
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
pub(crate) fn add_surround(buf: &mut TextBuffer, target: SurroundTarget, ch: char) {
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
pub(crate) fn delete_surround(buf: &mut TextBuffer, ch: char) {
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
pub(crate) fn change_surround(buf: &mut TextBuffer, ch: char, replacement: char) {
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
pub(crate) fn surround_selections(buf: &mut TextBuffer, ch: char) {
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
pub(crate) fn replace_char(buf: &mut TextBuffer, ch: char, count: usize) {
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
pub(crate) fn adjust_number(buf: &mut TextBuffer, delta: i64) {
    let Some(m) = motion::find_number(buf, buf.cursor()) else {
        return;
    };
    let replacement = motion::apply_number_delta(&m, delta);
    let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: m.from, to: m.to };
    buf.delete_range(&range);
    buf.insert_text(m.from, &replacement);
    buf.set_cursor(m.from.0, m.from.1 + replacement.chars().count() - 1);
}

pub(crate) fn case_kind_for_op(op: Op) -> motion::CaseKind {
    match op {
        Op::Lowercase => motion::CaseKind::Lower,
        Op::Uppercase => motion::CaseKind::Upper,
        Op::CaseToggle => motion::CaseKind::Toggle,
        Op::Yank | Op::Delete | Op::Change | Op::Indent | Op::Outdent => {
            unreachable!("case_kind_for_op is only ever called for Op::Lowercase/Uppercase/CaseToggle")
        }
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

pub(crate) fn case_operator_motion(buf: &mut TextBuffer, m: motion::Motion, count: Option<usize>, kind: motion::CaseKind) {
    let Some(range) = motion::motion_range(buf, m, count) else {
        return;
    };
    case_operator_range(buf, &range, kind);
}

pub(crate) fn case_operator_lines(buf: &mut TextBuffer, count: Option<usize>, kind: motion::CaseKind) {
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
pub(crate) fn toggle_case(buf: &mut TextBuffer, count: usize) {
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
pub(crate) fn put(buf: &mut TextBuffer, registers: &mut Registers, before: bool, count: Option<usize>, register: Option<char>) {
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
pub(crate) fn redirect_cw_to_ce(buf: &TextBuffer, m: &motion::Motion) -> motion::Motion {
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
pub(crate) fn open_line(buf: &mut TextBuffer, above: bool) {
    let (row, _) = buf.cursor();
    if above {
        buf.insert_text((row, 0), "\n");
        buf.set_cursor(row, 0);
    } else {
        let len = buf.line_len(row);
        buf.insert_text((row, len), "\n");
    }
}

pub(crate) fn resolve_insert_start(buf: &mut TextBuffer, cmd: InsertCmd) {
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

// The typing loop once Insert mode has actually started (cursor already
// positioned by `resolve_insert_start`, or -- for `c{motion}`/`cc`/
// Visual `c` -- already sitting exactly where a delete just left it, no
// repositioning needed at all). Always returns to Normal mode in the
// caller (the one real Normal-mode loop, `repl.rs`'s
// `run_normal_mode_navigation`) once this returns -- Escape/Ctrl-C/
// Ctrl+Space all mean exactly the same thing here (leave Insert mode),
// and EOF does too (nothing sensible to keep typing into). Unlike a live
// shell prompt's own Insert-mode-to-Normal-mode transition, there's no
// second "which mode am I actually in" question to answer afterward:
// this pane's own Normal mode is already the one true one, so Ctrl+Space
// has nothing further to detach *to* -- it used to jump straight past
// Normal mode to inspecting/switching other windows in one keystroke;
// now it just means "stop typing," and reaching another window from here
// is `<C-w>...` after this returns, same total keystroke count as before.
//
// `replace` (`R` -- see `KeyOutcome::EnterReplace`'s own doc comment)
// swaps two of this loop's own arms (`Key::Char`/`Key::Backspace`) from
// their ordinary Insert-mode behavior to overtype instead -- everything
// else (motion keys, Enter, exit) behaves identically either way, which
// is why this is a flag on the one shared loop rather than a second copy
// of it.
// term_rows/term_cols are plain by-value snapshots, not &mut usize, even
// though on_idle (built by every call site) closes over the *same*
// underlying term_rows/term_cols one level up as &mut usize to drive
// service_background_jobs's own resize handling -- a second, direct
// &mut borrow of that same storage here would conflict with the
// closure's borrow of it, since both live for this whole call. The
// practical effect: a resize mid-insert-session is still fully applied
// (session screen, job ptys, the caller's own term_rows/term_cols) by
// the time this function returns, but this function's own rendering
// keeps using the size it started with until then -- the same "next
// natural redraw" caveat this codebase already accepts elsewhere for a
// loop that's actively blocked on a keystroke.
// Lines scrolled per mouse wheel notch, here and in repl.rs's own
// Normal-mode navigation wheel handling -- matches most terminals'/
// editors' own default wheel granularity (a single line per notch reads
// as sluggish for a fast scroll).
pub(crate) const MOUSE_WHEEL_LINES: usize = 3;

// `extra_cursors`: every position besides `buf.cursor()` itself that
// this same Insert-mode session must also type into -- multi-selection
// `c`'s own "replace every one of these with what I'm about to type"
// (repl.rs's own Key::Char('c') arm passes each deleted selection's own
// gap, other than the one `buf.cursor()` already sits on; every other
// caller passes `&[]`, the ordinary single-cursor case this always was
// before). `cursors[0]` is kept mirroring `buf.cursor()` throughout --
// navigation (Left/Right/Up/Down/PageUp/PageDown/wheel) only ever moves
// that one, matching real editors' own multi-cursor convention that
// navigating mid-edit doesn't drag every other cursor along with it;
// only an actual mutation (Enter/Tab/Backspace/a typed char) replicates
// across the whole set, via apply_insert_to_all/apply_backspace_to_all.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_insert_mode(
    buf: &mut TextBuffer,
    vk: &mut VimKeys,
    rect: Rect,
    registers: &mut Registers,
    on_idle: &mut dyn FnMut(),
    replace: bool,
    term_rows: usize,
    term_cols: usize,
    color_overrides: Option<&highlight::ColorOverrides>,
    extra_cursors: &[(usize, usize)],
) -> io::Result<()> {
    let mode = if replace { EditorMode::Replace } else { EditorMode::Insert };
    let mut cursors: Vec<(usize, usize)> = std::iter::once(buf.cursor()).chain(extra_cursors.iter().copied()).collect();
    render_editor_frame(buf, vk, mode, rect, term_rows, term_cols, color_overrides);
    // `"."`'s own accumulator for this session -- see `Registers::
    // set_last_insert`'s own doc comment. Best-effort: a Backspace just
    // pops the most recently accumulated character regardless of whether
    // it's actually erasing something typed *this* session or older
    // pre-existing text it backed into -- real vim tracks that
    // distinction precisely; this doesn't.
    let mut inserted = String::new();
    loop {
        // Goes through `vk.next_key` (not a bare `read_key_idle`) so a
        // macro recorded/replayed from Normal mode -- see its own doc
        // comment -- still works across a full Insert-mode excursion:
        // `run_normal_mode_navigation` calls this function inline, with
        // the same `vk`, so nothing here needs its own bookkeeping.
        let key = match vk.next_key(|| editor::read_key_idle(on_idle))? {
            Some(k) => k,
            None => {
                buf.set_mark('^', buf.cursor());
                registers.set_last_insert(inserted);
                return Ok(());
            }
        };
        match key {
            // `^`: vim's own name for this mark (`:help '^`) -- wherever
            // the cursor was the last time Insert mode ended, however it
            // ended. `gi` reads it back (see resolve_insert_start's own
            // `LastInsertPos` arm).
            //
            // Ctrl-C is a plain alias for Escape throughout this editor
            // (see the identical treatment in the unified Normal mode's
            // own Visual-mode handling); Ctrl+Space is too, here (see
            // this function's own doc comment).
            Key::CtrlSpace | Key::Escape | Key::CtrlC => {
                // The `'^'` mark records the exact position insert mode
                // left off at (so `gi` resumes appending from the same
                // spot, one-past-the-last-character included) -- captured
                // *before* the Normal-mode-only clamp just below, which
                // must not affect it.
                buf.set_mark('^', buf.cursor());
                registers.set_last_insert(inserted);
                // Normal mode's cursor can never sit *after* the last
                // character of a line (unlike Insert mode, which allows
                // that to append) -- clamp back by one column on the way
                // out, same convention toggle_case's own post-edit clamp
                // already uses. A wholly empty line (line_len 0) has
                // nothing to clamp to but column 0, which set_cursor's
                // own min() already leaves it at.
                let (row, col) = buf.cursor();
                let len = buf.line_len(row);
                if col >= len && len > 0 {
                    buf.set_cursor(row, len - 1);
                }
                return Ok(());
            }
            Key::Enter => {
                apply_insert_to_all(buf, &mut cursors, "\n");
                buf.set_cursor(cursors[0].0, cursors[0].1);
                inserted.push('\n');
            }
            // Tab inserts spaces up to the next INDENT_WIDTH boundary
            // (vim's own `expandtab` behavior, always on -- see
            // INDENT_WIDTH's own doc comment on why there's no `:set` to
            // choose a literal tab character instead). Never overtypes
            // even in Replace mode, same as Enter just above: a literal
            // tab byte would also break this editor's own one-char-per-
            // column rendering model (build_editor_frame/render_row treat
            // every buffer char as exactly one terminal column, with no
            // tab-stop-aware expansion anywhere in that pipeline).
            Key::Tab => {
                let (_, col) = buf.cursor();
                let width = INDENT_WIDTH - (col % INDENT_WIDTH);
                let spaces = " ".repeat(width);
                apply_insert_to_all(buf, &mut cursors, &spaces);
                buf.set_cursor(cursors[0].0, cursors[0].1);
                inserted.push_str(&spaces);
            }
            // Replace mode's own Backspace: known simplification -- steps
            // the cursor back without restoring the character it walks
            // back over (real vim remembers and restores each one) and
            // never crosses a line boundary backward, unlike ordinary
            // Insert mode's own version just below. Never combined with
            // extra_cursors in practice (only `c`, always plain Insert,
            // ever passes any) -- moves `cursors[0]` directly rather than
            // going through apply_backspace_to_all's own multi-cursor
            // machinery, since there's never more than one cursor here.
            Key::Backspace if replace => {
                let (row, col) = buf.cursor();
                if col > 0 {
                    buf.set_cursor(row, col - 1);
                }
                cursors[0] = buf.cursor();
                inserted.pop();
            }
            Key::Backspace => {
                apply_backspace_to_all(buf, &mut cursors);
                buf.set_cursor(cursors[0].0, cursors[0].1);
                inserted.pop();
            }
            Key::Left => {
                motion::apply_motion(buf, motion::Motion::Left, None);
                cursors[0] = buf.cursor();
            }
            Key::Right => {
                // `Motion::Right` clamps at the last real character (its
                // ordinary Normal-mode meaning); Insert mode's cursor is
                // allowed one column past that (where the next typed
                // char would land), so this moves it directly rather
                // than going through the clamped motion.
                let (row, col) = buf.cursor();
                buf.set_cursor(row, (col + 1).min(buf.line_len(row)));
                cursors[0] = buf.cursor();
            }
            Key::Up => {
                motion::apply_motion(buf, motion::Motion::Up, None);
                cursors[0] = buf.cursor();
            }
            Key::Down => {
                motion::apply_motion(buf, motion::Motion::Down, None);
                cursors[0] = buf.cursor();
            }
            // Same physical-key-as-Ctrl-F/Ctrl-B convention as Normal
            // mode's own vimkeys.rs handling -- real vim honors
            // PageUp/PageDown in Insert mode too, not just Normal.
            Key::PageDown => {
                motion::apply_motion(buf, motion::Motion::PageDown, None);
                cursors[0] = buf.cursor();
            }
            Key::PageUp => {
                motion::apply_motion(buf, motion::Motion::PageUp, None);
                cursors[0] = buf.cursor();
            }
            // Mouse wheel: scrolls the view without otherwise touching
            // the cursor (Motion::ScrollLineDown/Up's own behavior --
            // only nudges the cursor back into view if scrolling would
            // otherwise carry it off-screen), same as Ctrl-E/Ctrl-Y
            // already do in Normal mode. MOUSE_WHEEL_LINES lines per
            // notch, not 1 -- matches most terminals'/editors' own
            // default wheel granularity.
            Key::Mouse(ev) if ev.is_scroll_down() => {
                motion::apply_motion(buf, motion::Motion::ScrollLineDown, Some(MOUSE_WHEEL_LINES));
                cursors[0] = buf.cursor();
            }
            Key::Mouse(ev) if ev.is_scroll_up() => {
                motion::apply_motion(buf, motion::Motion::ScrollLineUp, Some(MOUSE_WHEEL_LINES));
                cursors[0] = buf.cursor();
            }
            Key::Char(c) => {
                let (row, col) = buf.cursor();
                // Replace mode overwrites the character already at the
                // cursor, if there is one -- deleting it first, then
                // inserting, naturally extends the line once the cursor
                // reaches its end (nothing left there to overwrite),
                // matching real vim's own `R` behavior at end of line.
                // Same "never combined with extra_cursors" reasoning as
                // Replace's own Backspace arm above.
                if replace && col < buf.line_len(row) {
                    let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (row, col), to: (row, col) };
                    buf.delete_range(&range);
                    let mut b = [0u8; 4];
                    buf.insert_text((row, col), c.encode_utf8(&mut b));
                    cursors[0] = buf.cursor();
                } else {
                    let mut b = [0u8; 4];
                    apply_insert_to_all(buf, &mut cursors, c.encode_utf8(&mut b));
                    buf.set_cursor(cursors[0].0, cursors[0].1);
                }
                inserted.push(c);
            }
            _ => {}
        }
        scroll_to_show_cursor(buf, editor_content_cols(buf, rect));
        render_editor_frame(buf, vk, mode, rect, term_rows, term_cols, color_overrides);
    }
}

// Inserts `text` (never containing more than one '\n', and only ever
// exactly "\n" alone when it does -- Enter is the only run_insert_mode
// caller that ever inserts a newline, and always just that one
// character) at every position in `cursors`, replicating multi-
// selection `c`'s own "type once, it lands everywhere" behavior (see
// run_insert_mode's own doc comment on `extra_cursors`). Processes them
// in ascending (row, col) order -- ascending, not descending, because
// this is an *insertion*: text appearing at an earlier position only
// ever pushes a later, not-yet-touched one further away, never
// invalidates its own coordinates, so the earliest can always go first
// and every later one's position is patched to account for it before
// its own turn comes (the exact opposite ordering delete_selections'
// own doc comment establishes for a *removal*, and apply_backspace_to_
// all below, for the same underlying reason run the other way).
fn apply_insert_to_all(buf: &mut TextBuffer, cursors: &mut [(usize, usize)], text: &str) {
    let is_newline = text == "\n";
    let width = text.chars().count();
    let mut order: Vec<usize> = (0..cursors.len()).collect();
    order.sort_by_key(|&i| cursors[i]);
    for step in 0..order.len() {
        let i = order[step];
        let (row, col) = cursors[i];
        cursors[i] = buf.insert_text((row, col), text);
        for &j in &order[step + 1..] {
            let (r, c) = cursors[j];
            if is_newline {
                // A not-yet-processed cursor on the *same* row, at or
                // after the split column, moves onto the newly created
                // row instead, its own column now relative to that new
                // line's own start; anything on a strictly later row
                // just shifts down by the one new line.
                if r == row && c >= col {
                    cursors[j] = (r + 1, c - col);
                } else if r > row {
                    cursors[j] = (r + 1, c);
                }
            } else if r == row && c >= col {
                cursors[j] = (r, c + width);
            }
        }
    }
}

// Backspace, replicated across every tracked cursor -- see
// apply_insert_to_all's own doc comment for the shared shape/reasoning;
// this is its deletion counterpart, so it processes furthest-first
// instead (removing something at a later position can never invalidate
// an earlier, not-yet-processed cursor's own coordinates, but the
// reverse isn't true -- the same rule delete_selections already
// established for a whole visual selection, applied here one character
// at a time instead). A cursor already at the very start of the buffer
// (row 0, col 0) is simply left alone, matching ordinary single-cursor
// Backspace's own ordinary Insert-mode arm doing nothing there either.
fn apply_backspace_to_all(buf: &mut TextBuffer, cursors: &mut [(usize, usize)]) {
    let mut order: Vec<usize> = (0..cursors.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(cursors[i]));
    for step in 0..order.len() {
        let i = order[step];
        let (row, col) = cursors[i];
        if col > 0 {
            let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, col - 1), to: (row, col) };
            buf.delete_range(&range);
            cursors[i] = (row, col - 1);
            for &j in &order[step + 1..] {
                let (r, c) = cursors[j];
                if r == row && c >= col {
                    cursors[j] = (r, c - 1);
                }
            }
        } else if row > 0 {
            let prev_len = buf.line_len(row - 1);
            let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row - 1, prev_len), to: (row, 0) };
            buf.delete_range(&range);
            cursors[i] = (row - 1, prev_len);
            for &j in &order[step + 1..] {
                let (r, c) = cursors[j];
                if r > row {
                    cursors[j] = (r - 1, c);
                } else if r == row {
                    // A degenerate overlap (two distinct cursors both at
                    // column 0 of the same row) -- this row is gone,
                    // joined into row - 1, so fold this one in there
                    // too, same as the one actually being backspaced.
                    cursors[j] = (r - 1, prev_len + c);
                }
            }
        }
    }
}

// `"%"`: vim's own current-filename register -- refreshed here (a no-op
// if there's no path, e.g. an unnamed buffer that still hasn't been
// written anywhere) whenever the buffer's own path could plausibly have
// just changed (repl.rs's `run_edit_frame`, once when it starts driving a
// session, and `run_command_mode`'s own `w`/`wq`/`x` handling, since `:w
// newname` can name a previously-unnamed buffer).
pub(crate) fn set_last_filename(buf: &TextBuffer, registers: &mut Registers) {
    if let Some(path) = buf.path() {
        registers.set_last_filename(path.to_string_lossy().into_owned());
    }
}

// All of this pane's own rect -- the mode-line lives in the terminal's
// own global status row now (repl::render_global_status_row), not
// carved out of this pane's own rect. `.max(1)`: a degenerate
// zero-height rect still gets *some* content rather than a panicking
// view.
fn editor_content_rows(rect: Rect) -> usize {
    rect.rows.max(1)
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
    // "recording @a" while `q{reg}` is active -- mirrors repl.rs's own
    // `normal_mode_status_left`, the `ScreenBuffer`-backed twin of this
    // status line.
    let recording = vk.is_recording().map(|r| format!("recording @{r}  ")).unwrap_or_default();
    let mut left = if !pending.is_empty() {
        format!("{recording}{label} {pending}")
    } else {
        let last = vk.last_motion_display();
        if !last.is_empty() { format!("{recording}{label} [{last}]") } else { format!("{recording}{label}") }
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

// The (start, end) char-index range `range` covers on this one `line`,
// if any, already rebased to be relative to `start_char` (the char
// index of render_row's own viewport-window-local `chars[0]` -- see
// that function's own doc comment for why this is a char index, not a
// display column, despite `chars` itself being a *column*-bounded
// window: `chars` is still built by walking consecutive char indices
// starting at `start_char`, just stopping early once the column budget
// runs out, so every position in it corresponds 1:1 to a real char
// index offset by `start_char` -- never a column) and clamped to
// `cols` -- i.e. directly usable as an index into that array, unlike
// spans_for_line/diagnostic_spans_for_line just below, which stay
// line-absolute and get shifted separately inside render_row itself
// (see its own doc comment for why the two don't share one convention:
// a Linewise selection's own `(0, cols)` is *already* viewport-local by
// definition -- it means "the whole visible row," not "the whole
// line's own text," so shifting it by `start_char` again would be
// wrong). Once mirrored exactly by repl.rs's own `selection_columns_
// in_line` (see its doc comment); that one stays offset-less on purpose
// -- ScreenBuffer never scrolls horizontally (Buffer::viewport_left's
// own doc comment).
fn selection_columns_in_line(range: &motion::MotionRange, line: usize, start_char: usize, cols: usize) -> Option<(usize, usize)> {
    if line < range.from.0 || line > range.to.0 {
        return None;
    }
    if range.shape == motion::MotionShape::Linewise {
        return Some((0, cols));
    }
    let start = if line == range.from.0 { range.from.1.saturating_sub(start_char) } else { 0 };
    let end = if line == range.to.0 { (range.to.1 + 1).saturating_sub(start_char).min(cols) } else { cols };
    Some((start, end))
}

// One column of the gutter drawn to the left of a line's own content.
// The diagnostic marker and line numbers are the only columns today, but
// the shape anticipates the same slot holding blame/coverage columns
// later -- each is just another (width, render) pair appended to
// GUTTER_COLUMNS, with no change needed anywhere else: build_editor_frame
// only ever asks "how wide is the whole gutter" and "render line N's
// gutter cells", never which columns exist. `render` returns the *fully
// styled* cell text (its own SGR codes, if it wants color/dim) so each
// column stays free to look however it needs to -- a future diff column
// wants red/green, not the line-number column's dim gray -- rather than
// this shared machinery imposing one style on all of them. `None` means
// "blank, unstyled" -- used for filler rows past the buffer's own last
// line, matching how render_row's own caller already leaves those rows'
// content blank too. `render` also takes `starts` (line_starts's own
// prefix-sum, computed once per frame by build_editor_frame) since the
// diagnostic column needs it to translate buf.diagnostics's whole-buffer
// char offsets into "does this line's own span intersect one" -- the
// line-number column ignores it, same as any future column that doesn't
// need it.
struct GutterColumn {
    width: fn(&TextBuffer) -> usize,
    render: fn(buf: &TextBuffer, starts: &[usize], line: usize, width: usize) -> Option<String>,
}

static GUTTER_COLUMNS: &[GutterColumn] = &[
    GutterColumn { width: breakpoint_column_width, render: render_breakpoint_cell },
    GutterColumn { width: blame_column_width, render: render_blame_cell },
    GutterColumn { width: diff_column_width, render: render_diff_cell },
    GutterColumn { width: diagnostic_column_width, render: render_diagnostic_cell },
    GutterColumn { width: line_number_width, render: render_line_number_cell },
];

fn total_gutter_width(buf: &TextBuffer) -> usize {
    GUTTER_COLUMNS.iter().map(|col| (col.width)(buf)).sum()
}

fn render_gutter(out: &mut String, buf: &TextBuffer, starts: &[usize], line: usize) {
    for col in GUTTER_COLUMNS {
        let width = (col.width)(buf);
        match (col.render)(buf, starts, line, width) {
            Some(cell) => out.push_str(&cell),
            None => out.push_str(&" ".repeat(width)),
        }
    }
}

// `:git blame`'s own gutter column: `short_commit` (8) + ' ' + `date`
// (10, "YYYY-MM-DD") + ' ' + author (grows to fit the widest one actually
// present, capped at BLAME_AUTHOR_MAX_WIDTH) + a trailing separator
// space. Collapses to 0 -- no reserved space at all -- when blame isn't
// currently toggled on for this buffer, unlike diagnostic_column_width's
// always-reserved 2 columns (a stable sign column vim users expect to
// never shift): blame is wide enough that reserving it unconditionally
// would waste most of the window's width for the common case of it being
// off, and unlike the sign column, "on" is a deliberate, infrequent user
// toggle rather than something that comes and goes as they type.
const BLAME_AUTHOR_MAX_WIDTH: usize = 16;

fn blame_column_width(buf: &TextBuffer) -> usize {
    let Some(blame) = &buf.blame else { return 0 };
    let author_width = blame.iter().map(|b| b.author.chars().count()).max().unwrap_or(0).clamp(1, BLAME_AUTHOR_MAX_WIDTH);
    8 + 1 + 10 + 1 + author_width + 1
}

fn render_blame_cell(buf: &TextBuffer, _starts: &[usize], line: usize, width: usize) -> Option<String> {
    let blame = buf.blame.as_ref()?;
    let entry = blame.get(line)?;
    let author_width = width.saturating_sub(8 + 1 + 10 + 1 + 1);
    let author: String = entry.author.chars().take(author_width).collect();
    Some(format!("\x1b[2m{} {} {:<aw$} \x1b[0m", entry.short_commit, entry.date, author, aw = author_width))
}

// `:git diff`'s own gutter column: a single marker glyph + one padding
// space, same fixed-2 shape as diagnostic_column_width just below --
// unlike blame's column, a diff mark never needs more than one glyph to
// say what it means. Collapses to 0 when diff isn't currently toggled on
// for this buffer, same reasoning as blame_column_width above (a
// deliberate, infrequent toggle, not something that comes and goes on
// its own the way the diagnostic sign column's own contents do).
fn diff_column_width(buf: &TextBuffer) -> usize {
    if buf.diff.is_some() { 2 } else { 0 }
}

fn render_diff_cell(buf: &TextBuffer, _starts: &[usize], line: usize, _width: usize) -> Option<String> {
    let mark = buf.diff.as_ref()?.get(&line)?;
    let (color, glyph) = match mark {
        crate::git::DiffMark::Added => ("32", '+'),
        crate::git::DiffMark::Changed => ("33", '~'),
        crate::git::DiffMark::Removed => ("31", '-'),
    };
    Some(format!("\x1b[{color}m{glyph}\x1b[0m "))
}

// A fixed 2 columns (marker glyph + one padding space) -- vim's own
// `:set signcolumn` convention, not something that needs to grow with
// the buffer the way the line-number column does.
// `bish tool debug`'s own breakpoint column -- collapses to zero width
// when `buf.breakpoints` is empty (the common case for an ordinary `e`-
// opened buffer, which never touches it at all), same convention blame/
// diff already use, unlike the diagnostic column's always-reserved 2.
fn breakpoint_column_width(buf: &TextBuffer) -> usize {
    if buf.breakpoints.is_empty() { 0 } else { 2 }
}

// 1-based, matching how the debugger itself (and ListItem::line) numbers
// lines -- `line` here is the 0-indexed row every other GutterColumn
// uses.
fn render_breakpoint_cell(buf: &TextBuffer, _starts: &[usize], line: usize, _width: usize) -> Option<String> {
    if line >= buf.line_count() || !buf.breakpoints.contains(&(line + 1)) {
        return None;
    }
    Some("\x1b[1;31m\u{25cf}\x1b[0m ".to_string())
}

fn diagnostic_column_width(_buf: &TextBuffer) -> usize {
    2
}

// Whether any diagnostic's own char-offset range (whole-buffer text
// offsets, same convention buffer_highlight_spans's own spans use)
// intersects this one line's span -- same predicate diagnostic_spans_
// for_line uses per-column below, just answering "any at all" instead of
// "which columns" for the coarser gutter marker. Only the worst-severity
// mark would matter once Severity grows past its current one variant;
// diagnostic_style below is already written to make that a one-line
// change when it does.
fn line_has_diagnostic(buf: &TextBuffer, starts: &[usize], line: usize) -> bool {
    let line_start = starts[line];
    let line_end = line_start + buf.line_len(line);
    buf.diagnostics.iter().any(|d| d.start < line_end && d.end > line_start)
}

fn render_diagnostic_cell(buf: &TextBuffer, starts: &[usize], line: usize, _width: usize) -> Option<String> {
    if line >= buf.line_count() || !line_has_diagnostic(buf, starts, line) {
        return None;
    }
    Some("\x1b[33m\u{25cf}\x1b[0m ".to_string())
}

// Vim's own gutter-width convention: as many digits as the buffer's last
// line number needs, plus one trailing space of padding before the
// buffer's own content starts. Grows dynamically as the buffer gains
// lines (matching vim), rather than reserving a fixed width up front.
fn line_number_width(buf: &TextBuffer) -> usize {
    buf.line_count().to_string().len() + 1
}

fn render_line_number_cell(buf: &TextBuffer, _starts: &[usize], line: usize, width: usize) -> Option<String> {
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
// comment -- BashHighlighter is still the only one), and are exactly
// where a new `FileType` variant would slot in below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileType {
    Bash,
    Unknown,
}

fn file_type(buf: &TextBuffer) -> FileType {
    match buf.path().and_then(|p| p.extension()) {
        Some(ext) if ext == "bash" => FileType::Bash,
        _ => FileType::Unknown,
    }
}

fn is_bash_file(buf: &TextBuffer) -> bool {
    file_type(buf) == FileType::Bash
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
// default() (no cwd, no known_functions) still stands in for the
// highlighter's own *classification* step: same "no context to offer"
// choice command mode's own colon-line already makes, since nothing
// here has a live Shell to pull those from -- Flag/Subcommand/Link/
// InvalidCommand refinements that need them simply don't fire, same as
// there. `color_overrides` (bishopt's own syn_col_* -- see bishedit::
// highlight::ColorOverrides/SYN_COL_OPTIONS) is a separate, later step
// (picking a *color* for an already-classified span, not classifying
// it), threaded in from wherever a live Shell to read it from actually
// exists: repl.rs's own run_edit_frame, one level up from every caller
// of this whole chain.
//
// Not a full fix for every multi-line case: `next_span`'s own doc
// comment (this same module) documents a pre-existing lexer position-
// tracking gap for a heredoc body that itself contains a $VAR/$(...)
// expansion -- content *after* such a heredoc in the same buffer can
// still come out mis-highlighted. Narrower and pre-existing either way,
// not something this change introduces or could fix without touching
// lexer.rs's own heredoc-body capture.
fn buffer_highlight_spans(buf: &TextBuffer, color_overrides: Option<&highlight::ColorOverrides>) -> Vec<StyledSpan> {
    if !is_bash_file(buf) {
        return Vec::new();
    }
    let text = buffer_text(buf);
    BashHighlighter
        .highlight(&text, HighlightContext::default())
        .into_iter()
        .map(|s| {
            let (fg, attrs) = highlight::resolve_style(s.kind, color_overrides);
            StyledSpan { start: s.start, end: s.end, fg, attrs }
        })
        .collect()
}

// `:diag`'s own worker (see repl.rs's run_command_mode, the one caller):
// runs every diagnose tool configured for this buffer's language against
// the *whole* buffer text, same `buffer_text`/is_bash_file gate as
// buffer_highlight_spans -- for exactly the same reason (a multi-line
// construct needs to be seen as one thing, and `.bash` is the only
// recognized language today). Unlike buffer_highlight_spans this isn't
// called on every redraw: `:diag` is an explicit, user-triggered check,
// not a live-typing overlay, so there's no per-keystroke cost to worry
// about and the result is cached on TextBuffer::diagnostics instead of
// recomputed here (see that field's own doc comment for why it also
// self-clears the moment the buffer it describes actually changes).
//
// A plain `Vec<&dyn Linter>` rather than BashLinter called directly --
// lint.rs's own doc comment already frames Linter as "the one shared
// core behind bish tool check, in-editor squiggles, and bish tool
// lsp-server," and this feature's own ask was explicit that one language
// may eventually run more than one diagnose tool over the same buffer
// (a style linter alongside a correctness one, say); concatenating each
// tool's own findings here is what makes adding a second one later just
// "add it to this list," not a structural change.
pub(crate) fn diagnose_buffer(buf: &TextBuffer) -> Vec<lint::Diagnostic> {
    if !is_bash_file(buf) {
        return Vec::new();
    }
    let text = buffer_text(buf);
    let linters: [&dyn Linter; 1] = [&BashLinter];
    linters.iter().flat_map(|l| l.check(&text)).collect()
}

// A `Diagnostic`'s own flat char offset (into `buffer_text`'s joined
// string -- the same addressing `lint::Diagnostic`/`Fix` use throughout)
// translated into this buffer's real `(line, col)`, via the same
// `line_starts` prefix-sum table `diagnostic_spans_for_line`'s own
// underline rendering already builds fresh per redraw. Used by the
// diagnostics pane (repl.rs) to jump the cursor to a selected problem,
// and by `apply_fix` below to know where to splice its replacement.
pub(crate) fn diagnostic_position(buf: &TextBuffer, offset: usize) -> (usize, usize) {
    let starts = line_starts(buf);
    let line = match starts.binary_search(&offset) {
        Ok(l) => l,
        Err(l) => l.saturating_sub(1),
    };
    let line = line.min(buf.line_count().saturating_sub(1));
    let col = offset.saturating_sub(starts[line]).min(buf.line_len(line));
    (line, col)
}

// Splices `diagnostic`'s own `Fix` (if it has one) into `buf` --
// `false`, no-op, if it doesn't. The same two primitives `tool.rs`'s own
// CLI-only `apply_fixes` (a whole sorted batch, working on a raw
// string) uses conceptually, just against a real `TextBuffer` and for
// exactly one diagnostic: `delete_range` already clears `buf.
// diagnostics` as part of any real edit (same rule any other change
// obeys), so the caller re-running `diagnose_buffer` afterward is what
// actually produces a fresh, correctly-offset list -- patching the old
// one in place isn't worth the risk once a splice has shifted
// everything after it.
pub(crate) fn apply_fix(buf: &mut TextBuffer, diagnostic: &lint::Diagnostic) -> bool {
    let Some(fix) = &diagnostic.fix else { return false };
    let from = diagnostic_position(buf, fix.start);
    let to = diagnostic_position(buf, fix.end);
    let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from, to };
    buf.delete_range(&range);
    buf.insert_text(from, &fix.replacement);
    true
}

// The batch counterpart to `apply_fix` above -- what a pre-save hook
// (below) needs, since a hook's whole point is applying every fix it
// found in one go, not one at a time. Same sort-by-descending-start-then
// -splice shape as tool.rs's own `apply_fixes`, just calling `apply_fix`
// against the real buffer for each one instead of splicing a raw string:
// every diagnostic's `start`/`end` are offsets into the *original*
// `buffer_text(buf)`, and processing right-to-left means every edit
// still to come sits entirely before the point already spliced, so an
// as-yet-unapplied offset never needs adjusting for one that already
// landed. The overlap guard is defensive, matching tool.rs's own -- no
// current hook produces overlapping fixes, but a corrupted splice would
// be a much worse failure mode than silently skipping one.
fn apply_all_fixes(buf: &mut TextBuffer, diagnostics: &[lint::Diagnostic]) -> usize {
    let mut candidates: Vec<&lint::Diagnostic> = diagnostics.iter().filter(|d| d.fix.is_some()).collect();
    candidates.sort_by_key(|d| std::cmp::Reverse(d.fix.as_ref().unwrap().start));
    let mut applied = 0;
    let mut last_applied_start = usize::MAX;
    for d in candidates {
        let fix = d.fix.as_ref().unwrap();
        if fix.end > last_applied_start {
            continue;
        }
        apply_fix(buf, d);
        last_applied_start = fix.start;
        applied += 1;
    }
    applied
}

// One filetype's pre-save hook: given the buffer's whole text, the
// Diagnostics (with Fixes) it wants applied before the write hits disk --
// the same shape `Linter::check` already uses, so a hook that would also
// make sense as a live diagnostic doesn't need a second implementation.
// `Result`, not a plain `Vec`, because unlike a Linter a hook may refuse
// to touch content it can't make sense of at all (`BashFormatter::
// check`'s own doc comment: it bails on a parse error rather than guess)
// -- `Err` just means "leave the file alone," same as a failed `bish
// tool format` leaving its target untouched.
type PreSaveHook = fn(&str) -> Result<Vec<lint::Diagnostic>, String>;

fn bash_format_hook(text: &str) -> Result<Vec<lint::Diagnostic>, String> {
    Ok(strip_buffer_implicit_trailing_newline(text, BashFormatter.check(text)?))
}

// A hook's own Diagnostics assume the same "exactly one trailing
// newline is real content" convention `bish tool format`'s CLI writer
// does -- `BashFormatter::check`'s own EOF-gap rule always intends
// literally "\n" there, since it's producing a plain string meant to be
// written to a file as-is. `TextBuffer::save` already adds that
// trailing newline itself on every write (see its own doc comment: one
// line per row, joined by "\n", plus one more at the end) -- lines
// never store a trailing newline as their own content. Applied
// literally, a fix reaching all the way to the end of the buffer's text
// that still ends in '\n' would double it up as a real extra blank last
// line (`buf.insert_text` splits on every '\n' it's given, with no idea
// one of them was only ever meant to represent `save`'s own implicit
// one). Trimmed once here, right where the hook already knows it's
// producing Diagnostics headed for a `TextBuffer`, rather than teaching
// `BashFormatter` itself two different EOF conventions.
//
// The stripped fix can end up start == end with an empty replacement --
// a script whose buffer content already matches canonical layout exactly
// (`buffer_text` just never carries the trailing newline this diagnostic
// otherwise exists to add). That diagnostic is dropped outright rather
// than left in as a no-op `Fix`: `apply_fix`/`apply_all_fixes` count any
// `Some(fix)` as "applied" regardless of whether it changes anything, so
// leaving it in would make an already-formatted buffer misreport as
// `FormatOutcome::Formatted` instead of `AlreadyFormatted`.
fn strip_buffer_implicit_trailing_newline(text: &str, mut diagnostics: Vec<lint::Diagnostic>) -> Vec<lint::Diagnostic> {
    let end = text.chars().count();
    let is_eof_newline_fix = diagnostics.last().and_then(|d| d.fix.as_ref()).is_some_and(|f| f.end == end && f.replacement.ends_with('\n'));
    if !is_eof_newline_fix {
        return diagnostics;
    }
    let fix = diagnostics.last_mut().unwrap().fix.as_mut().unwrap();
    fix.replacement.pop();
    if fix.start == fix.end && fix.replacement.is_empty() {
        diagnostics.pop();
    }
    diagnostics
}

// Which hooks run before a save, by filetype -- bash is the only
// filetype recognized at all today (`file_type`'s own doc comment), so
// it's the only entry with anything in its list. A future filetype adds
// its own arm here; a future bash-only hook (or one shared across every
// filetype, e.g. line-ending normalization) just joins this one slice --
// no change needed to `run_pre_save_hooks` itself either way.
fn pre_save_hooks(ft: FileType) -> &'static [PreSaveHook] {
    match ft {
        FileType::Bash => &[bash_format_hook],
        FileType::Unknown => &[],
    }
}

// Runs one hook against `buf`'s current content and applies whatever
// fixes it reports, returning how many actually landed -- the shared
// core both `run_pre_save_hooks` (silent, every hook in the list) and
// `format_buffer` (`:format`, one hook surfaced with real feedback)
// splice fixes through.
fn run_one_hook(buf: &mut TextBuffer, hook: PreSaveHook) -> Result<usize, String> {
    let text = buffer_text(buf);
    let diagnostics = hook(&text)?;
    Ok(apply_all_fixes(buf, &diagnostics))
}

// `:w`/`:wq`/`:x`'s own worker (repl.rs's run_command_mode, called right
// before `tb.save(...)`): runs every pre-save hook this buffer's
// filetype has, in order, applying whatever fixes each one reports
// straight to `buf` so they land in the same write. Each hook re-reads
// `buffer_text(buf)` fresh rather than sharing one snapshot across the
// whole loop, so a second hook already sees the first one's own edits --
// the same "hooks compose like a pipeline" behavior a real formatter
// followed by a real linter-fixer would need. A hook that errors (an
// unparseable buffer, for bash's own hook) is skipped silently: this
// runs on every save, including ones mid-edit with a script that
// doesn't parse yet, and refusing to save over a syntax error would be
// far more disruptive than just not reformatting it this time.
pub(crate) fn run_pre_save_hooks(buf: &mut TextBuffer) {
    for hook in pre_save_hooks(file_type(buf)) {
        let _ = run_one_hook(buf, *hook);
    }
}

// `:format`/`:fmt`'s own result -- unlike the silent pre-save path
// above, an explicit, user-triggered format command has somewhere to
// put real feedback (repl.rs's command-output overlay/sink_err), so it
// gets to distinguish "nothing to do" from "couldn't even try" instead
// of collapsing both into a no-op.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FormatOutcome {
    Formatted,
    AlreadyFormatted,
    // This filetype has no pre-save hook at all (`pre_save_hooks`
    // returns an empty slice) -- distinct from a hook running and
    // finding nothing to change.
    NotSupported,
    // A hook ran and refused (`BashFormatter::check`'s own parse
    // error, for bash's hook) -- the message is that Err's own text.
    Error(String),
}

// `:format`/`:fmt`'s own worker (repl.rs's run_command_mode): the same
// per-filetype hook list `run_pre_save_hooks` runs silently before every
// save, just triggered by hand and reporting what happened instead of
// swallowing it. Runs every hook for this filetype (today, at most one),
// same "each sees the last one's own edits" pipeline -- but stops at the
// first one that errors, rather than skipping just that one and moving
// on: an explicit command asking to reformat this buffer that silently
// formatted with only half its hooks would be a worse surprise than
// reporting the failure and leaving the buffer exactly as it was before
// this call (every hook here only ever mutates `buf` after fully
// succeeding, so a later hook's error never leaves an earlier hook's own
// edits half-applied).
pub(crate) fn format_buffer(buf: &mut TextBuffer) -> FormatOutcome {
    let hooks = pre_save_hooks(file_type(buf));
    if hooks.is_empty() {
        return FormatOutcome::NotSupported;
    }
    let mut applied = 0;
    for hook in hooks {
        match run_one_hook(buf, *hook) {
            Ok(n) => applied += n,
            Err(e) => return FormatOutcome::Error(e),
        }
    }
    if applied > 0 { FormatOutcome::Formatted } else { FormatOutcome::AlreadyFormatted }
}

// `:git blame`'s own worker (repl.rs's run_command_mode): toggles the
// gutter's blame column off if it's currently on (`Ok(false)`), or runs a
// fresh `git blame` and turns it on (`Ok(true)`) if it's currently off --
// same "one command, two states" shape as vim's own `:GBlame`-style
// plugins, just built in. Blame only ever reflects this buffer's last
// *saved* content (crate::git::blame reads the real file from disk, not
// this in-memory buffer), so a dirty buffer is refused outright rather
// than silently showing blame that doesn't line up with what's on screen
// -- same reasoning `:w`'s own dirty-buffer handling already applies
// elsewhere, just surfaced as an Err here instead of a buffer.is_dirty()
// special case in repl.rs.
pub(crate) fn toggle_git_blame(buf: &mut TextBuffer) -> Result<bool, String> {
    if buf.blame.is_some() {
        buf.blame = None;
        return Ok(false);
    }
    if buf.is_dirty() {
        return Err("buffer has unsaved changes -- save first (blame only reflects what's on disk)".to_string());
    }
    let path = buf.path().ok_or_else(|| "no file name".to_string())?;
    if !crate::git::available() {
        return Err("git executable not found".to_string());
    }
    let path = path.to_path_buf();
    buf.blame = Some(crate::git::blame(&path)?);
    Ok(true)
}

// `:git diff`'s own worker -- same "one command, two states" toggle shape
// and same dirty-buffer/no-path/no-git refusals as toggle_git_blame just
// above (crate::git::diff reads the file on disk too, not this buffer's
// own in-memory content -- see its own doc comment on why there's no
// live-buffer-vs-HEAD diffing yet).
pub(crate) fn toggle_git_diff(buf: &mut TextBuffer) -> Result<bool, String> {
    if buf.diff.is_some() {
        buf.diff = None;
        return Ok(false);
    }
    if buf.is_dirty() {
        return Err("buffer has unsaved changes -- save first (diff only reflects what's on disk)".to_string());
    }
    let path = buf.path().ok_or_else(|| "no file name".to_string())?;
    if !crate::git::available() {
        return Err("git executable not found".to_string());
    }
    let path = path.to_path_buf();
    buf.diff = Some(crate::git::diff(&path)?);
    Ok(true)
}

// `:diff`'s own worker -- unlike `toggle_git_diff` just above, this
// needs no git repository (or even git installed) at all: it answers a
// genuinely different question, "what have I typed in this buffer
// since I last saved" rather than "what's changed since the last
// commit", via a hand-rolled Myers diff (crate::diff, through git::
// marks_from_diff for the shared DiffMark-conversion logic) between
// this buffer's own *current, in-memory* content and what's actually
// on disk right now. Shares the same `buf.diff`/gutter rendering
// `toggle_git_diff` already populates -- the two are mutually exclusive
// toggle states (turning one on turns the other off, same as any
// single field can only hold one thing at a time), not a second,
// independent gutter column. No dirty-buffer refusal, unlike
// toggle_git_diff -- an unsaved buffer is exactly the interesting case
// here, not one to refuse.
pub(crate) fn toggle_buffer_diff(buf: &mut TextBuffer) -> Result<bool, String> {
    if buf.diff.is_some() {
        buf.diff = None;
        return Ok(false);
    }
    let path = buf.path().ok_or_else(|| "no file name".to_string())?;
    let on_disk = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let disk_lines: Vec<&str> = on_disk.lines().collect();
    let current = buf.text();
    let current_lines: Vec<&str> = current.lines().collect();
    buf.diff = Some(crate::git::marks_from_diff(&disk_lines, &current_lines));
    Ok(true)
}

// Color/attrs for one diagnostic's own underline -- mirrors highlight::
// default_style's shape exactly (severity in, presentation out), kept as
// its own small function for the same reason that one is: Severity only
// has one variant today (Warning), but the match is already exhaustive
// against the real enum rather than a wildcard, so adding e.g. Error
// later is a one-line addition here, not a search for every place
// severity might matter.
fn diagnostic_style(severity: lint::Severity) -> (vt100::Color, vt100::CellAttrs) {
    let underline = vt100::CellAttrs { underline: true, ..vt100::CellAttrs::default() };
    match severity {
        lint::Severity::Warning => (vt100::Color::Indexed(3), underline),
    }
}

// diagnostic_spans_for_line's own sibling to spans_for_line (see that
// function's own doc comment for the shared shape/reasoning -- same
// whole-buffer-char-offset convention, same per-line clamp) -- reads
// buf.diagnostics instead of a precomputed highlight pass, and resolves
// each one's own color from diagnostic_style instead of a single shared
// kind-to-style mapping (a highlight pass is one Highlighter's own
// output, always styled uniformly by HighlightKind; diagnostics can vary
// per finding once Severity grows past its current one variant).
fn diagnostic_spans_for_line(diagnostics: &[lint::Diagnostic], line_start: usize, line_len: usize) -> Vec<StyledSpan> {
    let line_end = line_start + line_len;
    diagnostics
        .iter()
        .filter(|d| d.start < line_end && d.end > line_start)
        .map(|d| {
            let (fg, attrs) = diagnostic_style(d.severity);
            StyledSpan { start: d.start.saturating_sub(line_start), end: (d.end - line_start).min(line_len), fg, attrs }
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
// rather than per row. `diag_styled` (diagnostic_spans_for_line's own
// per-line slice) is composed *between* syntax and selection -- after
// the syntax layer, so a diagnostic's underline is never hidden beneath
// its own token color, but before the selection layer, so selecting
// text over a diagnostic still reads as a selection first (matching
// selection's own "wins regardless of what it's covering" rule, one
// layer up).
// `hoffset` (Buffer::viewport_left) is a *display column*, not a char
// index (see bishedit::unicode_width's own doc comment) -- `chars` is a
// window onto the line covering display columns `hoffset..hoffset+cols`
// (via `char_at_col`/accumulated width, not a flat `hoffset+c` char
// offset, which would grab exactly `cols` *characters* regardless of
// how many real terminal columns those actually occupy -- letting a
// wide char anywhere in the window push real content past `cols`
// columns and spill into whatever's rendered immediately to the right,
// e.g. a neighboring split pane). A char that would only *partially*
// fit at the right edge is dropped whole, same "no half a CJK glyph"
// rule editor.rs's own `truncate_visible` already uses; any budget left
// over after that is padded with plain spaces, matching this function's
// own previous behavior for a short line. Still built by walking
// *consecutive* char indices starting from `start_char` (found via
// `char_at_col`), never skipping one in the middle -- so `chars[i]`
// always corresponds to real char index `start_char + i`, letting
// `highlights` (selection_columns_in_line's own output, already
// expressed in this same `start_char`-relative frame -- see its own doc
// comment) and the rebased `line_styled`/`diag_styled` spans below
// align with it exactly, with no separate cell-width bookkeeping
// needed for *those* two layers at all.
#[allow(clippy::too_many_arguments)]
fn render_row(out: &mut String, buf: &TextBuffer, line: usize, hoffset: usize, cols: usize, line_styled: &[StyledSpan], diag_styled: &[StyledSpan], highlights: &[(usize, usize)]) {
    let line_chars = buf.line_chars(line);
    let start_char = char_at_col(&line_chars, hoffset);
    let mut chars: Vec<char> = Vec::with_capacity(cols);
    let mut used = 0;
    let mut end_char = start_char;
    while end_char < line_chars.len() {
        let w = char_width(line_chars[end_char]);
        if used + w > cols {
            break;
        }
        chars.push(line_chars[end_char]);
        used += w;
        end_char += 1;
    }
    while used < cols {
        chars.push(' ');
        used += 1;
    }

    let selected: Vec<StyledSpan> = highlights
        .iter()
        .map(|&(start, end)| StyledSpan { start, end, fg: vt100::Color::Default, attrs: vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() } })
        .collect();

    // Clamped against `chars.len()` (the real array length after
    // width-bounded selection/padding above), not `cols` (the column
    // *budget* it was bounded to fit) -- the two only coincide when
    // there's no wide char anywhere in this window; whenever there is,
    // `chars.len() < cols` (a wide char spends 2 columns of budget for
    // only 1 array slot), and clamping to the wider `cols` instead would
    // hand `highlight::compose` a span end past the real end of `chars`.
    fn rebase(spans: &[StyledSpan], start_char: usize, len: usize) -> Vec<StyledSpan> {
        spans
            .iter()
            .filter(|s| s.end > start_char && s.start < start_char + len)
            .map(|s| StyledSpan { start: s.start.saturating_sub(start_char), end: (s.end - start_char).min(len), fg: s.fg, attrs: s.attrs })
            .collect()
    }
    let line_styled = rebase(line_styled, start_char, chars.len());
    let diag_styled = rebase(diag_styled, start_char, chars.len());

    let cells = highlight::compose(&chars, &[&line_styled, &diag_styled, &selected]);
    out.push_str(&highlight::render_styled(&cells));
}

// The actual rendering, factored out as a pure string-builder (build the
// whole escape-coded string first, print/feed it exactly once) --
// mirrors repl.rs's own `compose_redraw`/`render_compositor_frame`
// split. Content rows plus real-cursor positioning at the end -- no
// status row: that's the terminal's own global one now
// (repl::render_global_status_row), drawn separately by render_editor_
// frame, below, since it belongs at an absolute terminal position this
// function's own pane-relative-or-absolute `row_origin`/`col_origin`
// duality (see just below) has no way to express for freeze_editor_
// frame's own target. Reimplemented here rather than shared with
// repl.rs's own `render_normal_mode_frame`: the two render different
// concrete `Buffer` types (that one reads a `ScreenBuffer`'s
// scrollback/live-grid split directly, not through the `Buffer` trait at
// all).
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
pub fn build_editor_frame(
    buf: &TextBuffer,
    vk: &VimKeys,
    mode: EditorMode,
    rect: Rect,
    row_origin: usize,
    col_origin: usize,
    color_overrides: Option<&highlight::ColorOverrides>,
) -> String {
    let content_rows = editor_content_rows(rect);
    let total = buf.line_count();
    // Reserves at least one column for content even if the gutter would
    // otherwise want more than the whole pane -- only reachable in a
    // pathologically narrow split, but `content_cols` below would
    // underflow without this.
    let gutter_width = total_gutter_width(buf).min(rect.cols.saturating_sub(1));
    let content_cols = rect.cols - gutter_width;
    // How far this line's own rendering is scrolled right -- see Buffer::
    // viewport_left's own doc comment. scroll_to_show_cursor is what
    // actually keeps this in bounds of the cursor's own column; nothing
    // here clamps it, so a buffer that's never had a cursor move onto a
    // long line (or whose gutter just grew, e.g. crossing a line-count
    // digit boundary, shrinking content_cols out from under an old
    // offset) could in principle scroll a short line's content fully off
    // to the left -- reachable only via a resize/edit race no test below
    // exercises, and self-correcting the moment the cursor moves again.
    let hoffset = buf.viewport_left();
    let active = if mode == EditorMode::Normal { crate::repl::active_visual_range(vk, buf) } else { None };
    // Computed once for the whole buffer, not per visible row -- see
    // buffer_highlight_spans's own doc comment for why a multi-line
    // construct needs that.
    let whole_styled = buffer_highlight_spans(buf, color_overrides);
    let starts = line_starts(buf);
    let mut out = String::new();
    for r in 0..content_rows {
        let line = buf.viewport_top() + r;
        out.push_str(&format!("\x1b[{};{}H\x1b[K", row_origin + r + 1, col_origin + 1));
        render_gutter(&mut out, buf, &starts, line);
        if line < total {
            // The char index render_row's own window will start
            // rendering from for *this* line -- see selection_columns_
            // in_line's own doc comment for why this (not `hoffset`
            // itself) is the right rebase point once a line can have
            // wide chars in it.
            let start_char = char_at_col(&buf.line_chars(line), hoffset);
            let mut highlights = Vec::new();
            for range in buf.selections.iter().chain(active.iter()) {
                if let Some(cols) = selection_columns_in_line(range, line, start_char, content_cols) {
                    highlights.push(cols);
                }
            }
            let line_styled = spans_for_line(&whole_styled, starts[line], buf.line_len(line));
            let diag_styled = diagnostic_spans_for_line(&buf.diagnostics, starts[line], buf.line_len(line));
            render_row(&mut out, buf, line, hoffset, content_cols, &line_styled, &diag_styled, &highlights);
        }
    }

    let (cl, cc) = buf.cursor();
    let screen_row = cl.saturating_sub(buf.viewport_top()).min(content_rows.saturating_sub(1));
    // The cursor's own real display column, not its char index -- see
    // bishedit::unicode_width's own doc comment for why those two
    // differ once any wide/zero-width char precedes it on this line.
    let cursor_col = col_of(&buf.line_chars(cl), cc);
    let screen_col = gutter_width + cursor_col.saturating_sub(hoffset).min(content_cols.saturating_sub(1));
    out.push_str(&format!("\x1b[{};{}H\x1b[?25h", row_origin + screen_row + 1, col_origin + screen_col + 1));
    out
}

pub fn render_editor_frame(buf: &TextBuffer, vk: &VimKeys, mode: EditorMode, rect: Rect, term_rows: usize, term_cols: usize, color_overrides: Option<&highlight::ColorOverrides>) {
    let mut out = crate::repl::render_global_status_row(&status_text(buf, vk, mode, term_cols), term_rows);
    out.push_str(&build_editor_frame(buf, vk, mode, rect, rect.row, rect.col, color_overrides));
    print!("{}", out);
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
pub fn freeze_editor_frame(screen: &Rc<RefCell<vt100::Screen>>, buf: &TextBuffer, vk: &VimKeys, rect: Rect, color_overrides: Option<&highlight::ColorOverrides>) {
    let framed = build_editor_frame(buf, vk, EditorMode::Normal, rect, 0, 0, color_overrides);
    screen.borrow_mut().feed(framed.as_bytes());
}

#[cfg(test)]
mod indent_tests {
    use super::*;

    fn buf(text: &str) -> TextBuffer {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), text);
        buf.set_cursor(0, 0);
        buf
    }

    fn text_of(buf: &TextBuffer) -> String {
        (0..buf.line_count()).map(|l| (0..buf.line_len(l)).map(|c| buf.char_at(l, c).unwrap()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn indent_rows_prepends_shiftwidth_spaces_to_every_non_empty_line() {
        let mut b = buf("foo\n\nbar");
        indent_rows(&mut b, 0, 2);
        assert_eq!(text_of(&b), "    foo\n\n    bar");
    }

    #[test]
    fn outdent_rows_strips_up_to_shiftwidth_leading_whitespace() {
        let mut b = buf("      foo\n  bar\nbaz");
        outdent_rows(&mut b, 0, 2);
        assert_eq!(text_of(&b), "  foo\nbar\nbaz");
    }

    #[test]
    fn indent_operator_motion_shifts_every_line_the_motion_touches_regardless_of_its_own_shape() {
        // WordForward from line 0 to line 1 is an ordinary exclusive
        // charwise motion -- >{motion} still treats its target as whole
        // lines (see Op::Indent's own doc comment).
        let mut b = buf("foo\nbar");
        indent_operator_motion(&mut b, motion::Motion::WordForward, None);
        assert_eq!(text_of(&b), "    foo\n    bar");
        assert_eq!(b.cursor(), (0, 0));
    }

    #[test]
    fn indent_lines_shifts_count_lines_starting_at_the_cursor_by_one_shiftwidth_each() {
        let mut b = buf("one\ntwo\nthree");
        indent_lines(&mut b, Some(2));
        assert_eq!(text_of(&b), "    one\n    two\nthree");
    }

    #[test]
    fn outdent_lines_default_count_is_one() {
        let mut b = buf("    one\n    two");
        outdent_lines(&mut b, None);
        assert_eq!(text_of(&b), "one\n    two");
    }

    #[test]
    fn indent_selections_shifts_every_committed_selection_whole_line() {
        let mut b = buf("one\ntwo\nthree");
        b.selections = vec![
            motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 0), to: (0, 0) },
            motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (2, 0), to: (2, 0) },
        ];
        indent_selections(&mut b);
        assert_eq!(text_of(&b), "    one\ntwo\n    three");
    }

    #[test]
    fn outdent_selections_is_a_noop_with_no_selections() {
        let mut b = buf("    one");
        outdent_selections(&mut b);
        assert_eq!(text_of(&b), "    one");
    }
}

#[cfg(test)]
mod macro_tests {
    use super::*;
    use crate::bishedit::vimkeys::KeyOutcome;

    fn rect() -> Rect {
        Rect { row: 0, col: 0, rows: 24, cols: 80 }
    }

    fn text_of(buf: &TextBuffer) -> String {
        (0..buf.line_count()).map(|l| buf.line_chars(l).into_iter().collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    // Drives one `KeyOutcome` the same way `run_normal_mode_navigation`'s
    // own big `match vk.feed(key)` does, for exactly the two variants
    // this test needs -- a motion, or an Insert-mode excursion (which
    // stays inline via `run_insert_mode`, same `vk` throughout, the
    // property this test exists to exercise).
    fn apply(buf: &mut TextBuffer, vk: &mut VimKeys, registers: &mut Registers, outcome: KeyOutcome) {
        match outcome {
            KeyOutcome::Motion(m, count) => motion::apply_motion(buf, m, count),
            KeyOutcome::EnterInsert(cmd) => {
                resolve_insert_start(buf, cmd);
                run_insert_mode(buf, vk, rect(), registers, &mut || {}, false, 24, 80, None, &[]).unwrap();
            }
            other => panic!("unexpected outcome in this test: {other:?}"),
        }
    }

    // A macro whose recorded content spans a real Insert-mode excursion
    // (`A;<Esc>`) plus a plain motion (`j`) -- built directly via
    // `record_key` (there's no real terminal to drive `run_insert_mode`'s
    // own reads live in a test), but *replayed* for real: `@a` below
    // drives the actual `run_insert_mode` call through `apply`'s
    // `EnterInsert` arm, and that function's own reads (routed through
    // `vk.next_key`, same as every other host read site -- see that
    // method's own doc comment) must be served entirely from the replay
    // queue for this to produce the right buffer content at all -- if
    // `run_insert_mode` instead fell through to a real terminal read,
    // it would see immediate EOF and abandon each excursion after just
    // the `A`, never inserting the `;`.
    #[test]
    fn macro_replay_drives_a_real_insert_mode_excursion() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "one\ntwo\nthree");
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();

        vk.start_recording('a');
        for key in [Key::Char('A'), Key::Char(';'), Key::Escape, Key::Char('j')] {
            vk.record_key(key);
        }
        vk.stop_recording();

        // `2@a`: replays the whole recorded sequence (Insert excursion
        // included) twice in a row, landing on "one" then "two" (cursor
        // starts on "one"), leaving "three" untouched.
        assert!(vk.queue_macro_replay('a', 2));
        while let Some(key) = vk.next_key(|| Ok(None)).unwrap() {
            let outcome = vk.feed(key);
            apply(&mut buf, &mut vk, &mut registers, outcome);
        }

        assert_eq!(text_of(&buf), "one;\ntwo;\nthree");
    }

    #[test]
    fn macro_replay_count_repeats_a_pure_motion_macro() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "one\ntwo\nthree\nfour");
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();

        let mut scripted = std::iter::once(Key::Char('j'));
        let mut read_live = || -> io::Result<Option<Key>> { Ok(scripted.next()) };

        vk.start_recording('a');
        let key = vk.next_key(&mut read_live).unwrap().unwrap();
        let outcome = vk.feed(key);
        apply(&mut buf, &mut vk, &mut registers, outcome);
        vk.stop_recording();
        assert_eq!(buf.cursor(), (1, 0));

        assert!(vk.queue_macro_replay('a', 2));
        while let Some(key) = vk.next_key(|| Ok(None)).unwrap() {
            let outcome = vk.feed(key);
            apply(&mut buf, &mut vk, &mut registers, outcome);
        }
        assert_eq!(buf.cursor(), (3, 0));
    }
}

// Regression: `c` on multiple selections used to only type into the one
// gap `delete_selections` picked as this buffer's own new `cursor` (the
// leftmost) -- every other deleted selection's own gap sat there
// untouched, receiving nothing typed. run_insert_mode's own
// `extra_cursors` param is what replicates each keystroke across every
// one of them instead; these drive it directly (bypassing repl.rs's own
// `c` handler, which is what actually seeds `extra_cursors` from
// delete_selections' own return value in production, but has no real
// terminal to read a scripted key sequence from in a test -- same
// "record into a macro, then replay it" trick macro_tests above already
// uses for exactly that reason).
#[cfg(test)]
mod multi_cursor_insert_tests {
    use super::*;

    fn rect() -> Rect {
        Rect { row: 0, col: 0, rows: 24, cols: 80 }
    }

    fn line(buf: &TextBuffer, row: usize) -> String {
        buf.line_chars(row).into_iter().collect()
    }

    fn scripted(vk: &mut VimKeys, keys: &[Key]) {
        vk.start_recording('a');
        for &k in keys {
            vk.record_key(k);
        }
        vk.stop_recording();
        assert!(vk.queue_macro_replay('a', 1));
    }

    #[test]
    fn typing_after_a_multi_cursor_change_lands_at_every_gap() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), " one\n two\n three");
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Char('D'), Key::Char('O'), Key::Char('N'), Key::Char('E'), Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut || {}, false, 24, 80, None, &[(1, 0), (2, 0)]).unwrap();

        assert_eq!(line(&buf, 0), "DONE one");
        assert_eq!(line(&buf, 1), "DONE two");
        assert_eq!(line(&buf, 2), "DONE three");
    }

    #[test]
    fn backspace_after_a_multi_cursor_change_removes_from_every_gap() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "X one\nX two");
        buf.set_cursor(0, 1); // right after the "X" on line 0
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Backspace, Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut || {}, false, 24, 80, None, &[(1, 1)]).unwrap();

        assert_eq!(line(&buf, 0), " one");
        assert_eq!(line(&buf, 1), " two");
    }

    #[test]
    fn enter_after_a_multi_cursor_change_splits_every_gap_line() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "ab\ncd");
        buf.set_cursor(0, 1); // between 'a' and 'b'
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Enter, Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut || {}, false, 24, 80, None, &[(1, 1)]).unwrap();

        assert_eq!(buf.line_count(), 4);
        assert_eq!(line(&buf, 0), "a");
        assert_eq!(line(&buf, 1), "b");
        assert_eq!(line(&buf, 2), "c");
        assert_eq!(line(&buf, 3), "d");
    }

    #[test]
    fn two_gaps_on_the_same_row_do_not_corrupt_each_others_position() {
        // Gaps at columns 0 and 6 of one row -- typing "X" at both must
        // land right where each original gap was, not have the first
        // insertion's own rightward shift throw off the second's.
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), " place ");
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Char('X'), Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut || {}, false, 24, 80, None, &[(0, 6)]).unwrap();

        assert_eq!(line(&buf, 0), "X placeX ");
    }
}

// Normal mode's cursor can never sit *after* the last character of a
// line (only Insert mode allows that, to append) -- leaving Insert mode
// while sitting there must pull the cursor back onto the last character
// instead of leaving it in that Insert-only position.
#[cfg(test)]
mod insert_mode_exit_clamp_tests {
    use super::*;

    fn rect() -> Rect {
        Rect { row: 0, col: 0, rows: 24, cols: 80 }
    }

    fn scripted(vk: &mut VimKeys, keys: &[Key]) {
        vk.start_recording('a');
        for &k in keys {
            vk.record_key(k);
        }
        vk.stop_recording();
        assert!(vk.queue_macro_replay('a', 1));
    }

    #[test]
    fn escape_at_end_of_line_pulls_the_cursor_back_onto_the_last_char() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "ab");
        buf.set_cursor(0, 2); // one past 'b', a valid Insert-mode position
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut || {}, false, 24, 80, None, &[]).unwrap();

        assert_eq!(buf.cursor(), (0, 1), "cursor must land on 'b', not past it");
    }

    #[test]
    fn typing_to_the_end_of_a_line_then_escaping_also_clamps() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Char('h'), Key::Char('i'), Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut || {}, false, 24, 80, None, &[]).unwrap();

        assert_eq!(buf.cursor(), (0, 1), "cursor must land on the 'i', not past it");
    }

    #[test]
    fn the_caret_mark_still_records_the_true_one_past_end_insert_position() {
        // `gi` resumes appending from exactly where insert mode left off,
        // which the Normal-mode clamp above must not disturb.
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "ab");
        buf.set_cursor(0, 2);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut || {}, false, 24, 80, None, &[]).unwrap();

        assert_eq!(buf.get_mark('^'), Some((0, 2)));
    }

    #[test]
    fn escape_on_a_wholly_empty_line_stays_at_column_zero() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut || {}, false, 24, 80, None, &[]).unwrap();

        assert_eq!(buf.cursor(), (0, 0));
    }
}

#[cfg(test)]
mod diagnose_tests {
    use super::*;

    // `is_bash_file` keys off the path's own extension (see its own doc
    // comment) -- `TextBuffer::new_unnamed`/`insert_text` can't produce
    // one, so a real `.bash` temp file is what actually exercises
    // diagnose_buffer's language gate, the same way textbuffer.rs's own
    // `open_and_save_round_trip_a_real_file` test does for `open`/`save`.
    fn temp_bash_buffer(tag: &str, text: &str) -> TextBuffer {
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-diag-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("script.bash");
        std::fs::write(&path, text).unwrap();
        TextBuffer::open(&path, 10).unwrap()
    }

    #[test]
    fn diagnose_buffer_runs_the_bash_linter_against_a_bash_file() {
        let buf = temp_bash_buffer("basic", "echo $foo\n");
        let diags = diagnose_buffer(&buf);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "unquoted-expansion");
    }

    #[test]
    fn diagnose_buffer_is_a_noop_for_a_file_with_no_recognized_language() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "echo $foo");
        assert!(diagnose_buffer(&buf).is_empty());
    }

    #[test]
    fn diagnostic_spans_for_line_clamps_to_the_requested_lines_own_extent() {
        let diags = vec![lint::Diagnostic { start: 5, end: 9, severity: lint::Severity::Warning, code: "unquoted-expansion", message: String::new(), fix: None }];
        let spans = diagnostic_spans_for_line(&diags, 0, 20);
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start, spans[0].end), (5, 9));
        assert!(diagnostic_spans_for_line(&diags, 10, 20).is_empty());
    }

    #[test]
    fn a_real_edit_clears_previously_computed_diagnostics() {
        let mut buf = temp_bash_buffer("clears", "echo $foo\n");
        buf.diagnostics = diagnose_buffer(&buf);
        assert!(!buf.diagnostics.is_empty());
        buf.insert_text((0, 0), "x");
        assert!(buf.diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_position_maps_a_flat_offset_to_line_and_column() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "abc\ndefgh\nij");
        assert_eq!(diagnostic_position(&buf, 0), (0, 0));
        assert_eq!(diagnostic_position(&buf, 3), (0, 3)); // end of "abc"
        assert_eq!(diagnostic_position(&buf, 4), (1, 0)); // 'd'
        assert_eq!(diagnostic_position(&buf, 9), (1, 5)); // end of "defgh"
        assert_eq!(diagnostic_position(&buf, 10), (2, 0)); // 'i'
    }

    #[test]
    fn apply_fix_splices_the_replacement_and_returns_true() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "echo $foo");
        // "$foo" sits at flat offsets 5..9.
        let diagnostic = lint::Diagnostic {
            start: 5,
            end: 9,
            severity: lint::Severity::Warning,
            code: "unquoted-expansion",
            message: String::new(),
            fix: Some(lint::Fix { start: 5, end: 9, replacement: "\"$foo\"".to_string() }),
        };
        assert!(apply_fix(&mut buf, &diagnostic));
        assert_eq!(buf.line_chars(0).into_iter().collect::<String>(), "echo \"$foo\"");
    }

    #[test]
    fn apply_fix_is_a_no_op_without_a_fix() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "echo $foo");
        let diagnostic = lint::Diagnostic { start: 5, end: 9, severity: lint::Severity::Warning, code: "unquoted-expansion", message: String::new(), fix: None };
        assert!(!apply_fix(&mut buf, &diagnostic));
        assert_eq!(buf.line_chars(0).into_iter().collect::<String>(), "echo $foo");
    }
}

#[cfg(test)]
mod horizontal_scroll_tests {
    use super::*;

    fn buf(text: &str) -> TextBuffer {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), text);
        buf.set_cursor(0, 0);
        buf
    }

    #[test]
    fn scroll_to_show_cursor_leaves_viewport_left_at_zero_while_the_cursor_fits() {
        let mut b = buf("hello world");
        b.set_cursor(0, 5);
        scroll_to_show_cursor(&mut b, 10);
        assert_eq!(b.viewport_left(), 0);
    }

    #[test]
    fn scroll_to_show_cursor_scrolls_right_just_far_enough_to_keep_the_cursor_visible() {
        let mut b = buf(&"x".repeat(50));
        b.set_cursor(0, 20);
        scroll_to_show_cursor(&mut b, 10);
        // The cursor (column 20) must be the *last* visible column of a
        // 10-wide window -- vim's own "scroll exactly enough, not all the
        // way" convention, same one viewport_top already follows.
        assert_eq!(b.viewport_left(), 11);
    }

    #[test]
    fn scroll_to_show_cursor_scrolls_back_left_once_the_cursor_moves_before_the_viewport() {
        let mut b = buf(&"x".repeat(50));
        b.set_cursor(0, 20);
        scroll_to_show_cursor(&mut b, 10);
        assert_eq!(b.viewport_left(), 11);
        b.set_cursor(0, 3);
        scroll_to_show_cursor(&mut b, 10);
        assert_eq!(b.viewport_left(), 3);
    }

    #[test]
    fn selection_columns_in_line_rebases_a_charwise_range_by_hoffset_and_clamps_to_cols() {
        let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 15), to: (0, 24) };
        // Scrolled 10 columns right, a 10-wide window: the selection
        // (line-absolute 15..25) becomes window-local 5..10, clamped at
        // the window's own right edge.
        assert_eq!(selection_columns_in_line(&range, 0, 10, 10), Some((5, 10)));
    }

    #[test]
    fn selection_columns_in_line_clamps_a_charwise_range_starting_left_of_the_viewport() {
        let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 0), to: (0, 24) };
        assert_eq!(selection_columns_in_line(&range, 0, 10, 10), Some((0, 10)));
    }

    #[test]
    fn selection_columns_in_line_linewise_always_spans_the_full_viewport_window_regardless_of_hoffset() {
        let range = motion::MotionRange { shape: motion::MotionShape::Linewise, from: (0, 0), to: (0, 0) };
        assert_eq!(selection_columns_in_line(&range, 0, 37, 10), Some((0, 10)));
    }
}

#[cfg(test)]
mod pre_save_hook_tests {
    use super::*;

    // A nonexistent path still leaves the buffer's own `path()` set (see
    // `TextBuffer::open`'s own doc comment) -- exactly what `file_type`
    // reads, with no real file needed for these tests. `text` is given
    // with no trailing newline: that's the shape a buffer's own `lines`
    // actually have for real content (`TextBuffer::open` strips exactly
    // one trailing '\n' off whatever it reads, same as `buffer_text`
    // never producing one either) -- a helper that instead spliced a
    // trailing '\n' straight into `insert_text` would leave a phantom
    // empty last line that isn't how a freshly opened file ever looks,
    // and would silently hide the "TextBuffer::save adds its own
    // trailing newline" issue `strip_buffer_implicit_trailing_newline`
    // exists to handle.
    fn buf_with_ext(text: &str, ext: &str) -> TextBuffer {
        let path = format!("/tmp/bish-fileeditor-pre-save-hook-test.{ext}");
        let mut buf = TextBuffer::open(std::path::Path::new(&path), 10).unwrap();
        buf.insert_text((0, 0), text);
        buf.set_cursor(0, 0);
        buf
    }

    #[test]
    fn file_type_recognizes_only_dot_bash_today() {
        assert_eq!(file_type(&buf_with_ext("x", "bash")), FileType::Bash);
        assert_eq!(file_type(&buf_with_ext("x", "sh")), FileType::Unknown);
        assert_eq!(file_type(&buf_with_ext("x", "txt")), FileType::Unknown);
        assert_eq!(file_type(&TextBuffer::new_unnamed(10)), FileType::Unknown);
    }

    #[test]
    fn run_pre_save_hooks_reformats_a_bash_buffer_before_save() {
        let mut buf = buf_with_ext("if true\nthen\necho hi\nfi", "bash");
        run_pre_save_hooks(&mut buf);
        assert_eq!(buffer_text(&buf), "if true; then\n\techo hi\nfi");
    }

    #[test]
    fn run_pre_save_hooks_leaves_an_already_formatted_bash_buffer_untouched() {
        let mut buf = buf_with_ext("if true; then\n\techo hi\nfi", "bash");
        run_pre_save_hooks(&mut buf);
        assert_eq!(buffer_text(&buf), "if true; then\n\techo hi\nfi");
    }

    // The regression case: BashFormatter::check always intends exactly
    // one trailing newline for a plain string, but `buffer_text` (unlike
    // a file's own on-disk bytes) never has one -- so this hook's own
    // EOF-gap fix would want to *insert* "\n" right after the buffer's
    // last real character. Applied literally that becomes a genuine new
    // empty last line, which `TextBuffer::save`'s own implicit trailing
    // newline then doubles up into a real blank line at the end of the
    // file on disk. Asserted end-to-end through a real `save()` (not
    // just `buffer_text`, which can't see this bug -- see
    // `strip_buffer_implicit_trailing_newline`'s own doc comment) so a
    // regression here fails loudly rather than just in the editor.
    #[test]
    fn run_pre_save_hooks_does_not_leave_a_spurious_blank_line_at_end_of_file_on_save() {
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-eof-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("script.bash");
        std::fs::write(&path, "if true; then\n\techo hi\nfi\n").unwrap();
        let mut buf = TextBuffer::open(&path, 10).unwrap();
        run_pre_save_hooks(&mut buf);
        buf.save(None).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "if true; then\n\techo hi\nfi\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_pre_save_hooks_does_nothing_for_a_non_bash_buffer() {
        let mut buf = buf_with_ext("if true\nthen\necho hi\nfi\n", "txt");
        run_pre_save_hooks(&mut buf);
        assert_eq!(buffer_text(&buf), "if true\nthen\necho hi\nfi\n");
    }

    #[test]
    fn run_pre_save_hooks_leaves_an_unparseable_bash_buffer_untouched_rather_than_blocking_save() {
        let mut buf = buf_with_ext("if true\nthen\n", "bash");
        run_pre_save_hooks(&mut buf);
        assert_eq!(buffer_text(&buf), "if true\nthen\n");
    }

    #[test]
    fn format_buffer_reports_formatted_and_applies_the_fixes() {
        let mut buf = buf_with_ext("if true\nthen\necho hi\nfi", "bash");
        assert_eq!(format_buffer(&mut buf), FormatOutcome::Formatted);
        assert_eq!(buffer_text(&buf), "if true; then\n\techo hi\nfi");
    }

    #[test]
    fn format_buffer_reports_already_formatted_and_leaves_the_buffer_untouched() {
        let mut buf = buf_with_ext("if true; then\n\techo hi\nfi", "bash");
        assert_eq!(format_buffer(&mut buf), FormatOutcome::AlreadyFormatted);
        assert_eq!(buffer_text(&buf), "if true; then\n\techo hi\nfi");
    }

    // `buf_with_ext`'s own buffers never round-trip through a real file on
    // disk (it splices `insert_text` into a buffer opened against a
    // nonexistent path) -- this one does, matching exactly what `:format`
    // sees against a file really opened via `e`: a regression test for
    // the "an EOF-gap fix that's already a no-op still counted as
    // applied" bug (see `strip_buffer_implicit_trailing_newline`'s own
    // doc comment) would only have caught it through this real load path.
    #[test]
    fn format_buffer_reports_already_formatted_for_a_real_file_loaded_from_disk() {
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-real-load-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clean.bash");
        std::fs::write(&path, "if true; then\n\techo hi\nfi\n").unwrap();
        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert_eq!(format_buffer(&mut buf), FormatOutcome::AlreadyFormatted);
        assert_eq!(buffer_text(&buf), "if true; then\n\techo hi\nfi");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn format_buffer_reports_not_supported_for_a_non_bash_buffer_and_leaves_it_untouched() {
        let mut buf = buf_with_ext("if true\nthen\necho hi\nfi", "txt");
        assert_eq!(format_buffer(&mut buf), FormatOutcome::NotSupported);
        assert_eq!(buffer_text(&buf), "if true\nthen\necho hi\nfi");
    }

    #[test]
    fn format_buffer_reports_the_parse_error_for_an_unparseable_bash_buffer_and_leaves_it_untouched() {
        let mut buf = buf_with_ext("if true\nthen\n", "bash");
        let outcome = format_buffer(&mut buf);
        assert!(matches!(outcome, FormatOutcome::Error(_)), "expected FormatOutcome::Error, got {outcome:?}");
        assert_eq!(buffer_text(&buf), "if true\nthen\n");
    }
}

// `:git blame`'s own tests -- `toggle_git_blame`'s dirty-buffer/no-path
// checks run unconditionally (no subprocess involved), but the real
// end-to-end round trip needs an actual git repository and a real `git`
// on $PATH, so that one test skips itself (rather than failing) when
// crate::git::available() says there isn't one -- matching this whole
// feature's own "quietly unavailable, not a hard dependency" contract
// (see git.rs's own module doc comment).
#[cfg(test)]
mod git_blame_tests {
    use super::*;

    #[test]
    fn toggle_git_blame_refuses_a_buffer_with_unsaved_changes() {
        let mut buf = TextBuffer::open(std::path::Path::new("/tmp/bish-fileeditor-git-blame-dirty-test.txt"), 10).unwrap();
        // insert_text always marks a buffer dirty -- see its own doc comment.
        buf.insert_text((0, 0), "hello\n");
        let err = toggle_git_blame(&mut buf).unwrap_err();
        assert!(err.contains("unsaved changes"), "{err}");
        assert!(buf.blame.is_none());
    }

    #[test]
    fn toggle_git_blame_refuses_a_buffer_with_no_path() {
        let mut buf = TextBuffer::new_unnamed(10);
        let err = toggle_git_blame(&mut buf).unwrap_err();
        assert!(err.contains("no file name"), "{err}");
    }

    #[test]
    fn toggle_git_blame_toggles_on_then_off_for_a_real_git_repo_file() {
        if !crate::git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-git-blame-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(&dir).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test User"]);
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        run(&["add", "f.txt"]);
        run(&["commit", "-q", "-m", "initial"]);

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert!(!buf.is_dirty());
        assert_eq!(toggle_git_blame(&mut buf).unwrap(), true);
        let blame = buf.blame.as_ref().unwrap();
        assert_eq!(blame.len(), 2);
        assert_eq!(blame[0].author, "Test User");
        assert_eq!(blame[0].short_commit.len(), 8);
        assert_eq!(toggle_git_blame(&mut buf).unwrap(), false);
        assert!(buf.blame.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

// `:git diff`'s own tests -- same shape/skip-if-no-git reasoning as
// git_blame_tests above.
#[cfg(test)]
mod git_diff_tests {
    use super::*;
    use crate::git::DiffMark;

    #[test]
    fn toggle_git_diff_refuses_a_buffer_with_unsaved_changes() {
        let mut buf = TextBuffer::open(std::path::Path::new("/tmp/bish-fileeditor-git-diff-dirty-test.txt"), 10).unwrap();
        buf.insert_text((0, 0), "hello\n");
        let err = toggle_git_diff(&mut buf).unwrap_err();
        assert!(err.contains("unsaved changes"), "{err}");
        assert!(buf.diff.is_none());
    }

    #[test]
    fn toggle_git_diff_refuses_a_buffer_with_no_path() {
        let mut buf = TextBuffer::new_unnamed(10);
        let err = toggle_git_diff(&mut buf).unwrap_err();
        assert!(err.contains("no file name"), "{err}");
    }

    fn git_run(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git").args(args).current_dir(dir).status().unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn toggle_git_diff_toggles_on_then_off_and_marks_a_changed_line() {
        if !crate::git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-git-diff-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        git_run(&dir, &["init", "-q"]);
        git_run(&dir, &["config", "user.email", "test@example.com"]);
        git_run(&dir, &["config", "user.name", "Test User"]);
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        git_run(&dir, &["add", "f.txt"]);
        git_run(&dir, &["commit", "-q", "-m", "initial"]);
        // Change on disk (not through the buffer -- toggle_git_diff reads
        // the real file, same as toggle_git_blame does) so there's a real
        // diff for `git diff` to find.
        std::fs::write(&path, "one\nCHANGED\nthree\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert!(!buf.is_dirty());
        assert_eq!(toggle_git_diff(&mut buf).unwrap(), true);
        assert_eq!(buf.diff.as_ref().unwrap().get(&1), Some(&DiffMark::Changed));
        assert_eq!(buf.diff.as_ref().unwrap().len(), 1);
        assert_eq!(toggle_git_diff(&mut buf).unwrap(), false);
        assert!(buf.diff.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggle_git_diff_marks_every_line_added_for_an_untracked_file() {
        if !crate::git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-git-diff-untracked-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        git_run(&dir, &["init", "-q"]);
        let path = dir.join("new.txt");
        std::fs::write(&path, "a\nb\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        toggle_git_diff(&mut buf).unwrap();
        let diff = buf.diff.as_ref().unwrap();
        assert_eq!(diff.get(&0), Some(&DiffMark::Added));
        assert_eq!(diff.get(&1), Some(&DiffMark::Added));
        assert_eq!(diff.len(), 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggle_git_diff_errors_outside_a_git_repository() {
        if !crate::git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-git-diff-norepo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, "a\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        let err = toggle_git_diff(&mut buf).unwrap_err();
        assert!(err.to_lowercase().contains("git repository"), "{err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
