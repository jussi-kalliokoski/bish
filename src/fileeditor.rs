// `e [file]`: the modal text editor, built on bishedit's vim vocabulary
// (motion.rs, vimkeys.rs, registers.rs) and `TextBuffer`.
//
// Reached through `exec::ExecResult::Edit`, as `window` and `fg` are: a
// builtin has no keystroke or rendering access, and `Registers` lives in
// `repl::run`. repl.rs owns detach and resume (`Frame::Edit`); this module
// drives one open session while it's focused.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

use crate::bishedit::Buffer;
use crate::bishedit::format::BashFormatter;
use crate::bishedit::highlight::{self, HighlightContext, Highlighter, StyledSpan};
use crate::bishedit::lint::{self, BashLinter, Linter};
use crate::bishedit::motion;
use crate::bishedit::registers::{RegisterShape, RegisterValue, Registers};
use crate::bishedit::snippet::{self, Abbr, LiveSnippet, Snippet, SnippetHost};
use crate::bishedit::textbuffer;
use crate::bishedit::textbuffer::TextBuffer;
use crate::bishedit::unicode_width::char_width;
use crate::bishedit::vimkeys::{InsertCmd, Op, SurroundTarget, VimKeys};
use crate::editor::{self, Key};
use crate::lsp;
use crate::term;
use crate::vt100;
use crate::window::Rect;

// What repl.rs's `edit_frames` holds: more than a `TextBuffer`, so a
// half-typed count or a Visual selection survives a detach.
pub struct EditSession {
    pub buffer: TextBuffer,
    pub vk: VimKeys,
}

impl EditSession {
    // `None` opens an unnamed buffer; `Some` opens that file, or prepares
    // to create it (`TextBuffer::open`). Compressed content opens read-only
    // (see `compressed_text`).
    pub fn open(path: Option<&str>, vheight: usize) -> io::Result<EditSession> {
        let buffer = match path {
            Some(p) => match compressed_text(p) {
                Some(Ok(text)) => {
                    let mut buffer = TextBuffer::from_text(std::path::Path::new(p), &text, vheight);
                    buffer.set_readonly(true);
                    buffer
                }
                Some(Err(e)) => return Err(io::Error::other(e)),
                None => TextBuffer::open(std::path::Path::new(p), vheight)?,
            },
            None => TextBuffer::new_unnamed(vheight),
        };
        Ok(EditSession { buffer, vk: VimKeys::new() })
    }
}

// The text behind a path holding compressed content, a zip member
// (`some.zip!/dir/file.txt`) or a gzip'd file (`notes.txt.gz`); `None`
// for an ordinary path. Read-only, since writing back would mean
// compressing, which crate::inflate doesn't do; callers mark the buffer
// readonly so `:w` refuses. Converted lossily: a binary member shows
// replacement characters, and `e --hex` shows its bytes.
pub(crate) fn compressed_text(path: &str) -> Option<Result<String, String>> {
    compressed_bytes(path).map(|r| r.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

// `compressed_text`'s bytes, before the text conversion, for `e --hex`.
pub(crate) fn compressed_bytes(path: &str) -> Option<Result<Vec<u8>, String>> {
    if let Some((archive, inner)) = crate::archive::split(path) {
        return Some(crate::archive::read_member(&archive, &inner));
    }
    let as_path = std::path::Path::new(path);
    match crate::archive::kind_of(as_path) {
        Some(crate::archive::Kind::Gzip) => Some(crate::archive::gunzip(as_path).map(|(_, bytes)| bytes)),
        // A zip or tar is browsed, not opened
        // (`repl::expand_browse_targets`); `e --hex some.zip` gets here and
        // wants the archive's own bytes.
        Some(crate::archive::Kind::Zip) | Some(crate::archive::Kind::Tar) | None => None,
    }
}

// One file `e` was asked to open. `e a.sh b.sh` opens a frame per file
// on the pane's frame stack, the first named on top, so `:q` reveals
// the next.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EditTarget {
    // `None` is an unnamed buffer: `e` with no file.
    pub path: Option<String>,
    // `--hex`: open as bytes in the hex editor (`hexedit::HexSession`).
    pub hex: bool,
    // `--readonly`: refuse every edit, for text or hex.
    pub readonly: bool,
}

// `e`'s argument parsing, shared by the builtin and `bish tool edit`.
// Flags apply to the file after them: `e script.sh --hex core.bin`
// opens one of each. Flags with no file after them apply to an unnamed
// buffer (`e --hex`). `--` ends options; an unknown flag is an error,
// not a filename (a typo'd `--hxe` shouldn't open a buffer).
pub fn parse_edit_args(args: &[String]) -> Result<Vec<EditTarget>, String> {
    let mut targets: Vec<EditTarget> = Vec::new();
    let mut pending = EditTarget::default();
    let mut literal = false;
    for arg in args {
        if !literal {
            match arg.as_str() {
                "--" => {
                    literal = true;
                    continue;
                }
                "--hex" => {
                    pending.hex = true;
                    continue;
                }
                "--readonly" => {
                    pending.readonly = true;
                    continue;
                }
                // A lone `-` is a filename.
                other if other.starts_with('-') && other != "-" => {
                    return Err(format!("unrecognized option '{arg}'"));
                }
                _ => {}
            }
        }
        targets.push(EditTarget { path: Some(arg.clone()), ..std::mem::take(&mut pending) });
    }
    // Trailing flags, or no arguments: one unnamed buffer with them.
    if targets.is_empty() || pending != EditTarget::default() {
        targets.push(pending);
    }
    Ok(targets)
}

// The mode as the status line (`mode_label`) and the Visual
// highlighting in `build_editor_frame` need it. Replace behaves like
// Insert for both, labelled `-- REPLACE --`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorMode {
    Normal,
    Insert,
    Replace,
}

// Scrolls so the cursor is visible. `content_cols` is the pane's
// content width now (`editor_content_cols`), passed in so a resize
// applies on the next keystroke. `viewport_left` is a display column,
// not a char index (see bishedit::unicode_width), so the cursor goes
// through `col_of`.
pub(crate) fn scroll_to_show_cursor(buf: &mut TextBuffer, content_cols: usize) {
    if buf.wrap.wrap {
        scroll_wrapped(buf, content_cols);
        return;
    }
    let (line, col) = buf.cursor();
    let height = buf.viewport_height();
    // `scrolloff`, capped at half the pane, since a wider margin leaves no
    // middle for the cursor, and clamped at the buffer's ends.
    let margin = buf.wrap.scrolloff.min(height.saturating_sub(1) / 2);
    let top = buf.viewport_top();
    let highest = buf.line_count().saturating_sub(height);
    if buf.folds.any_closed() {
        // With a closed fold, line numbers no longer count rows, so rows are
        // counted.
        scroll_rows_to(buf, line, 0, content_cols);
    } else if line < top + margin {
        buf.set_viewport_top(line.saturating_sub(margin));
    } else if line + margin >= top + height {
        buf.set_viewport_top((line + margin + 1 - height).min(highest));
    }
    // In drawn columns, so a tabular file scrolls by what's on screen.
    let display = display_row(buf, line, tabular_layout(buf).as_ref());
    let cursor_col = col_at_cell(&display.cells, display.cell_of[col.min(buf.line_len(line))]);
    let width = content_cols.max(1);
    // `sidescrolloff`, capped at half the pane like `scrolloff`.
    let margin = buf.wrap.sidescrolloff.min(width.saturating_sub(1) / 2);
    if cursor_col < buf.viewport_left() + margin {
        buf.set_viewport_left(cursor_col.saturating_sub(margin));
    } else if cursor_col + margin >= buf.viewport_left() + width {
        buf.set_viewport_left(cursor_col + margin + 1 - width);
    }
}

// How far one horizontal wheel notch moves the view, more than
// `MOUSE_WHEEL_LINES` since a column is a small step. Untested with a
// trackpad's sideways gesture.
pub(crate) const MOUSE_WHEEL_COLUMNS: usize = 6;

// Moves the view sideways by `columns` (negative for left), as the
// horizontal wheel and vim's `zh`/`zl` do, then brings the cursor into
// view rather than the view to the cursor. A no-op while `wrap` is on.
pub(crate) fn scroll_horizontally(buf: &mut TextBuffer, columns: isize, content_cols: usize) {
    if buf.wrap.wrap || columns == 0 {
        return;
    }
    let width = content_cols.max(1);
    let layout = tabular_layout(buf);
    // Only as far as the widest visible line, which is all the reader can
    // be scrolling towards.
    let last = (buf.viewport_top() + buf.viewport_height()).min(buf.line_count());
    let widest = (buf.viewport_top()..last)
        .map(|line| {
            let row = display_row(buf, line, layout.as_ref());
            col_at_cell(&row.cells, row.cells.len())
        })
        .max()
        .unwrap_or(0);
    let furthest = widest.saturating_sub(width);
    let left = (buf.viewport_left() as isize + columns).clamp(0, furthest as isize) as usize;
    buf.set_viewport_left(left);

    // And move the cursor along, keeping the `sidescrolloff` margin, so the
    // next key doesn't snap the view back.
    let (line, col) = buf.cursor();
    let display = display_row(buf, line, layout.as_ref());
    let cursor_col = col_at_cell(&display.cells, display.cell_of[col.min(buf.line_len(line))]);
    let margin = buf.wrap.sidescrolloff.min(width.saturating_sub(1) / 2);
    let lowest = left + margin;
    let highest = (left + width).saturating_sub(margin + 1);
    let target = cursor_col.clamp(lowest.min(highest), highest);
    if target != cursor_col {
        let cell = cell_at_col(&display.cells, target);
        buf.set_cursor(line, source_at_or_before(&display, cell).min(buf.line_len(line)));
    }
}

// The same in visual rows. Horizontal scroll is zero: a wrapped line has
// nothing off to the side.
fn scroll_wrapped(buf: &mut TextBuffer, content_cols: usize) {
    buf.set_viewport_left(0);
    let (line, col) = buf.cursor();
    // A closed fold is drawn as one row, whatever its first line's width.
    let seg = match buf.folds.closed_at(line) {
        Some(_) => 0,
        None => crate::bishedit::wrap::segment_of(&line_segments(buf, line, content_cols), col),
    };
    scroll_rows_to(buf, line, seg, content_cols);
}

// Moves the window just enough to show visual row `seg` of `line` with
// `scrolloff` rows around it, counting the rows the screen draws, so
// wrapped lines and closed folds count as what they take up.
fn scroll_rows_to(buf: &mut TextBuffer, line: usize, seg: usize, content_cols: usize) {
    let height = buf.viewport_height().max(1);

    // `scrolloff` in visual rows, which are what it protects when lines
    // wrap.
    let margin = buf.wrap.scrolloff.min(height.saturating_sub(1) / 2);
    let back_from = |buf: &TextBuffer, from: (usize, usize), steps: usize| {
        let mut at = from;
        for _ in 0..steps {
            let previous = previous_row(buf, at, content_cols);
            if previous == at {
                break;
            }
            at = previous;
        }
        at
    };

    // Above the window: the cursor's row goes `margin` rows below the top.
    let above = line < buf.viewport_top() || (line == buf.viewport_top() && seg < buf.viewport_sub());
    if above || rows_between(buf, (buf.viewport_top(), buf.viewport_sub()), (line, seg), margin, content_cols) < margin {
        let top = back_from(buf, (line, seg), margin);
        buf.set_viewport_top(top.0);
        buf.set_viewport_sub(top.1);
        return;
    }
    // Below it: walk from the top, and if the cursor isn't within the
    // height minus the margin, walk back from it to put it `margin` rows
    // above the bottom.
    let reach = height.saturating_sub(margin);
    let mut at = (buf.viewport_top(), buf.viewport_sub());
    for _ in 0..reach {
        if at == (line, seg) {
            return;
        }
        at = next_row(buf, at, content_cols);
    }
    let top = back_from(buf, (line, seg), reach.saturating_sub(1));
    buf.set_viewport_top(top.0);
    buf.set_viewport_sub(top.1);
}

// How many visual rows `to` is below `from`, giving up past `limit`,
// since callers only ask whether it's at least that far.
fn rows_between(buf: &TextBuffer, from: (usize, usize), to: (usize, usize), limit: usize, content_cols: usize) -> usize {
    let mut at = from;
    for n in 0..limit {
        if at == to {
            return n;
        }
        let next = next_row(buf, at, content_cols);
        if next == at {
            break;
        }
        at = next;
    }
    limit
}

// Both walk the rows `visible_rows` draws, so scrolling agrees with the
// screen about closed folds.
fn next_row(buf: &TextBuffer, (line, sub): (usize, usize), content_cols: usize) -> (usize, usize) {
    if let Some((_, end)) = buf.folds.closed_at(line) {
        return (end + 1, 0);
    }
    if sub + 1 < line_segments(buf, line, content_cols).len() { (line, sub + 1) } else { (line + 1, 0) }
}

fn previous_row(buf: &TextBuffer, (line, sub): (usize, usize), content_cols: usize) -> (usize, usize) {
    if sub > 0 {
        return (line, sub - 1);
    }
    if line == 0 {
        return (0, 0);
    }
    let above = line - 1;
    if let Some((start, _)) = buf.folds.closed_at(above) {
        return (start, 0);
    }
    (above, line_segments(buf, above, content_cols).len().saturating_sub(1))
}

// The buffer position a click at screen row/column `row0`/`col0`
// (0-based) lands on, for a buffer drawn in `rect`: the inverse of
// `build_editor_frame`'s placement (gutter, viewport, `char_at_col`), so
// they agree about what's under the pointer. `None` outside `rect` or
// below the last line; a click in the gutter or past a line's end lands
// on the nearest position on that line.
pub(crate) fn position_at_screen(buf: &TextBuffer, rect: Rect, row0: usize, col0: usize) -> Option<(usize, usize)> {
    if row0 < rect.row || col0 < rect.col || col0 >= rect.col + rect.cols {
        return None;
    }
    let row = row0 - rect.row;
    if row >= editor_content_rows(rect) {
        return None;
    }
    let gutter = total_gutter_width(buf).min(rect.cols.saturating_sub(1));
    let content_cols = rect.cols - gutter;
    // The rows the frame drew, so a click can't land on a line not shown
    // there.
    let rows = visible_rows(buf, content_cols, editor_content_rows(rect));
    let visual = rows.get(row)?;
    let line = visual.line;
    let x = (col0 - rect.col).saturating_sub(gutter);
    let chars = buf.line_chars(line);
    let display = display_row(buf, line, tabular_layout(buf).as_ref());
    let cell = if buf.wrap.wrap {
        // In a wrapped row, an offset into its segment, past the continuation
        // prefix.
        let into = x.saturating_sub(visual.seg.indent);
        let base = col_at_cell(&display.cells, display.cell_of[visual.seg.start.min(chars.len())]);
        cell_at_col(&display.cells, base + into)
    } else {
        cell_at_col(&display.cells, buf.viewport_left() + x)
    };
    // A click on padding belongs to the character before it.
    let col = source_at_or_before(&display, cell);
    let col = if buf.wrap.wrap { col.min(visual.seg.end.saturating_sub(1).max(visual.seg.start)) } else { col };
    // Normal mode's cursor can't sit past a line's last character, so an
    // overshooting click lands on it.
    Some((line, col.min(chars.len().saturating_sub(1))))
}

// The columns left for text after the gutter: `build_editor_frame`'s
// `content_cols`, for callers of `scroll_to_show_cursor`.
pub(crate) fn editor_content_cols(buf: &TextBuffer, rect: Rect) -> usize {
    let gutter_width = total_gutter_width(buf).min(rect.cols.saturating_sub(1));
    rect.cols - gutter_width
}

// `K`'s hover popup, shared by debugger.rs and the file editor (docs.rs's
// `hover_lines` is the content). Below the cursor (screen-relative),
// above when there's no room, clamped inside `rect`.
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
    let top = if cursor_row + 1 + box_height <= bottom_limit { cursor_row + 1 } else { cursor_row.saturating_sub(box_height).max(rect.row) };
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

// The completion popup: a list under the cursor. Reverse video like
// `render_hover_popup`, to be legible over text without assuming
// colours; the selection is marked with `>` and bold, not colour alone.
pub(crate) fn render_completion_popup(
    items: &[crate::bishedit::completion::EditorCompletion],
    selected: usize,
    cursor_row: usize,
    cursor_col: usize,
    rect: Rect,
) -> String {
    let rows: Vec<(String, String)> = items.iter().map(|i| (i.label.clone(), i.detail.clone())).collect();
    render_popup_list(&rows, selected, cursor_row, cursor_col, rect)
}

/// The same widget over plain `(label, detail)` rows, for a code action
/// picker; completion is built on it.
pub(crate) fn render_popup_list(items: &[(String, String)], selected: usize, cursor_row: usize, cursor_col: usize, rect: Rect) -> String {
    const MAX_ROWS: usize = 10;
    if items.is_empty() {
        return String::new();
    }
    // Scrolled to keep the selection visible: a server can answer with
    // hundreds.
    let first = selected.saturating_sub(MAX_ROWS - 1);
    let shown: Vec<&(String, String)> = items.iter().skip(first).take(MAX_ROWS).collect();
    let max_width = rect.cols.saturating_sub(4).clamp(10, 60);
    let label_width = shown.iter().map(|i| i.0.chars().count()).max().unwrap_or(1);
    let detail_width = shown.iter().map(|i| i.1.chars().count()).max().unwrap_or(0);
    // `> ` marker, the label, and the detail with a gap before it.
    let inner_width = (2 + label_width + if detail_width > 0 { detail_width + 2 } else { 0 }).min(max_width).max(1);
    let box_width = (inner_width + 2).min(rect.cols.max(3));
    let box_height = shown.len() + 2;

    let bottom_limit = rect.row + rect.rows;
    let top = if cursor_row + 1 + box_height <= bottom_limit { cursor_row + 1 } else { cursor_row.saturating_sub(box_height).max(rect.row) };
    let left = cursor_col.min((rect.col + rect.cols).saturating_sub(box_width));

    let mut out = String::new();
    out.push_str(&format!("\x1b[{};{}H\x1b[7m\u{256d}{}\u{256e}\x1b[0m", top + 1, left + 1, "\u{2500}".repeat(box_width.saturating_sub(2))));
    for (i, item) in shown.iter().enumerate() {
        let is_selected = first + i == selected;
        let marker = if is_selected { "> " } else { "  " };
        let mut text = format!("{marker}{}", item.0);
        if !item.1.is_empty() {
            let used = text.chars().count();
            if inner_width > used + 2 {
                text.push_str(&" ".repeat(inner_width - used - item.1.chars().count().min(inner_width - used)));
                text.push_str(&item.1);
            }
        }
        let padded: String = format!("{text:<inner_width$}").chars().take(inner_width).collect();
        let weight = if is_selected { "\x1b[7;1m" } else { "\x1b[7m" };
        out.push_str(&format!("\x1b[{};{}H{weight}\u{2502}{padded}\u{2502}\x1b[0m", top + 2 + i, left + 1));
    }
    out.push_str(&format!("\x1b[{};{}H\x1b[7m\u{2570}{}\u{256f}\x1b[0m", top + box_height, left + 1, "\u{2500}".repeat(box_width.saturating_sub(2))));
    out
}

// Character-level wrap of one hover line into `width`-wide rows, enough
// for doc comments and man snippets.
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

// `d{motion}`/`c{motion}`: removes the motion's range from the cursor
// (`TextBuffer::delete_range`) into a register. Returns whether anything
// was deleted; `Change` enters Insert only then.
pub(crate) fn delete_motion(
    buf: &mut TextBuffer,
    registers: &mut Registers,
    m: motion::Motion,
    count: Option<usize>,
    register: Option<char>,
) -> bool {
    let Some(range) = motion::motion_range(buf, m, count) else {
        return false;
    };
    let shape = match range.shape {
        motion::MotionShape::Linewise => RegisterShape::Line,
        motion::MotionShape::Blockwise => RegisterShape::Block,
        _ => RegisterShape::Char,
    };
    let text = buf.delete_range(&range);
    registers.record_delete(register, RegisterValue { text, shape });
    true
}

// `dd`: exactly `count` lines from the cursor, clamped as
// `motion::whole_lines` clamps, so the register and the removed range
// agree.
pub(crate) fn delete_lines(buf: &mut TextBuffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
    let count = count.unwrap_or(1).max(1);
    let text = motion::whole_lines(buf, count);
    registers.record_delete(register, RegisterValue { text, shape: RegisterShape::Line });
    let (row, last) = motion::line_span(&*buf, count);
    let range = motion::MotionRange { shape: motion::MotionShape::Linewise, from: (row, 0), to: (last, 0) };
    buf.delete_range(&range);
}

// `cc`/`S`: the lines' text goes, and the first line stays, as in vim;
// the register gets the lines linewise, as from `dd`. The cursor lands
// after the rebuilt indent (`autoindent_for`), as `o` and `O` do, unlike
// `vim -u NONE`: see the corpus's `cc-keeps-the-indent` divergence.
pub(crate) fn change_lines(buf: &mut TextBuffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
    let count = count.unwrap_or(1).max(1);
    let text = motion::whole_lines(buf, count);
    registers.record_delete(register, RegisterValue { text, shape: RegisterShape::Line });
    let (row, last) = motion::line_span(&*buf, count);
    let indent = autoindent_for(buf, row);
    // From the start of the first line to the end of the last, inclusive:
    // empties the lines and joins what's left into one.
    let end_col = buf.line_len(last);
    if end_col > 0 || last > row {
        let to = if end_col > 0 { (last, end_col - 1) } else { (last, 0) };
        buf.delete_range(&motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (row, 0), to });
    }
    buf.set_cursor(row, 0);
    if !indent.is_empty() {
        buf.insert_text((row, 0), &indent);
        buf.set_cursor(row, indent.chars().count());
    }
}

// The selection between two carets as an `Inclusive` range, so an
// Insert-mode selection renders, yanks and deletes like a Visual one.
// Carets sit between characters, so the later end steps back one:
// dragging from before `a` to before `c` selects `ab`. `None` when they
// coincide.
fn selection_between(buf: &TextBuffer, a: (usize, usize), b: (usize, usize)) -> Option<motion::MotionRange> {
    let (from, to) = if a <= b { (a, b) } else { (b, a) };
    if from == to {
        return None;
    }
    let to = if to.1 > 0 {
        (to.0, to.1 - 1)
    } else {
        // A caret at a line's start ends the selection at the end of the line
        // before.
        let previous = to.0.checked_sub(1)?;
        (previous, buf.line_len(previous))
    };
    if to < from {
        return None;
    }
    Some(motion::MotionRange { shape: motion::MotionShape::Inclusive, from, to })
}

// One level of indent, as the characters this buffer indents with: a
// tab when `expandtab` is off, `shiftwidth` spaces otherwise.
fn one_indent(buf: &TextBuffer) -> String {
    if buf.expandtab { " ".repeat(buf.shiftwidth) } else { "\t".to_string() }
}

// How many leading characters of `row` make one indent's worth of
// columns, which `<<` removes. A tab counts a whole tabstop; a line
// indented less loses what it has, as in vim.
fn outdent_chars(buf: &TextBuffer, row: usize) -> usize {
    let mut columns = 0;
    let mut chars = 0;
    while columns < buf.shiftwidth {
        match buf.char_at(row, chars) {
            Some(' ') => columns += 1,
            Some('\t') => columns += buf.tabstop - (columns % buf.tabstop),
            _ => break,
        }
        chars += 1;
    }
    chars
}

// One indent to every non-empty line in `from_row..=to_row` (vim never
// indents an empty line). `>` and `<` are always linewise
// (`vimkeys::Op::Indent`), so only the rows matter.
pub(crate) fn indent_rows(buf: &mut TextBuffer, from_row: usize, to_row: usize) {
    for row in from_row..=to_row {
        if buf.line_len(row) == 0 {
            continue;
        }
        buf.insert_text((row, 0), &one_indent(buf));
    }
}

// `<`'s counterpart: strips up to one indent's worth of columns from
// each line in range (see `outdent_chars`).
pub(crate) fn outdent_rows(buf: &mut TextBuffer, from_row: usize, to_row: usize) {
    for row in from_row..=to_row {
        // In columns, not characters: with `expandtab` off one tab is a whole
        // indent.
        let strip = outdent_chars(buf, row);
        if strip == 0 {
            continue;
        }
        let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, 0), to: (row, strip) };
        buf.delete_range(&range);
    }
}

// `>{motion}`/`<{motion}`: the motion resolves as for any operator (an
// empty one is a no-op), but only its rows are used. Unlike vim, an
// exclusive motion ending at column 0 of a line still includes that
// line (`>j` from a line's start).
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

// `>>`/`<<`: `count` lines from the cursor, each shifted once (`3>>` is
// three lines, not three levels), as in vim.
pub(crate) fn indent_lines(buf: &mut TextBuffer, count: Option<usize>) {
    let count = count.unwrap_or(1).max(1);
    let (row, last) = motion::line_span(&*buf, count);
    indent_rows(buf, row, last);
    buf.set_cursor(row, 0);
}

pub(crate) fn outdent_lines(buf: &mut TextBuffer, count: Option<usize>) {
    let count = count.unwrap_or(1).max(1);
    let (row, last) = motion::line_span(&*buf, count);
    outdent_rows(buf, row, last);
    buf.set_cursor(row, 0);
}

// Visual `>`/`<`: shifts every line any selection touches (the active
// one is already in `buf.selections`), linewise whatever the shape. No
// need to go in reverse as `delete_selections` does: changing leading
// whitespace never moves another line.
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

// `x`: deletes up to `count` grapheme clusters from the cursor, to the
// end of the line, so one `x` on a ZWJ emoji sequence removes the whole
// glyph.
pub(crate) fn delete_char_forward(buf: &mut TextBuffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
    let (row, col) = buf.cursor();
    let len = buf.line_len(row);
    if len == 0 {
        return;
    }
    let chars = buf.line_chars(row);
    // From the cluster's start, in case the cursor is somehow inside one.
    let start = crate::bishedit::grapheme::cluster_range(&chars, col.min(len - 1)).0;
    let mut end = start;
    for _ in 0..count.unwrap_or(1).max(1) {
        end = crate::bishedit::grapheme::next_boundary(&chars, end);
    }
    let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, start), to: (row, end) };
    let deleted = buf.delete_range(&range);
    if !deleted.is_empty() {
        registers.record_delete(register, RegisterValue { text: deleted, shape: RegisterShape::Char });
    }
}

// `ys{motion}`/`yss`'s target. A motion resolves as for any operator
// (`None` when empty); `yss` spans `count` lines from the cursor, as
// `dd` does.
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

// `ys{motion}{ch}`/`yss{ch}`: wraps the range in `ch`'s pair, close
// first, so the open position doesn't move (see
// `motion::surround_insert_points`). The cursor lands on the opening
// delimiter, as in vim-surround.
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

// `ds{ch}`: removes the nearest enclosing pair `ch` names, and the
// padding `motion::surround_delete_spans` strips, close side first. A
// no-op when there's no such pair.
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

// `cs{ch}{replacement}`: replaces the pair's two delimiters with
// `replacement`'s, padding untouched, close side first.
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

// Visual `S{ch}`: wraps every selection in `ch`'s pair, as editor.rs's
// `surround_selections` does on one line.
pub(crate) fn surround_selections(buf: &mut TextBuffer, ch: char) {
    if buf.selections.is_empty() {
        return;
    }
    let Some((open, close)) = motion::surround_delims(ch) else {
        return;
    };
    let mut ranges = buf.selections.clone();
    crate::sort::sort_by_key(&mut ranges, |r| std::cmp::Reverse(r.from));
    let mut leftmost_open_at = (0, 0);
    for range in &ranges {
        let (open_at, close_at) = motion::surround_insert_points(buf, range);
        buf.insert_text(close_at, &close);
        buf.insert_text(open_at, &open);
        leftmost_open_at = open_at;
    }
    buf.set_cursor(leftmost_open_at.0, leftmost_open_at.1);
}

// `r{ch}`: replaces `count` characters from the cursor with `ch`; never
// crosses a line break, and refuses if too few remain (as editor.rs's
// `replace_char`).
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

// `Ctrl-A`/`Ctrl-X`, as editor.rs's `adjust_number`.
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
        Op::Yank | Op::Delete | Op::Change | Op::Indent | Op::Outdent | Op::Fold => {
            unreachable!("case_kind_for_op is only ever called for Op::Lowercase/Uppercase/CaseToggle")
        }
    }
}

// `gu`/`gU`/`g~` over a range, and `guu`/`gUU`/`g~~` over lines.
// Linewise ranges are replaced a row at a time, without newlines:
// reinserting the extracted text, with its trailing newline, would
// split off a blank line.
fn case_operator_range(buf: &mut TextBuffer, range: &motion::MotionRange, kind: motion::CaseKind) {
    if range.shape == motion::MotionShape::Linewise {
        for row in range.from.0..=range.to.0 {
            let len = buf.line_len(row);
            if len == 0 {
                continue;
            }
            // `Inclusive` to the last character, not `Exclusive` past it:
            // `motion::extract_text` never lands on the past-the-end
            // column, so an `Exclusive` range ending there would walk into
            // the next line, and this reinserts that text.
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
    let (row, last) = motion::line_span(&*buf, count);
    let range = motion::MotionRange { shape: motion::MotionShape::Linewise, from: (row, 0), to: (last, 0) };
    case_operator_range(buf, &range, kind);
}

// `~`: toggles the case of `count` characters from the cursor and moves
// past them, as editor.rs's `toggle_case`.
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

// `p`/`P`: a linewise register pastes as whole lines below or above the
// cursor's line, a charwise one inline. Both through `insert_text`, by
// bracketing the text with newlines and choosing the column.
pub(crate) fn put(buf: &mut TextBuffer, registers: &mut Registers, before: bool, count: Option<usize>, register: Option<char>) {
    let value = registers.read(register);
    if value.text.is_empty() {
        return;
    }
    let count = count.unwrap_or(1).max(1);
    let (row, col) = buf.cursor();
    let repeated: String = value.text.repeat(count);
    match value.shape {
        // Whole lines go above or below a closed fold, never into it, as with
        // `o` and `O`.
        RegisterShape::Line => {
            let body = repeated.strip_suffix('\n').unwrap_or(&repeated);
            let (first, last) = buf.fold_span(row);
            if before {
                buf.insert_text((first, 0), &format!("{body}\n"));
                buf.set_cursor(first, 0);
            } else {
                let end = buf.line_len(last);
                buf.insert_text((last, end), &format!("\n{body}"));
                buf.set_cursor(last + 1, 0);
            }
        }
        RegisterShape::Char => {
            let insert_col = if before { col } else { (col + 1).min(buf.line_len(row)) };
            let new_cursor = buf.insert_text((row, insert_col), &repeated);
            // The cursor ends on the last inserted character, as in
            // `vimkeys::apply_put`.
            buf.set_cursor(new_cursor.0, new_cursor.1.saturating_sub(1));
        }
        // A block goes back as a rectangle, each line at the same column on
        // consecutive lines; that's what makes `Block` a register shape. A
        // line too short is padded to the column, as vim does.
        RegisterShape::Block => {
            let insert_col = if before { col } else { (col + 1).min(buf.line_len(row)) };
            let piece: Vec<&str> = value.text.split('\n').collect();
            for (offset, chunk) in piece.iter().enumerate() {
                let line = row + offset;
                if line >= buf.line_count() {
                    let last = buf.line_count() - 1;
                    buf.insert_text((last, buf.line_len(last)), "\n");
                }
                let len = buf.line_len(line);
                let text = chunk.repeat(count);
                if len < insert_col {
                    buf.insert_text((line, len), &" ".repeat(insert_col - len));
                }
                buf.insert_text((line, insert_col), &text);
            }
            buf.set_cursor(row, insert_col);
        }
    }
}

// vim's `cw` acts like `ce`; see editor.rs's `redirect_cw_to_ce`.
pub(crate) fn redirect_cw_to_ce(buf: &TextBuffer, m: &motion::Motion) -> motion::Motion {
    let (row, col) = buf.cursor();
    let on_word_char = matches!(buf.char_at(row, col), Some(c) if !c.is_whitespace());
    match m {
        motion::Motion::WordForward if on_word_char => motion::Motion::WordEnd,
        motion::Motion::WordForwardBig if on_word_char => motion::Motion::WordEndBig,
        other => other.clone(),
    }
}

// `o`/`O`: a newline at the end (`o`) or start (`O`) of the line, through
// `insert_text`. For `O` the returned cursor is the pushed-down line, so
// it's moved to the new blank one.
pub(crate) fn open_line(buf: &mut TextBuffer, above: bool) {
    // On a closed fold, above or below the whole fold: a line opened inside
    // it would be hidden.
    let (first, last) = buf.fold_span(buf.cursor().0);
    let row = if above { first } else { last };
    let indent = autoindent_for(buf, row);
    if above {
        buf.insert_text((row, 0), "\n");
        buf.set_cursor(row, 0);
    } else {
        let len = buf.line_len(row);
        buf.insert_text((row, len), "\n");
    }
    let (row, _) = buf.cursor();
    if !indent.is_empty() {
        buf.insert_text((row, 0), &indent);
        buf.set_cursor(row, indent.chars().count());
    }
}

/// The whitespace a line opened from `row` starts with: `row`'s leading
/// whitespace, copied. Plain `autoindent`, not `smartindent`: no guessing
/// at blocks after `{` or `then`, so never wrong in a surprising way.
/// Empty for a whitespace-only line, so Enter on a blank line leaves no
/// trailing spaces.
pub(crate) fn autoindent_for(buf: &TextBuffer, row: usize) -> String {
    let chars = buf.line_chars(row);
    if chars.iter().all(|c| c.is_whitespace()) {
        return String::new();
    }
    chars.iter().take_while(|c| **c == ' ' || **c == '\t').collect()
}

// Places the cursor for an `InsertCmd` before `run_insert_mode` starts,
// removing text first for `s`, `S` and `C` (into a register, as any
// delete). `S` clears the current line, not the buffer.
pub(crate) fn resolve_insert_start(buf: &mut TextBuffer, cmd: InsertCmd) {
    let (row, col) = buf.cursor();
    match cmd {
        InsertCmd::Before => {}
        InsertCmd::After => buf.set_cursor(row, (col + 1).min(buf.line_len(row))),
        // `I` inserts at the first non-blank, where `^` goes, not in front of
        // the indentation.
        InsertCmd::LineStart => {
            let col = motion::first_non_blank(buf, row);
            buf.set_cursor(row, col);
        }
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
        // Lines scrolled per wheel event, here, in repl.rs's Normal mode
        // and in the pager. One, because terminals report distance, not
        // notches (kitty sends an event per cell height travelled), so no
        // constant could mean one notch. The wheel moves the cursor and the
        // view follows it.
        InsertCmd::LastInsertPos => {
            let (target_row, target_col) = buf.get_mark('^').unwrap_or((row, col));
            let target_row = target_row.min(buf.line_count() - 1);
            let target_col = target_col.min(buf.line_len(target_row));
            buf.set_cursor(target_row, target_col);
        }
    }
}

// Lines scrolled per wheel event, here, in repl.rs's Normal mode and in
// the pager. One, because terminals report distance, not notches: kitty
// sends one event per cell height travelled (two to four per turn), so
// no constant here can mean one notch. The wheel moves the cursor, as
// the arrow keys kitty used to translate it into did, so the view
// follows rather than leads.
pub(crate) const MOUSE_WHEEL_LINES: usize = 1;

// What one idle tick reported: `None` for nothing, `Some` for repaint
// into this geometry. Together because a repaint is mostly due to a
// resize, when the geometry measured before Insert mode is stale.
// `compositor_redraw` paints a driven `Frame::Edit` blank (see
// `service_background_jobs`), so the caller must repaint.
#[derive(Clone, Copy)]
pub(crate) struct IdleRedraw {
    pub(crate) rect: Rect,
    pub(crate) term_rows: usize,
    pub(crate) term_cols: usize,
}

/// What Insert mode needs from whoever drives it. One object rather
/// than two closures, since both need the same mutable state and two
/// closures would borrow it twice. `idle` runs while stdin is quiet;
/// `complete` answers Ctrl-N, as nothing without a language server.
pub(crate) trait InsertServices {
    fn idle(&mut self, buf: &mut TextBuffer) -> Option<IdleRedraw>;
    fn complete(&mut self, _buf: &TextBuffer, _row: usize, _col: usize) -> Vec<crate::bishedit::completion::EditorCompletion> {
        Vec::new()
    }
    /// The signature of the call being typed here, as popup lines, or `None`
    /// to show nothing (closing what's open). `typed` is the character just
    /// inserted, `None` when something else moved the cursor; the
    /// implementor decides whether it's a trigger, since that's the server's
    /// `signatureHelpProvider.triggerCharacters`.
    fn signature(&mut self, _buf: &TextBuffer, _row: usize, _col: usize, _typed: Option<char>) -> Option<Vec<String>> {
        None
    }

    /// Whether Insert mode waits on the terminal once queued keys are spent.
    /// Real editing does: a macro ending mid-insert leaves you in Insert, as
    /// in vim. Tests replaying a macro with no terminal answer false, or
    /// they'd hang waiting on fd 0.
    fn awaits_live_input(&self) -> bool {
        true
    }
}

/// For a caller with nothing to offer: no idle work, no completions. Only
/// this module's tests; real callers have a shell.
#[cfg(test)]
pub(crate) struct NoInsertServices;

#[cfg(test)]
impl InsertServices for NoInsertServices {
    fn idle(&mut self, _buf: &mut TextBuffer) -> Option<IdleRedraw> {
        None
    }

    fn awaits_live_input(&self) -> bool {
        false
    }
}

// The completion popup while it's open. Fetched once and filtered as
// you type, so it's instant: a snapshot of the server's answer.
// `isIncomplete` is read and ignored (see `lsp::completions`).
struct LiveCompletion {
    /// Everything the server offered, in its own order.
    all: Vec<crate::bishedit::completion::EditorCompletion>,
    /// Indices into `all` still matching what has been typed.
    shown: Vec<usize>,
    /// Index into `shown`.
    selected: usize,
    /// Where the completed word starts, on `row`, captured at opening so the
    /// filter sees what's been typed since.
    row: usize,
    word_start: usize,
}

impl LiveCompletion {
    // Narrows to candidates still matching the word under the cursor. False
    // when none do, and the caller closes the popup.
    fn refilter(&mut self, buf: &TextBuffer) -> bool {
        let (row, col) = buf.cursor();
        if row != self.row || col < self.word_start {
            return false;
        }
        let line = buf.line_chars(row);
        let typed: String = line[self.word_start..col.min(line.len())].iter().collect();
        let typed = typed.to_lowercase();
        self.shown = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, item)| typed.is_empty() || item.label.to_lowercase().starts_with(&typed))
            .map(|(i, _)| i)
            .collect();
        self.selected = self.selected.min(self.shown.len().saturating_sub(1));
        !self.shown.is_empty()
    }

    fn items(&self) -> Vec<crate::bishedit::completion::EditorCompletion> {
        self.shown.iter().filter_map(|i| self.all.get(*i).cloned()).collect()
    }

    fn chosen(&self) -> Option<&crate::bishedit::completion::EditorCompletion> {
        self.all.get(*self.shown.get(self.selected)?)
    }
}

// Insert mode's loop, once `resolve_insert_start` (or a `c` delete) has
// placed the cursor. Escape, Ctrl-C, Ctrl+G and EOF all end it, back to
// `run_normal_mode_navigation`. `replace` (`R`) makes typing and
// Backspace overtype. `term_rows`/`term_cols` are copies: `on_idle`
// holds the originals, so a resize applies when this returns.
// `extra_cursors` are the other positions a multi-selection `c` types
// into; navigation moves only the main cursor, edits apply to all.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_insert_mode(
    buf: &mut TextBuffer,
    vk: &mut VimKeys,
    rect: Rect,
    registers: &mut Registers,
    // The buffer is handed in, not closed over: the caller holds it
    // mutably for the whole call, and an idle tick syncing the document to
    // a language server (`sync_language_server_document`) needs to see it.
    services: &mut dyn InsertServices,
    replace: bool,
    term_rows: usize,
    term_cols: usize,
    color_overrides: Option<&highlight::ColorOverrides>,
    extra_cursors: &[(usize, usize)],
    // The shell's abbreviations, narrowed here to this file's language (see
    // `language_of`, `abbr --lang=`). Empty without a shell.
    abbrs: &[Abbr],
) -> io::Result<()> {
    let mode = if replace { EditorMode::Replace } else { EditorMode::Insert };
    // Mutable: an idle tick may report new geometry (`IdleRedraw`), which
    // later renders must use.
    let (mut rect, mut term_rows, mut term_cols) = (rect, term_rows, term_cols);
    let mut cursors: Vec<(usize, usize)> = std::iter::once(buf.cursor()).chain(extra_cursors.iter().copied()).collect();
    // Where a mouse drag started, while one is in progress. Insert mode has
    // its own selection, as GUI editors do, rather than switching to
    // Visual.
    let mut drag_anchor: Option<(usize, usize)> = None;
    // Once per session: neither the language nor the snapshot table can
    // change while typing.
    let abbrs = snippet::for_language(abbrs, &language_of(buf));
    let mut live: Option<LiveSnippet> = None;
    let mut completing: Option<LiveCompletion> = None;
    // The signature of the call being typed, while there is one; see
    // `InsertServices::signature`.
    let mut signature: Option<Vec<String>> = None;
    // Before the first frame too: `a`, `o` or `A` can put the cursor outside
    // the viewport, and the first character would go in out of sight.
    // Folds around the cursor's line open, as vim keeps them in Insert mode,
    // since a closed fold can't show what's typed into it.
    let line = buf.cursor().0;
    buf.folds.open_all(line);
    buf.set_viewport_height(editor_content_rows(rect));
    scroll_to_show_cursor(buf, editor_content_cols(buf, rect));
    render_editor_frame(buf, vk, mode, rect, term_rows, term_cols, color_overrides);
    // `"."`'s text for this session (`Registers::set_last_insert`).
    // Backspace pops the last character even when erasing older text,
    // which vim tracks and this doesn't.
    let mut inserted = String::new();
    // Whether input is a bracketed paste (`Key::PasteStart`). Autoindent and
    // `abbr` are off meanwhile: pasted lines would staircase, and pasted
    // code could trip an abbreviation.
    let mut pasting = false;
    // Bracketed paste on for the duration. `sync_bracketed_paste` (repl.rs)
    // does it for a foreground job; the two never overlap.
    let _paste_guard = term::BracketedPasteGuard::enable();
    let live_input = services.awaits_live_input();
    loop {
        // Waits for input before `vk.next_key`, rather than passing
        // `on_idle` to it: that call borrows `vk` exclusively, so `on_idle`
        // couldn't redraw (as in `run_normal_mode_navigation`). Queued keys
        // (a macro, a mapping's expansion) aren't on stdin, so they skip
        // the wait.
        while !vk.has_pending_keys() && live_input && !term::stdin_ready(editor::IDLE_POLL_MS) {
            if let Some(geometry) = services.idle(buf) {
                rect = geometry.rect;
                term_rows = geometry.term_rows;
                term_cols = geometry.term_cols;
                // A shrink can leave the cursor below the viewport, which
                // needs the new height.
                buf.set_viewport_height(editor_content_rows(rect));
                scroll_to_show_cursor(buf, editor_content_cols(buf, rect));
                render_editor_frame(buf, vk, mode, rect, term_rows, term_cols, color_overrides);
            }
        }
        // Through `vk`, so a macro replay crosses into Insert mode with the
        // same `vk`; a byte is known ready, so its `on_idle` never runs.
        // `next_mapped_key` applies `::bish map -m insert`, with the mode
        // given since this loop is what makes it Insert. The raw reads below
        // (`<C-r>{register}`, `<C-o>`) stay unmapped, as Normal mode's
        // `wants_raw_key` does.
        let key = match vk.next_mapped_key("insert", || if live_input { editor::read_key_idle(&mut || {}) } else { Ok(None) })? {
            Some(k) => k,
            None => {
                buf.set_mark('^', buf.cursor());
                registers.set_last_insert(inserted);
                return Ok(());
            }
        };
        // A mapping whose prefix was typed speculatively (`Matcher::feed`)
        // has completed, so those characters are taken back first, through
        // Backspace's own path so multi-cursor and `inserted` stay
        // consistent. Invisible to undo, since an Insert session is one
        // undo step.
        for _ in 0..vk.take_pending_revert() {
            apply_backspace_to_all(buf, &mut cursors);
            buf.set_cursor(cursors[0].0, cursors[0].1);
            inserted.pop();
        }
        // A key outside a live snippet's vocabulary accepts it as it stands,
        // then means what it always does, as at the shell prompt, so the arms
        // below needn't know about snippets.
        if live.is_some()
            && !matches!(
                key,
                Key::Tab | Key::BackTab | Key::CtrlN | Key::CtrlP | Key::Enter | Key::CtrlY | Key::CtrlE | Key::Backspace | Key::Char(_)
            )
        {
            live.take().unwrap().accept(buf);
            cursors[0] = buf.cursor();
            buf.snippet_holes.clear();
        }

        // A key the popup doesn't use closes it, then means what it always
        // does. `Char` and `Backspace` narrow it instead: typing more of a
        // word is how you pick.
        if completing.is_some() && !matches!(key, Key::CtrlN | Key::CtrlP | Key::Enter | Key::Tab | Key::Escape | Key::Char(_) | Key::Backspace) {
            completing = None;
        }

        // Typing a character over a selection replaces it, as Backspace and
        // Delete delete it below. Not with a popup or snippet live, which own
        // `Char`, nor in Replace mode, which keeps the line's length.
        if !buf.selections.is_empty() && !replace && live.is_none() && completing.is_none() && matches!(key, Key::Char(_)) {
            let range = buf.selections[0];
            buf.selections.clear();
            buf.delete_range(&range);
            buf.set_cursor(range.from.0, range.from.1);
            cursors = vec![buf.cursor()];
        }

        match key {
            // The completion popup owns these while it's open.
            Key::CtrlN if completing.is_some() && live.is_none() => {
                let state = completing.as_mut().unwrap();
                if !state.shown.is_empty() {
                    state.selected = (state.selected + 1) % state.shown.len();
                }
            }
            Key::CtrlP if completing.is_some() && live.is_none() => {
                let state = completing.as_mut().unwrap();
                if !state.shown.is_empty() {
                    state.selected = (state.selected + state.shown.len() - 1) % state.shown.len();
                }
            }
            // Escape closes the popup and stays in Insert mode, as in every
            // editor with a completion menu.
            Key::Escape if completing.is_some() => {
                completing = None;
            }
            Key::Enter | Key::Tab if completing.is_some() => {
                let state = completing.take().unwrap();
                if let Some(item) = state.chosen().cloned() {
                    let (row, col) = buf.cursor();
                    // The server's range when it gave one, else the word
                    // being typed.
                    let (row, start, end) = item.replace.unwrap_or((row, state.word_start, col));
                    let line_len = buf.line_len(row);
                    let (start, end) = (start.min(line_len), end.min(line_len).max(start.min(line_len)));
                    // A snippet completion becomes the same live snippet an
                    // `abbr` does, so `fn ${1:name}()` arrives with the
                    // caret in `name`.
                    match item.snippet.then(|| Snippet::parse(&item.insert)).flatten() {
                        Some(snip) => {
                            let replaced: String = buf.line_chars(row)[start..end].iter().collect();
                            live = Some(LiveSnippet::start(snip, row, start, replaced, buf));
                        }
                        None => {
                            // `flatten`, not the raw text: a snippet with
                            // no tabstops still escapes `$` as `\$`.
                            let text = if item.snippet { snippet::flatten(&item.insert) } else { item.insert.clone() };
                            buf.replace_span((row, start), (row, end), &text);
                            buf.set_cursor(row, start + text.chars().count());
                            inserted.push_str(&text);
                        }
                    }
                    cursors[0] = buf.cursor();
                }
            }
            // Opening it, only with no snippet live, where Ctrl-N means the
            // next placeholder.
            Key::CtrlN if live.is_none() => {
                let (row, col) = buf.cursor();
                let line = buf.line_chars(row);
                let word_start = crate::bishedit::completion::find_word_start(&line, col.min(line.len()));
                let all = services.complete(buf, row, col);
                if !all.is_empty() {
                    let mut state = LiveCompletion { all, shown: Vec::new(), selected: 0, row, word_start };
                    if state.refilter(buf) {
                        completing = Some(state);
                    }
                }
            }
            // A live `abbr` snippet owns these keys, as at the shell
            // prompt: Tab advances, Ctrl-E cancels back to the name, Ctrl-Y
            // accepts, Enter advances except on the last placeholder, where
            // it accepts. Each resyncs `cursors[0]`, since the snippet
            // moves the real cursor and the other arms insert at
            // `cursors[0]`.
            Key::Tab | Key::CtrlN if live.is_some() => {
                let state = live.as_mut().unwrap();
                state.snip.advance(false);
                state.sync(buf);
                cursors[0] = buf.cursor();
            }
            Key::BackTab | Key::CtrlP if live.is_some() => {
                let state = live.as_mut().unwrap();
                state.snip.advance(true);
                state.sync(buf);
                cursors[0] = buf.cursor();
            }
            Key::Enter if live.as_ref().is_some_and(|s| !s.snip.at_last()) => {
                let state = live.as_mut().unwrap();
                state.snip.advance(false);
                state.sync(buf);
                cursors[0] = buf.cursor();
            }
            Key::Enter | Key::CtrlY if live.is_some() => {
                live.take().unwrap().accept(buf);
                cursors[0] = buf.cursor();
            }
            Key::CtrlE if live.is_some() => {
                live.take().unwrap().cancel(buf);
                cursors[0] = buf.cursor();
            }
            // Only eats what was typed into the active placeholder, never the
            // snippet's literal text, which the model couldn't put back.
            Key::Backspace if live.is_some() => {
                let state = live.as_mut().unwrap();
                if state.snip.backspace() {
                    state.sync(buf);
                    cursors[0] = buf.cursor();
                }
            }
            Key::Char(c) if live.is_some() => {
                let state = live.as_mut().unwrap();
                state.snip.type_char(c);
                state.sync(buf);
                cursors[0] = buf.cursor();
                inserted.push(c);
            }

            // `^` (`:help '^`) marks where the cursor was when Insert mode
            // last ended, for `gi`. Ctrl-C and Ctrl+G are Escape here.
            Key::CtrlG | Key::CtrlSpace | Key::Escape | Key::CtrlC => {
                // Before the clamp below: `gi` resumes at the exact spot,
                // past the last character included.
                buf.set_mark('^', buf.cursor());
                registers.set_last_insert(inserted);
                // Leaving Insert mode steps the cursor one column left, as
                // vim does, onto the character just typed, so `i`, type,
                // Escape, `x` deletes the last character typed. Column 0
                // stays put.
                let (row, col) = buf.cursor();
                buf.set_cursor(row, col.saturating_sub(1));
                return Ok(());
            }
            // Enter is an abbreviation trigger, as at the shell prompt: it
            // expands instead of inserting a newline, and a second Enter
            // inserts one. Not in Replace mode, which keeps the line's
            // length, and never on pasted text, where a match isn't
            // something you typed.
            Key::Enter if !replace && !pasting && expand_abbr(buf, &abbrs, &mut live) => {
                cursors[0] = buf.cursor();
            }
            Key::Enter => {
                // Autoindent, except for pasted text, which carries its own.
                let indent = if pasting { String::new() } else { autoindent_for(buf, buf.cursor().0) };
                let text = format!("\n{indent}");
                apply_insert_to_all(buf, &mut cursors, &text);
                buf.set_cursor(cursors[0].0, cursors[0].1);
                inserted.push_str(&text);
            }
            // `expandtab` on: spaces to the next `shiftwidth` boundary;
            // off: a tab. Never overtypes, even in Replace mode.
            Key::Tab => {
                let text = if buf.expandtab {
                    let (line, col) = buf.cursor();
                    // To the next boundary in drawn columns, so a tab after
                    // a literal tab lands where it looks like it should.
                    let at = display_column(buf, line, col);
                    " ".repeat(buf.shiftwidth - (at % buf.shiftwidth))
                } else {
                    "\t".to_string()
                };
                apply_insert_to_all(buf, &mut cursors, &text);
                buf.set_cursor(cursors[0].0, cursors[0].1);
                inserted.push_str(&text);
            }
            // Replace mode's Backspace steps back without restoring the
            // character (vim restores it) and doesn't cross lines. Only
            // plain Insert ever has extra cursors, so this moves
            // `cursors[0]` alone.
            Key::Backspace if replace => {
                let (row, col) = buf.cursor();
                if col > 0 {
                    buf.set_cursor(row, col - 1);
                }
                cursors[0] = buf.cursor();
                inserted.pop();
            }
            // With a selection, Backspace deletes it and leaves the caret
            // where it was.
            Key::Backspace if !buf.selections.is_empty() => {
                let range = buf.selections[0];
                buf.selections.clear();
                buf.delete_range(&range);
                buf.set_cursor(range.from.0, range.from.1);
                cursors = vec![buf.cursor()];
            }
            Key::Backspace => {
                apply_backspace_to_all(buf, &mut cursors);
                buf.set_cursor(cursors[0].0, cursors[0].1);
                inserted.pop();
            }
            // Delete: Backspace's forward twin, the selection first.
            // `inserted` isn't touched: what Delete removes lies ahead and
            // was never typed in this session.
            Key::Delete if !buf.selections.is_empty() => {
                let range = buf.selections[0];
                buf.selections.clear();
                buf.delete_range(&range);
                buf.set_cursor(range.from.0, range.from.1);
                cursors = vec![buf.cursor()];
            }
            Key::Delete => {
                apply_delete_to_all(buf, &mut cursors);
                buf.set_cursor(cursors[0].0, cursors[0].1);
            }
            // Ctrl-W deletes the word before the cursor (`:help i_CTRL-W`),
            // through `Motion::WordBackward` and `motion_range` as `db` is,
            // so it crosses lines when `db` does. Primary cursor only.
            // `inserted` isn't touched: it can delete text from before this
            // session.
            Key::CtrlW => {
                if let Some(range) = motion::motion_range(buf, motion::Motion::WordBackward, None) {
                    buf.delete_range(&range);
                    cursors[0] = buf.cursor();
                }
            }
            // Ctrl-U: back to the first non-blank, keeping the indentation;
            // from inside the indentation, to the line's start.
            Key::CtrlU => {
                let (row, col) = buf.cursor();
                let indent = motion::first_non_blank(buf, row);
                let target = if col > indent { indent } else { 0 };
                if target < col {
                    let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, target), to: (row, col) };
                    buf.delete_range(&range);
                    cursors[0] = buf.cursor();
                }
            }
            Key::Left => {
                motion::apply_motion(buf, motion::Motion::Left, None);
                cursors[0] = buf.cursor();
            }
            Key::Right => {
                // `Motion::Right` stops at the last character, Normal
                // mode's limit; Insert mode's cursor may sit one past it,
                // so it's moved directly.
                let (row, col) = buf.cursor();
                buf.set_cursor(row, (col + 1).min(buf.line_len(row)));
                cursors[0] = buf.cursor();
            }
            // The paste brackets insert nothing; they mark what's between
            // them.
            Key::PasteStart => pasting = true,
            Key::PasteEnd => pasting = false,
            // `Home` is column 0, vim's `<Home>`; `^` is the first non-blank.
            Key::Home => {
                motion::apply_motion(buf, motion::Motion::LineStart, None);
                cursors[0] = buf.cursor();
            }
            // Insert mode's cursor may sit past the last character, where
            // `Motion::LineEnd` stops, so it goes there directly, as
            // `Key::Right`.
            Key::End => {
                let (row, _) = buf.cursor();
                buf.set_cursor(row, buf.line_len(row));
                cursors[0] = buf.cursor();
            }
            // Alt-Left/Alt-Right: the `b`/`w` word motions, as most editors
            // bind them. The shell prompt's Alt-Left/Right browse directory
            // history instead.
            Key::AltLeft => {
                motion::apply_motion(buf, motion::Motion::WordBackward, None);
                cursors[0] = buf.cursor();
            }
            Key::AltRight => {
                motion::apply_motion(buf, motion::Motion::WordForward, None);
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
            // PageUp/PageDown as Ctrl-B/Ctrl-F, which vim honours in Insert
            // mode too.
            Key::PageDown => {
                motion::apply_motion(buf, motion::Motion::PageDown, None);
                cursors[0] = buf.cursor();
            }
            Key::PageUp => {
                motion::apply_motion(buf, motion::Motion::PageUp, None);
                cursors[0] = buf.cursor();
            }
            // A click puts the caret there and drops any selection.
            Key::Mouse(ev) if buf.mouse && ev.is_left_click() => {
                let (row0, col0) = ((ev.row as usize).saturating_sub(1), (ev.col as usize).saturating_sub(1));
                if let Some((line, col)) = position_at_screen(buf, rect, row0, col0) {
                    buf.selections.clear();
                    buf.set_cursor(line, col);
                    // Clicking collapses extra cursors: it says where you
                    // want to be.
                    cursors = vec![(line, col)];
                    drag_anchor = Some((line, col));
                }
            }
            // Dragging from it selects, staying in Insert mode.
            // `Inclusive`, so it renders and deletes as a Visual selection
            // does.
            Key::Mouse(ev) if buf.mouse && ev.is_left_drag() => {
                let (row0, col0) = ((ev.row as usize).saturating_sub(1), (ev.col as usize).saturating_sub(1));
                if let Some(anchor) = drag_anchor
                    && let Some(to) = position_at_screen(buf, rect, row0, col0)
                {
                    buf.set_cursor(to.0, to.1);
                    cursors = vec![to];
                    buf.selections = selection_between(buf, anchor, to).into_iter().collect();
                }
            }
            Key::Mouse(ev) if buf.mouse && ev.is_release() => drag_anchor = None,
            // The wheel moves the cursor and the view follows; see
            // `MOUSE_WHEEL_LINES`.
            Key::Mouse(ev) if ev.is_scroll_down() => {
                motion::apply_motion(buf, motion::Motion::Down, Some(MOUSE_WHEEL_LINES));
                cursors[0] = buf.cursor();
            }
            Key::Mouse(ev) if ev.is_scroll_up() => {
                motion::apply_motion(buf, motion::Motion::Up, Some(MOUSE_WHEEL_LINES));
                cursors[0] = buf.cursor();
            }
            // The horizontal wheel (`MouseEvent::is_scroll_left`). It moves
            // the cursor too: a caret off-screen is nowhere to type.
            Key::Mouse(ev) if ev.is_scroll_left() || ev.is_scroll_right() => {
                let columns = if ev.is_scroll_right() { MOUSE_WHEEL_COLUMNS as isize } else { -(MOUSE_WHEEL_COLUMNS as isize) };
                scroll_horizontally(buf, columns, editor_content_cols(buf, rect));
                cursors[0] = buf.cursor();
            }
            // Space is the other abbreviation trigger. A plain expansion
            // keeps the space after it; a snippet swallows it, since the
            // caret is already in the first placeholder.
            Key::Char(' ') if !replace && !pasting && !abbrs.is_empty() && expand_abbr(buf, &abbrs, &mut live) => {
                cursors[0] = buf.cursor();
                if live.is_none() {
                    let mut b = [0u8; 4];
                    apply_insert_to_all(buf, &mut cursors, ' '.encode_utf8(&mut b));
                    buf.set_cursor(cursors[0].0, cursors[0].1);
                    inserted.push(' ');
                }
            }
            Key::Char(c) => {
                // A selection still standing here (Replace mode, or a popup
                // or snippet live) is dropped, not replaced.
                buf.selections.clear();
                let (row, col) = buf.cursor();
                // Replace mode overwrites the character at the cursor, if
                // any, so at the end of the line it extends the line, as
                // vim's `R` does. Only one cursor, as for Replace's
                // Backspace.
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
        // Synced from the model every iteration rather than in each arm: the
        // buffer carries them only so the renderer can mark placeholders
        // (`TextBuffer::snippet_holes`).
        buf.snippet_holes = live.as_ref().map(|live| snippet_holes(live, buf)).unwrap_or_default();
        // Narrowed after the key is applied, to the word as it stands now.
        if let Some(state) = completing.as_mut()
            && !state.refilter(buf)
        {
            completing = None;
        }
        // Asked after the key is applied, about where the cursor is now. Only
        // while typing; an arrow or a Backspace out of the call gets `None`,
        // which closes it.
        {
            let typed = match key {
                Key::Char(c) => Some(c),
                _ => None,
            };
            if typed.is_some() || signature.is_some() {
                let (row, col) = buf.cursor();
                signature = services.signature(buf, row, col, typed);
            }
        }
        scroll_to_show_cursor(buf, editor_content_cols(buf, rect));
        render_editor_frame(buf, vk, mode, rect, term_rows, term_cols, color_overrides);
        // The completion popup wins the space when both want it: you opened
        // it, and keys go to it.
        if let Some(state) = completing.as_ref() {
            let (row, col) = buf.cursor();
            let (screen_row, screen_col) = popup_anchor(buf, rect, row, col);
            print!("{}", render_completion_popup(&state.items(), state.selected, screen_row, screen_col, rect));
            let _ = io::stdout().flush();
        } else if let Some(lines) = signature.as_ref() {
            let (row, col) = buf.cursor();
            let (screen_row, screen_col) = popup_anchor(buf, rect, row, col);
            print!("{}", render_hover_popup(lines, screen_row, screen_col, rect));
            let _ = io::stdout().flush();
        }
    }
}

// Where on screen a popup anchored at this buffer position goes: the
// cursor's cell, in terminal coordinates.
fn popup_anchor(buf: &TextBuffer, rect: Rect, row: usize, col: usize) -> (usize, usize) {
    let chars = buf.line_chars(row);
    let gutter = rect.cols.saturating_sub(editor_content_cols(buf, rect));
    let screen_row = rect.row + row.saturating_sub(buf.viewport_top());
    let screen_col = rect.col + gutter + crate::bishedit::unicode_width::col_of(&chars, col).saturating_sub(buf.viewport_left());
    (screen_row, screen_col)
}

// A live snippet's placeholders in the buffer's (line, column) space,
// for the renderer.
fn snippet_holes(live: &LiveSnippet, buf: &TextBuffer) -> Vec<textbuffer::SnippetHole> {
    live.holes()
        .into_iter()
        // A hole spanning a line break is drawn on its first line, to the end
        // of that line: the renderer marks spans within a line, and a default
        // with a newline is rare.
        .map(|(start, end, active)| textbuffer::SnippetHole {
            line: start.0,
            start: start.1,
            end: if end.0 == start.0 { end.1 } else { buf.line_len(start.0) },
            active,
        })
        .collect()
}

// The file editor's `abbr` expansion, editor.rs's
// `expand_abbr_at_cursor` for a `TextBuffer`. No command-position check:
// a file has no command positions, so an abbreviation expands wherever
// its name is the word ending at the cursor. `true` when something
// expanded, which tells Enter not to add a newline.
fn expand_abbr(buf: &mut TextBuffer, abbrs: &[Abbr], live: &mut Option<LiveSnippet>) -> bool {
    if abbrs.is_empty() || buf.is_readonly() {
        return false;
    }
    let (row, col) = buf.cursor();
    let line = buf.line_chars(row);
    let word_start = crate::bishedit::completion::find_word_start(&line, col.min(line.len()));
    let word: String = line[word_start..col.min(line.len())].iter().collect();
    if word.is_empty() {
        return false;
    }
    let Some(abbr) = abbrs.iter().find(|a| a.name == word) else {
        return false;
    };
    match Snippet::parse(&abbr.expansion) {
        Some(snip) => *live = Some(LiveSnippet::start(snip, row, word_start, word, buf)),
        None => {
            // `flatten`: even with no tabstops an expansion can carry `\$`
            // or `${1}`, and what goes in is what a finished snippet would
            // leave.
            let text = snippet::flatten(&abbr.expansion);
            buf.replace_span((row, word_start), (row, col), &text);
            buf.set_cursor(row, word_start + text.chars().count());
        }
    }
    true
}

// Inserts `text` (at most one `\n`, alone, from Enter) at every cursor
// in `cursors`, for multi-selection `c`. In ascending order: an
// insertion only pushes later cursors further, so each later one is
// adjusted before its turn.
fn apply_insert_to_all(buf: &mut TextBuffer, cursors: &mut [(usize, usize)], text: &str) {
    let is_newline = text == "\n";
    let width = text.chars().count();
    let mut order: Vec<usize> = (0..cursors.len()).collect();
    crate::sort::sort_by_key(&mut order, |&i| cursors[i]);
    for step in 0..order.len() {
        let i = order[step];
        let (row, col) = cursors[i];
        cursors[i] = buf.insert_text((row, col), text);
        for &j in &order[step + 1..] {
            let (r, c) = cursors[j];
            if is_newline {
                // A cursor on the same row at or after the split moves to
                // the new row, relative to its start; later rows shift down
                // one.
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

// Delete at every cursor, furthest first, like
// `apply_backspace_to_all`. The cursor stays; at a line's end the next
// line joins. At the end of the buffer, nothing happens.
fn apply_delete_to_all(buf: &mut TextBuffer, cursors: &mut [(usize, usize)]) {
    let mut order: Vec<usize> = (0..cursors.len()).collect();
    crate::sort::sort_by_key(&mut order, |&i| std::cmp::Reverse(cursors[i]));
    for step in 0..order.len() {
        let i = order[step];
        let (row, col) = cursors[i];
        if col < buf.line_len(row) {
            let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, col), to: (row, col + 1) };
            buf.delete_range(&range);
            for &j in &order[step + 1..] {
                let (r, c) = cursors[j];
                if r == row && c > col {
                    cursors[j] = (r, c - 1);
                }
            }
        } else if row + 1 < buf.line_count() {
            let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, col), to: (row + 1, 0) };
            buf.delete_range(&range);
            for &j in &order[step + 1..] {
                let (r, c) = cursors[j];
                if r > row + 1 {
                    cursors[j] = (r - 1, c);
                } else if r == row + 1 {
                    cursors[j] = (row, col + c);
                }
            }
        }
    }
}

// Backspace at every cursor, furthest first: removing text later never
// moves an earlier cursor. A cursor at the buffer's start is left
// alone.
fn apply_backspace_to_all(buf: &mut TextBuffer, cursors: &mut [(usize, usize)]) {
    let mut order: Vec<usize> = (0..cursors.len()).collect();
    crate::sort::sort_by_key(&mut order, |&i| std::cmp::Reverse(cursors[i]));
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
                    // Two cursors at column 0 of the same row: this row
                    // joins the one above, so this one moves there too.
                    cursors[j] = (r - 1, prev_len + c);
                }
            }
        }
    }
}

// `"%"`, the current filename register, refreshed when the buffer's path
// may have changed: when `run_edit_frame` starts, and after `:w`, `:wq`
// or `:x`, since `:w newname` can name a buffer. A no-op without a path.
pub(crate) fn set_last_filename(buf: &TextBuffer, registers: &mut Registers) {
    if let Some(path) = buf.path() {
        registers.set_last_filename(path.to_string_lossy().into_owned());
    }
}

/// `%` in a `:!` command line: the file being edited, as it was opened,
/// with vim's `:h`/`:t`/`:r`/`:e`/`:p` modifiers. `\%` is a literal `%`.
/// Only `:!`, since the colon line is a shell line where `%` is syntax
/// (`${v%.c}`, `printf '%s'`, `jobs %1`). `#` is left alone: there's no
/// alternate file, and it starts a shell comment.
pub(crate) fn expand_filename_macros(line: &str, buf: &TextBuffer) -> Result<String, String> {
    // vim's message and rule: an empty expansion stops the command.
    const EMPTY: &str = "E499: Empty file name for '%'";
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            // The backslash goes and the `%` stays; other `\x` belong to
            // the shell.
            '\\' if chars.get(i + 1) == Some(&'%') => {
                out.push('%');
                i += 2;
            }
            '%' => {
                let Some(path) = buf.path() else { return Err(EMPTY.to_string()) };
                i += 1;
                let expanded = apply_filename_modifiers(&path.to_string_lossy(), &chars, &mut i);
                if expanded.is_empty() {
                    return Err(EMPTY.to_string());
                }
                out.push_str(&expanded);
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// The modifiers after a `%`, applied left to right: `%:p:h` is the full
/// path's directory, `%:t:r` the basename without extension. An unknown
/// `:x` isn't a modifier and stays (`:!make %:2`).
fn apply_filename_modifiers(name: &str, chars: &[char], i: &mut usize) -> String {
    let mut value = name.to_string();
    while chars.get(*i) == Some(&':') {
        let Some(&modifier) = chars.get(*i + 1) else { break };
        value = match modifier {
            // `.` when there's nothing above, as in vim, not the empty
            // string, which would abort the command.
            'h' => match value.rsplit_once('/') {
                Some((head, _)) if !head.is_empty() => head.to_string(),
                Some(_) => "/".to_string(),
                None => ".".to_string(),
            },
            't' => value.rsplit('/').next().unwrap_or(&value).to_string(),
            // One extension; a leading dot isn't one (`.hidden`), as in vim.
            'r' => match split_extension(&value) {
                Some((root, _)) => root.to_string(),
                None => value,
            },
            'e' => match split_extension(&value) {
                Some((_, ext)) => ext.to_string(),
                None => String::new(),
            },
            'p' => std::fs::canonicalize(&value).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| absolute_path(&value)),
            _ => break,
        };
        *i += 2;
    }
    value
}

/// Splits `a/b/c.txt` into `a/b/c` and `txt`. `None` when the last
/// component has no extension: no dot, or only a dotfile's.
fn split_extension(path: &str) -> Option<(&str, &str)> {
    let start = path.rfind('/').map_or(0, |at| at + 1);
    let dot = path[start..].rfind('.')? + start;
    (dot > start).then(|| (&path[..dot], &path[dot + 1..]))
}

/// `:p` for a file that doesn't exist yet: the cwd in front, without
/// resolving links.
fn absolute_path(path: &str) -> String {
    if path.starts_with('/') {
        return path.to_string();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path).to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
    }
}

// The whole pane: the mode line is in the global status row
// (`repl::render_global_status_row`). At least 1, so a zero-height rect
// still has a row.
fn editor_content_rows(rect: Rect) -> usize {
    rect.rows.max(1)
}

// vim's mode line (`-- INSERT --` and so on), with `[+]` while there
// are unsaved changes.
fn mode_label(vk: &VimKeys, mode: EditorMode) -> &'static str {
    match mode {
        EditorMode::Insert => return "-- INSERT --",
        EditorMode::Replace => return "-- REPLACE --",
        EditorMode::Normal => {}
    }
    match vk.visual_anchor() {
        Some((RegisterShape::Char, _)) => "-- VISUAL --",
        Some((RegisterShape::Line, _)) => "-- VISUAL LINE --",
        Some((RegisterShape::Block, _)) => "-- VISUAL BLOCK --",
        None => "-- NORMAL --",
    }
}

fn status_text(buf: &TextBuffer, vk: &VimKeys, mode: EditorMode, cols: usize) -> String {
    let label = mode_label(vk, mode);
    let pending = vk.pending_display();
    // "recording @a" during `q{reg}`, as repl.rs's
    // `normal_mode_status_left` shows.
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
    let mut right = format!("{},{}  {}/{}", line + 1, col + 1, line + 1, total);

    // A message the server asked to show, else its progress, between the
    // mode and the cursor position, out of the way of a `:` line. A message
    // wins: it's news, and gone by the next key. Dropped, not truncated,
    // when the pane is too narrow for it and the position.
    if let Some(progress) = buf.echoed_message.as_ref().or(buf.lsp_progress.as_ref()) {
        let candidate = format!("{progress}   {right}");
        if left.chars().count() + candidate.chars().count() < cols {
            right = candidate;
        }
    }

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

// The (start, end) char range `range` covers on `line`, if any,
// relative to `start_char` (the char index of `render_row`'s first
// visible character) and clamped to `cols`, so it indexes that row's
// array directly. `spans_for_line` and `diagnostic_spans_for_line` stay
// line-absolute instead: a Linewise selection's `(0, cols)` already
// means the visible row.
fn selection_columns_in_line(range: &motion::MotionRange, line: usize, start_char: usize, cols: usize) -> Option<(usize, usize)> {
    if line < range.from.0 || line > range.to.0 {
        return None;
    }
    if range.shape == motion::MotionShape::Linewise {
        return Some((0, cols));
    }
    // A block covers the same columns on every row it spans.
    if let Some((left, right)) = motion::block_columns(range) {
        let start = left.saturating_sub(start_char);
        let end = (right + 1).saturating_sub(start_char).min(cols);
        return (end > start).then_some((start, end));
    }
    let start = if line == range.from.0 { range.from.1.saturating_sub(start_char) } else { 0 };
    let end = if line == range.to.0 { (range.to.1 + 1).saturating_sub(start_char).min(cols) } else { cols };
    Some((start, end))
}

// Which gutter column this is, so a click or hover on it can mean
// something. The renderers don't need it; code working inwards from a
// screen position does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GutterKind {
    Breakpoint,
    Blame,
    Diff,
    Diagnostic,
    LineNumber,
}

// One gutter column. `build_editor_frame` only asks for the total width
// and each line's cells, so a column is added by appending to
// `GUTTER_COLUMNS`. `render` returns styled text, `None` for blank;
// `starts` (`line_starts`, once per frame) maps diagnostics' offsets.
struct GutterColumn {
    kind: GutterKind,
    width: fn(&TextBuffer) -> usize,
    render: fn(buf: &TextBuffer, starts: &[usize], line: usize, width: usize) -> Option<String>,
}

static GUTTER_COLUMNS: &[GutterColumn] = &[
    GutterColumn { kind: GutterKind::Breakpoint, width: breakpoint_column_width, render: render_breakpoint_cell },
    GutterColumn { kind: GutterKind::Blame, width: blame_column_width, render: render_blame_cell },
    GutterColumn { kind: GutterKind::Diff, width: diff_column_width, render: render_diff_cell },
    GutterColumn { kind: GutterKind::Diagnostic, width: diagnostic_column_width, render: render_diagnostic_cell },
    GutterColumn { kind: GutterKind::LineNumber, width: line_number_width, render: render_line_number_cell },
];

/// Which line and gutter column a screen cell is in, or `None` in the
/// text or off the buffer. The inverse of `render_gutter`, walking the
/// same list in the same order so the two agree. Only a wrapped line's
/// first row has a gutter.
pub(crate) fn gutter_at_screen(buf: &TextBuffer, rect: Rect, row0: usize, col0: usize) -> Option<(usize, GutterKind)> {
    if row0 < rect.row || col0 < rect.col || col0 >= rect.col + rect.cols {
        return None;
    }
    let row = row0 - rect.row;
    if row >= editor_content_rows(rect) {
        return None;
    }
    let gutter = total_gutter_width(buf).min(rect.cols.saturating_sub(1));
    let x = col0 - rect.col;
    if x >= gutter {
        return None;
    }
    let content_cols = rect.cols - gutter;
    let rows = visible_rows(buf, content_cols, editor_content_rows(rect));
    let visual = rows.get(row)?;
    if visual.seg.start != 0 {
        return None;
    }
    let mut at = 0;
    for col in GUTTER_COLUMNS {
        let width = (col.width)(buf);
        if width > 0 && x < at + width {
            return Some((visual.line, col.kind));
        }
        at += width;
    }
    None
}

fn total_gutter_width(buf: &TextBuffer) -> usize {
    GUTTER_COLUMNS.iter().map(|col| (col.width)(buf)).sum()
}

// `first` is whether this is the line's first screen row: only that row
// gets the number and markers.
fn render_gutter(out: &mut String, buf: &TextBuffer, starts: &[usize], line: usize, first: bool) {
    for col in GUTTER_COLUMNS {
        let width = (col.width)(buf);
        let cell = if first { (col.render)(buf, starts, line, width) } else { None };
        match cell {
            Some(cell) => out.push_str(&cell),
            None => out.push_str(&" ".repeat(width)),
        }
    }
}

// This buffer's column layout, or `None` with no tabular form, which is
// most buffers and every buffer while wrapping. Measured over the whole
// file, so columns don't change width as you scroll: a pass per redraw,
// plus a markdown parse, the same `buffer_highlight_spans` does.
pub(crate) fn tabular_layout(buf: &TextBuffer) -> Option<crate::bishedit::tabular::Layout> {
    let style = buf.tabular.filter(|_| !buf.wrap.wrap)?;
    let lines: Vec<Vec<char>> = (0..buf.line_count()).map(|l| buf.line_chars(l)).collect();
    let lines: Vec<&[char]> = lines.iter().map(|l| l.as_slice()).collect();
    match table_regions(buf) {
        Some(regions) => Some(crate::bishedit::tabular::measure_regions(&lines, style, &regions)),
        None => Some(crate::bishedit::tabular::measure(&lines, style)),
    }
}

// Which lines of a markdown buffer are table rows. `None` for other
// languages, meaning the whole file is one table, as in a `.csv`. The
// parser, not a search for pipes, so a pipe in a code block or prose
// isn't a column, and separate tables aren't aligned together.
fn table_regions(buf: &TextBuffer) -> Option<Vec<std::ops::Range<usize>>> {
    if language_of(buf) != "markdown" {
        return None;
    }
    let text = buf.text();
    let doc = crate::markdown::parse(&text);
    let mut spans = Vec::new();
    collect_table_spans(&doc.blocks, &mut spans);

    let mut starts = vec![0usize];
    starts.extend(text.char_indices().filter(|(_, c)| *c == '\n').map(|(i, _)| i + 1));
    let line_of = |offset: usize| starts.partition_point(|s| *s <= offset).saturating_sub(1);
    Some(spans.into_iter().map(|s| line_of(s.start)..line_of(s.end.saturating_sub(1)) + 1).collect())
}

// Tables can sit in a block quote or a list item, so the whole tree is
// walked.
fn collect_table_spans(blocks: &[crate::markdown::Block], out: &mut Vec<std::ops::Range<usize>>) {
    use crate::markdown::Block;
    for block in blocks {
        match block {
            Block::Table(t) => out.push(t.span.clone()),
            Block::BlockQuote { blocks, .. } => collect_table_spans(blocks, out),
            Block::List(list) => {
                for item in &list.items {
                    collect_table_spans(&item.blocks, out);
                }
            }
            _ => {}
        }
    }
}

// One line as drawn: its characters plus any alignment padding. Without
// a layout, the line itself.
pub(crate) fn display_row(buf: &TextBuffer, line: usize, layout: Option<&crate::bishedit::tabular::Layout>) -> crate::bishedit::tabular::Row {
    let chars = buf.line_chars(line);
    let row = match layout {
        Some(layout) => crate::bishedit::tabular::row(&chars, line, layout),
        None => crate::bishedit::tabular::Row::plain(&chars),
    };
    splice_inlay_hints(expand_tabs(row, buf.tabstop), buf, line)
}

// Splices inlay hints into the row's cells, after `expand_tabs`, since a
// hint's width has nothing to do with tab stops. Everything downstream
// addresses a line through `Row`'s maps, so all of it becomes
// hint-aware here. A hint's cells map to no source character, so the
// cursor steps over one in a single press and `render_row` dims it, as
// with tabular padding.
fn splice_inlay_hints(row: crate::bishedit::tabular::Row, buf: &TextBuffer, line: usize) -> crate::bishedit::tabular::Row {
    if !buf.inlayhints {
        return row;
    }
    let mut hints = buf.inlay_hints.iter().filter(|(l, _, _)| *l == line).peekable();
    if hints.peek().is_none() {
        return row;
    }
    let mut cells: Vec<char> = Vec::with_capacity(row.cells.len());
    let mut source_at: Vec<Option<usize>> = Vec::with_capacity(row.cells.len());
    // In step with the cells: a hint at character `c` goes before the cell
    // `c` starts at; one past the end goes at the end.
    let mut pending: Vec<(usize, &str)> = hints.map(|(_, col, text)| (*col, text.as_str())).collect();
    crate::sort::sort_by_key(&mut pending, |(col, _)| *col);
    let mut next = 0usize;
    let emit = |cells: &mut Vec<char>, source_at: &mut Vec<Option<usize>>, upto: usize, next: &mut usize| {
        while *next < pending.len() && pending[*next].0 <= upto {
            for ch in pending[*next].1.chars() {
                cells.push(ch);
                source_at.push(None);
            }
            *next += 1;
        }
    };
    for (i, ch) in row.cells.iter().enumerate() {
        // Only where a character starts, never inside a tab's spaces or a
        // wide character.
        if let Some(source) = row.source_at.get(i).copied().flatten()
            && row.cell_of.get(source) == Some(&i)
        {
            emit(&mut cells, &mut source_at, source, &mut next);
        }
        cells.push(*ch);
        source_at.push(row.source_at.get(i).copied().flatten());
    }
    // Anything at or past the end goes on the end.
    emit(&mut cells, &mut source_at, usize::MAX, &mut next);

    // Rebuilt as `expand_tabs` does: a source character maps to its first
    // cell, and characters with no cell point one past the end, keeping the
    // maps monotonic.
    let mut cell_of = vec![cells.len(); row.cell_of.len()];
    for (cell, source) in source_at.iter().enumerate().rev() {
        if let Some(source) = source
            && *source < cell_of.len()
        {
            cell_of[*source] = cell;
        }
    }
    let mut last = cells.len();
    for slot in cell_of.iter_mut().rev() {
        if *slot > last {
            *slot = last;
        }
        last = *slot;
    }
    crate::bishedit::tabular::Row { cells, source_at, cell_of }
}

// A literal tab as spaces to the next `tabstop`, on the drawn cells,
// after the tabular layout and before anything reads them. Since the
// cursor, clicks, scrolling, selections, highlighting and diagnostics
// all go through `Row`'s maps, this makes all of them tab-aware. Each
// space maps back to the tab.
fn expand_tabs(row: crate::bishedit::tabular::Row, tabstop: usize) -> crate::bishedit::tabular::Row {
    if !row.cells.contains(&'\t') {
        return row;
    }
    let tabstop = tabstop.max(1);
    let mut cells = Vec::with_capacity(row.cells.len());
    let mut source_at = Vec::with_capacity(row.cells.len());
    let mut column = 0usize;
    for (i, ch) in row.cells.iter().enumerate() {
        let source = row.source_at.get(i).copied().flatten();
        if *ch == '\t' {
            for _ in 0..tabstop - (column % tabstop) {
                cells.push(' ');
                source_at.push(source);
                column += 1;
            }
            continue;
        }
        cells.push(*ch);
        source_at.push(source);
        column += char_width(*ch);
    }
    // Rebuilt: a tab's spaces all name one character, which maps to the
    // first.
    let mut cell_of = vec![cells.len(); row.cell_of.len()];
    for (cell, source) in source_at.iter().enumerate().rev() {
        if let Some(source) = source
            && *source < cell_of.len()
        {
            cell_of[*source] = cell;
        }
    }
    // Anything past the end points one past it, keeping the maps
    // monotonic.
    let mut last = cells.len();
    for slot in cell_of.iter_mut().rev() {
        if *slot > last {
            *slot = last;
        }
        last = *slot;
    }
    crate::bishedit::tabular::Row { cells, source_at, cell_of }
}

// The drawn column of a character index: what Tab advances from and
// `scroll_to_show_cursor` measures against.
fn display_column(buf: &TextBuffer, line: usize, col: usize) -> usize {
    let display = display_row(buf, line, tabular_layout(buf).as_ref());
    col_at_cell(&display.cells, display.cell_of[col.min(buf.line_len(line))])
}

// Which character a drawn cell belongs to. Padding belongs to the
// character before it, so a cursor placed on it lands somewhere real.
fn source_at_or_before(display: &crate::bishedit::tabular::Row, cell: usize) -> usize {
    let last = display.cells.len();
    for i in (0..=cell.min(last)).rev() {
        if let Some(Some(source)) = display.source_at.get(i) {
            return *source;
        }
    }
    0
}

// `char_at_col`/`col_of` over drawn cells, which differ from characters
// once padding is inserted, as for a wide glyph.
fn cell_at_col(cells: &[char], col: usize) -> usize {
    let mut used = 0;
    for (i, c) in cells.iter().enumerate() {
        if used >= col {
            return i;
        }
        used += char_width(*c);
    }
    cells.len()
}

fn col_at_cell(cells: &[char], cell: usize) -> usize {
    cells[..cell.min(cells.len())].iter().map(|c| char_width(*c)).sum()
}

// A span of the line, in characters, mapped into the window of cells
// being drawn, so padding shifts a highlight as far as the text under
// it. `None` when it falls outside the window.
fn to_window(display: &crate::bishedit::tabular::Row, start_cell: usize, avail: usize, start: usize, end: usize) -> Option<(usize, usize)> {
    let last = display.cell_of.len().saturating_sub(1);
    let from = display.cell_of[start.min(last)];
    let to = display.cell_of[end.min(last)];
    if to <= start_cell {
        return None;
    }
    let from = from.saturating_sub(start_cell);
    let to = (to - start_cell).min(avail);
    (from < to).then_some((from, to))
}

// One screen row: which buffer line, which slice of it, and whether it
// opens that line. `fold` is the last line of the closed fold this row
// stands for, when it stands for one rather than showing `line` itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisualRow {
    pub(crate) line: usize,
    pub(crate) first: bool,
    pub(crate) seg: crate::bishedit::wrap::Segment,
    pub(crate) fold: Option<usize>,
}

// How a line breaks into screen rows. With wrapping off, one segment
// for the whole line, to which callers apply `viewport_left`.
pub(crate) fn line_segments(buf: &TextBuffer, line: usize, content_cols: usize) -> Vec<crate::bishedit::wrap::Segment> {
    crate::bishedit::wrap::segments(&buf.line_chars(line), content_cols, &buf.wrap)
}

// The rows a pane shows from the buffer's scroll position, shared by
// rendering, clicks and scrolling so they agree.
pub(crate) fn visible_rows(buf: &TextBuffer, content_cols: usize, content_rows: usize) -> Vec<VisualRow> {
    let mut rows = Vec::with_capacity(content_rows);
    // A top line hidden by a closed fold is drawn from the fold's row.
    let mut line = buf.fold_span(buf.viewport_top()).0;
    let mut skip = if line == buf.viewport_top() { buf.viewport_sub() } else { 0 };
    while rows.len() < content_rows && line < buf.line_count() {
        // One row however long the fold: it shows what the fold is.
        if let Some((_, end)) = buf.folds.closed_at(line) {
            let seg = crate::bishedit::wrap::Segment { start: 0, end: buf.line_len(line), indent: 0 };
            rows.push(VisualRow { line, first: true, seg, fold: Some(end) });
            skip = 0;
            line = end + 1;
            continue;
        }
        for (i, seg) in line_segments(buf, line, content_cols).into_iter().enumerate() {
            if i < skip {
                continue;
            }
            if rows.len() >= content_rows {
                break;
            }
            rows.push(VisualRow { line, first: i == 0, seg, fold: None });
        }
        skip = 0;
        line += 1;
    }
    rows
}

// What a continued row starts with: `showbreak`, then padding to the
// reserved indent. Dim: it's the editor, not the file.
fn continuation_prefix(buf: &TextBuffer, indent: usize) -> String {
    if indent == 0 {
        return String::new();
    }
    let mark: String = buf.wrap.showbreak.chars().collect();
    let mark_width: usize = mark.chars().map(char_width).sum();
    let mark = if mark_width > indent { String::new() } else { mark };
    let used: usize = mark.chars().map(char_width).sum();
    format!("\x1b[2m{mark}\x1b[0m{}", " ".repeat(indent - used))
}

// `:git blame`'s column: the commit, the date and the author (as wide as
// the widest, capped), each followed by a space. Zero width while blame
// is off: unlike the sign column, it's wide, and toggled deliberately.
const BLAME_COMMIT_WIDTH: usize = 8;
const BLAME_DATE_WIDTH: usize = 10;
const BLAME_AUTHOR_MAX_WIDTH: usize = 16;

fn blame_column_width(buf: &TextBuffer) -> usize {
    let Some(blame) = &buf.blame else { return 0 };
    let author_width = blame.iter().flatten().map(|b| b.author.chars().count()).max().unwrap_or(0).clamp(1, BLAME_AUTHOR_MAX_WIDTH);
    BLAME_COMMIT_WIDTH + 1 + BLAME_DATE_WIDTH + 1 + author_width + 1
}

// What a line git said nothing about shows instead of a commit (see
// `TextBuffer::blame`), in its own colour, so it doesn't look like a
// rendering gap.
const BLAME_UNKNOWN: &str = "· N/A";

fn render_blame_cell(buf: &TextBuffer, _starts: &[usize], line: usize, width: usize) -> Option<String> {
    let blame = buf.blame.as_ref()?;
    let author_width = width.saturating_sub(BLAME_COMMIT_WIDTH + 1 + BLAME_DATE_WIDTH + 1 + 1);
    match blame.get(line)? {
        Some(entry) => {
            let author: String = entry.author.chars().take(author_width).collect();
            Some(format!("\x1b[2m{} {} {:<aw$} \x1b[0m", entry.short_commit, entry.date, author, aw = author_width))
        }
        None => Some(format!(
            "\x1b[33m{:<cw$}\x1b[0m {:<dw$} {:<aw$} ",
            BLAME_UNKNOWN,
            "",
            "",
            cw = BLAME_COMMIT_WIDTH,
            dw = BLAME_DATE_WIDTH,
            aw = author_width
        )),
    }
}

// `:git diff`'s column: a marker and a space. Zero width while diff is
// off, as for blame.
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

// The breakpoint column for `bish tool debug`, zero width with no
// breakpoints and no debug session, as blame and diff are.
fn breakpoint_column_width(buf: &TextBuffer) -> usize {
    // For the whole debug session, not just while a breakpoint exists (see
    // `TextBuffer::debug_attached`).
    if buf.breakpoints.is_empty() && !buf.debug_attached { 0 } else { 2 }
}

// 1-based, as the debugger numbers lines; `line` is 0-based like every
// gutter column's.
fn render_breakpoint_cell(buf: &TextBuffer, _starts: &[usize], line: usize, _width: usize) -> Option<String> {
    if line >= buf.line_count() || !buf.breakpoints.contains(&(line + 1)) {
        return None;
    }
    Some("\x1b[1;31m\u{25cf}\x1b[0m ".to_string())
}

// A fixed 2 columns, marker and space, as vim's `signcolumn`.
fn diagnostic_column_width(_buf: &TextBuffer) -> usize {
    2
}

// The worst severity among diagnostics whose range touches this line,
// the predicate `diagnostic_spans_for_line` uses per column, reduced to
// one mark. `max`, since `lint::Severity` is ordered least severe
// first.
fn line_severity(buf: &TextBuffer, starts: &[usize], line: usize) -> Option<lint::Severity> {
    let line_start = starts[line];
    let line_end = line_start + buf.line_len(line);
    buf.diagnostics.iter().filter(|d| d.start < line_end && d.end > line_start).map(|d| d.severity).max()
}

/// Every diagnostic touching `line`, worst first: what the gutter marker
/// stands for. Public because repl.rs says what they are; matching them
/// to a line is this file's arithmetic.
pub(crate) fn diagnostics_on_line(buf: &TextBuffer, line: usize) -> Vec<&lint::Diagnostic> {
    if line >= buf.line_count() {
        return Vec::new();
    }
    let starts = line_starts(buf);
    let line_start = starts[line];
    let line_end = line_start + buf.line_len(line);
    let mut found: Vec<&lint::Diagnostic> = buf.diagnostics.iter().filter(|d| d.start < line_end && d.end > line_start).collect();
    crate::sort::sort_by_key(&mut found, |d| std::cmp::Reverse(d.severity));
    found
}

fn render_diagnostic_cell(buf: &TextBuffer, starts: &[usize], line: usize, _width: usize) -> Option<String> {
    if line >= buf.line_count() {
        return None;
    }
    let severity = line_severity(buf, starts, line)?;
    // The bullet takes the severity's colour, not its underline.
    let (fg, _) = crate::theme::resolve(severity_element(severity), buf.colors.as_ref());
    let sgr = vt100::sgr_codes(fg, vt100::Color::Default, vt100::CellAttrs::default());
    Some(format!("{sgr}\u{25cf}\x1b[0m "))
}

// As many digits as the last line number needs, plus a space, as in
// vim.
fn line_number_width(buf: &TextBuffer) -> usize {
    // Sized for the widest number the column can draw: with
    // `relativenumber` the cursor's line still shows its absolute number,
    // and a gutter that resized as the cursor moved would shift the pane on
    // every `j`.
    buf.line_count().to_string().len() + 1
}

fn render_line_number_cell(buf: &TextBuffer, _starts: &[usize], line: usize, width: usize) -> Option<String> {
    if line >= buf.line_count() {
        return None;
    }
    let sgr = crate::theme::sgr(crate::theme::Ui::LineNumber, buf.colors.as_ref());
    Some(format!("{sgr}{:>pad$} \x1b[0m", line_number_text(buf, line), pad = width.saturating_sub(1)))
}

// The line's number, or with `relativenumber` its distance from the
// cursor, so `12j` can be read off the screen. The cursor's line keeps
// its absolute number: vim shows 0 only because `number` can be off,
// which bish's can't.
fn line_number_text(buf: &TextBuffer, line: usize) -> String {
    let (cursor, _) = buf.cursor();
    if !buf.relativenumber || line == cursor {
        return (line + 1).to_string();
    }
    // A closed fold is one row, so it counts as one: the number is what
    // `3j` does.
    if buf.folds.any_closed() {
        let (low, high) = (line.min(cursor), line.max(cursor));
        let target = buf.fold_span(high).0;
        let mut at = buf.fold_span(low).0;
        let mut rows = 0;
        while at < target {
            at = buf.fold_span(at).1 + 1;
            rows += 1;
        }
        return rows.to_string();
    }
    line.abs_diff(cursor).to_string()
}

// Language from the extension; no shebang or content sniffing. A name,
// not an enum, because `abbr --lang=` globs over it by hand
// (`--lang='*script'`), so `--lang=toml` works without this file knowing
// TOML. This table lists only extensions that differ from the language;
// others are their own lowercased extension. A buffer with no file is
// "text".
const LANGUAGE_BY_EXTENSION: &[(&str, &str)] = &[
    ("sh", "bash"),
    ("bash", "bash"),
    ("rs", "rust"),
    ("py", "python"),
    ("pyi", "python"),
    ("rb", "ruby"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("jsx", "javascript"),
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("mts", "typescript"),
    ("cts", "typescript"),
    ("htm", "html"),
    ("md", "markdown"),
    ("yml", "yaml"),
    ("kt", "kotlin"),
    ("hs", "haskell"),
    ("ex", "elixir"),
    ("exs", "elixir"),
    // Man page sections: `ls.1`, `printf.3` are roff, and `language_of`
    // looks through `.gz`, so pages from /usr/share/man land here.
    ("1", "roff"),
    ("2", "roff"),
    ("3", "roff"),
    ("4", "roff"),
    ("5", "roff"),
    ("6", "roff"),
    ("7", "roff"),
    ("8", "roff"),
    ("9", "roff"),
    ("man", "roff"),
    ("tmac", "roff"),
    // The INI family. `.conf` is the loosest (nginx's isn't INI), but it's
    // where people most often put INI, and a wrong guess only colours words
    // as keys.
    ("ini", "ini"),
    ("cfg", "ini"),
    ("conf", "ini"),
    ("desktop", "ini"),
    // systemd units, INI in a stricter dialect. Some (`.path`, `.link`,
    // `.target`) are generic words, but only highlighting depends on this.
    ("service", "ini"),
    ("socket", "ini"),
    ("timer", "ini"),
    ("target", "ini"),
    ("mount", "ini"),
    ("automount", "ini"),
    ("path", "ini"),
    ("slice", "ini"),
    ("network", "ini"),
    ("netdev", "ini"),
    ("link", "ini"),
    ("nmconnection", "ini"),
    // JSON with comments. `.json` stays strict, so a stray `//` isn't
    // coloured as a valid comment.
    ("jsonc", "jsonc"),
    // `local.env`, `production.env`.
    ("env", "dotenv"),
];

// Languages by file name, checked first, case-insensitively, for files
// with no extension: `.gitconfig`, `.editorconfig` and `.npmrc` are all
// stem to `Path`.
const LANGUAGE_BY_FILE_NAME: &[(&str, &str)] = &[
    (".gitconfig", "ini"),
    (".gitmodules", "ini"),
    (".editorconfig", "ini"),
    (".npmrc", "ini"),
    (".hgrc", "ini"),
    (".pypirc", "ini"),
    (".pylintrc", "ini"),
    ("pylintrc", "ini"),
    (".flake8", "ini"),
    (".coveragerc", "ini"),
    // TOML without saying so.
    ("pipfile", "toml"),
    ("poetry.lock", "toml"),
    // JSON with comments despite the extension: TypeScript accepts
    // comments here, and real `tsconfig.json` files have them.
    ("tsconfig.json", "jsonc"),
    ("jsconfig.json", "jsonc"),
    ("devcontainer.json", "jsonc"),
    (".eslintrc.json", "jsonc"),
    // direnv's, which is bash; here so the `.env` prefix rule below can't
    // claim it.
    (".envrc", "bash"),
    // Read by name by python-dotenv; all stem to `Path`, as `.gitconfig`.
    (".flaskenv", "dotenv"),
];

pub(crate) fn language_of(buf: &TextBuffer) -> String {
    match buf.path() {
        Some(path) => language_of_path(path),
        None => "text".to_string(),
    }
}

// What a file at `path` is written in, from its name, for text not in a
// buffer, such as either side of a diff.
pub(crate) fn language_of_path(path: &std::path::Path) -> String {
    // `.gz` is how it's stored, not what it is: `notes.json.gz` is JSON, so
    // the suffix is stepped over.
    let path = match path.extension() {
        Some(ext) if ext.eq_ignore_ascii_case("gz") => std::path::Path::new(path.file_stem().unwrap_or(path.as_os_str())),
        _ => path,
    };
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if let Some((_, lang)) = LANGUAGE_BY_FILE_NAME.iter().find(|(n, _)| *n == name) {
        return (*lang).to_string();
    }
    let parent = path.parent().and_then(|p| p.file_name()).map(|d| d.to_string_lossy().to_lowercase());
    match parent.as_deref() {
        // `config` alone is too common to claim, but in `.git` (or
        // `~/.config/git`) it's the git config.
        Some(".git") | Some("git") if name == "config" => return "ini".to_string(),
        // In `.cargo` it's cargo's, which is TOML.
        Some(".cargo") if name == "config" => return "toml".to_string(),
        // VS Code's `.vscode/*.json` files all accept comments, and have
        // them.
        Some(".vscode") if name.ends_with(".json") => return "jsonc".to_string(),
        _ => {}
    }
    // The `tsconfig.*.json` family (`tsconfig.app.json`, ...) is one format.
    if (name.starts_with("tsconfig.") || name.starts_with("jsconfig.")) && name.ends_with(".json") {
        return "jsonc".to_string();
    }
    // Likewise `.env`, `.env.local`, `.env.production` and the rest.
    // `.envrc` is claimed above as bash.
    if name == ".env" || name.starts_with(".env.") {
        return "dotenv".to_string();
    }
    let Some(ext) = path.extension() else {
        return "text".to_string();
    };
    let ext = ext.to_string_lossy().to_lowercase();
    LANGUAGE_BY_EXTENSION.iter().find(|(e, _)| *e == ext).map(|(_, lang)| (*lang).to_string()).unwrap_or(ext)
}

pub(crate) fn buffer_source(buf: &TextBuffer) -> String {
    buffer_text(buf)
}

// The buffer's text, lines joined by newlines, so a highlighter sees a
// multi-line construct (a heredoc, a JSON object) whole.
pub(crate) fn buffer_text(buf: &TextBuffer) -> String {
    (0..buf.line_count()).map(|l| (0..buf.line_len(l)).map(|c| buf.char_at(l, c).unwrap_or(' ')).collect::<String>()).collect::<Vec<_>>().join("\n")
}

// The offset in `buffer_text`'s string where each line starts: a prefix
// sum of line lengths plus one per newline, computed once per frame
// rather than per row.
pub(crate) fn line_starts_of(buf: &TextBuffer) -> Vec<usize> {
    line_starts(buf)
}

fn line_starts(buf: &TextBuffer) -> Vec<usize> {
    let mut starts = Vec::with_capacity(buf.line_count());
    let mut pos = 0;
    for l in 0..buf.line_count() {
        starts.push(pos);
        pos += buf.line_len(l) + 1;
    }
    starts
}

// This language's highlighter, if any. The table is in
// bishedit::highlight, since a markdown code fence asks the same
// question and must get the same answer. `None` renders plain text.
fn highlighter_for(language: &str) -> Option<Box<dyn Highlighter>> {
    highlight::highlighter_for_language(language)
}

// The style layer alone, for tests; rendering takes both layers from
// one pass (`buffer_spans`).
#[cfg(test)]
fn buffer_highlight_spans(buf: &TextBuffer, color_overrides: Option<&highlight::ColorOverrides>) -> Vec<StyledSpan> {
    buffer_spans(buf, color_overrides).0
}

// Both span layers from one run of the highlighter over the whole text,
// so multi-line constructs are seen whole. Recomputed every redraw: the
// files are scripts and configs, which lex in well under a
// millisecond. `HighlightContext::default()` stands in for a shell, so
// bash's context-dependent refinements don't fire. `color_overrides`
// (`::bish hl`) picks colours afterwards, from repl.rs's
// `run_edit_frame`.
//
// The link layer carries `HighlightSpan.link` (bash's resolved paths,
// markdown's link targets), plus bare URLs found in any text.
fn buffer_spans(buf: &TextBuffer, color_overrides: Option<&highlight::ColorOverrides>) -> (Vec<StyledSpan>, Vec<highlight::LinkSpan>) {
    let text = buffer_text(buf);
    let mut spans = match highlighter_for(&language_of(buf)) {
        Some(highlighter) => highlighter.highlight(&text, HighlightContext::default()),
        None => Vec::new(),
    };
    // After the language's spans, so it paints over them: the underline is
    // the cue a URL in a string can be clicked.
    for found in crate::url::find(&text) {
        let url: String = text.chars().skip(found.start).take(found.end - found.start).collect();
        spans.push(highlight::HighlightSpan { start: found.start, end: found.end, kind: highlight::HighlightKind::Link, link: Some(url) });
    }
    let links =
        spans.iter().filter_map(|s| Some(highlight::LinkSpan { start: s.start, end: s.end, url: absolute_link(buf, s.link.as_deref()?)? })).collect();
    let mut styled: Vec<StyledSpan> = spans
        .into_iter()
        .map(|s| {
            let (fg, attrs) = highlight::resolve_style(s.kind, color_overrides);
            StyledSpan { start: s.start, end: s.end, fg, attrs }
        })
        .collect();
    // Last, over the lexer's guesses: the server knows a parameter from a
    // constant. Only tokens with a colour are here (`sync_semantic_tokens`);
    // the rest keep the local highlighting.
    styled.extend(buf.semantic_spans.iter().cloned());
    (styled, links)
}

// A relative link target is relative to the buffer's directory; an
// unnamed buffer has none, so no link.
fn absolute_link(buf: &TextBuffer, target: &str) -> Option<String> {
    crate::url::absolute(target, buf.path().and_then(|p| p.parent()))
}

// `:diag`: runs this language's linters over the whole buffer, the
// findings concatenated. Not per redraw: `:diag` is explicit, and the
// result is cached on `TextBuffer::diagnostics`, which clears itself
// when the buffer changes.
pub(crate) fn diagnose_buffer(buf: &TextBuffer) -> Vec<lint::Diagnostic> {
    let linters: Vec<&dyn Linter> = match language_of(buf).as_str() {
        lang if lang == snippet::DEFAULT_LANG => vec![&BashLinter],
        "json" => vec![&lint::JsonLinter],
        "toml" => vec![&lint::TomlLinter],
        _ => Vec::new(),
    };
    if linters.is_empty() {
        return Vec::new();
    }
    let text = buffer_text(buf);
    linters.iter().flat_map(|l| l.check(&text)).collect()
}

// A diagnostic's flat char offset (into `buffer_text`, as
// `lint::Diagnostic` and `Fix` use) as `(line, col)`, through
// `line_starts`. For jumping to a problem and for `apply_fix`.
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

// `diagnostic_position`'s inverse for a language server's
// `(line, character)`: the column in the negotiated encoding (`utf-32`
// is bish's counting; `utf-16` differs on emoji), the line through the
// `line_starts` prefix sum. Clamps at every step: a server may name a
// position past what the buffer now holds, from an older revision, and
// the finding probably still points at the right place.
pub(crate) fn diagnostic_offset(buf: &TextBuffer, starts: &[usize], line: usize, character: usize, encoding: lsp::PositionEncoding) -> usize {
    let last = buf.line_count().saturating_sub(1);
    if line > last {
        return starts[last] + buf.line_len(last);
    }
    let chars = buf.line_chars(line);
    starts[line] + lsp::from_server_column(&chars, character, encoding)
}

// Applies a batch of `lsp::TextEdit`s, returning how many changed
// anything. Last first: LSP ranges don't overlap and apply as though at
// once, so starting from the end keeps earlier ranges valid, as
// `tool.rs`'s `apply_fixes` does. Multi-line edits are normal (a
// formatter rewriting a block), hence `delete_range` and `insert_text`.
pub(crate) fn apply_text_edits(buf: &mut TextBuffer, edits: &[lsp::TextEdit], encoding: lsp::PositionEncoding) -> usize {
    // One edit with its ends in this buffer's coordinates.
    struct Resolved<'a> {
        from: (usize, usize),
        to: (usize, usize),
        text: &'a str,
    }
    let starts = line_starts(buf);
    // Resolved against the current text before anything moves.
    let mut resolved: Vec<Resolved<'_>> = edits
        .iter()
        .map(|edit| {
            let from = position_of(buf, &starts, edit.start, encoding);
            let to = position_of(buf, &starts, edit.end, encoding);
            Resolved { from, to: to.max(from), text: edit.text.as_str() }
        })
        .collect();
    crate::sort::sort_by_key(&mut resolved, |edit| std::cmp::Reverse(edit.from));
    let mut applied = 0;
    for Resolved { from, to, text } in resolved {
        let unchanged = from == to && text.is_empty();
        if unchanged {
            continue;
        }
        if from != to {
            buf.delete_range(&crate::bishedit::motion::MotionRange { shape: crate::bishedit::motion::MotionShape::Exclusive, from, to });
        }
        if !text.is_empty() {
            buf.insert_text(from, text);
        }
        applied += 1;
    }
    applied
}

// A server position as `(row, col)`, clamped; see `diagnostic_offset`.
fn position_of(buf: &TextBuffer, starts: &[usize], position: (usize, usize), encoding: lsp::PositionEncoding) -> (usize, usize) {
    let offset = diagnostic_offset(buf, starts, position.0, position.1, encoding);
    diagnostic_position(buf, offset)
}

// A server's findings as diagnostics this editor draws. Here, not in
// lsp.rs, because only something holding the text can turn
// `(line, character)` into an offset. `fix` is `None`: an LSP fix is a
// code action, a separate request, and multi-range.
pub(crate) fn diagnostics_from_server(buf: &TextBuffer, findings: &[lsp::Finding], encoding: lsp::PositionEncoding) -> Vec<lint::Diagnostic> {
    let starts = line_starts(buf);
    findings
        .iter()
        .map(|f| lint::Diagnostic {
            start: diagnostic_offset(buf, &starts, f.start.0, f.start.1, encoding),
            end: diagnostic_offset(buf, &starts, f.end.0, f.end.1, encoding),
            severity: match f.severity {
                1 => lint::Severity::Error,
                2 => lint::Severity::Warning,
                3 => lint::Severity::Info,
                _ => lint::Severity::Hint,
            },
            code: std::borrow::Cow::Owned(f.code.clone()),
            // Always `Some`, the server's command when it named no source:
            // it tells a relayed finding from bish's own, so a new
            // publication replaces the right subset.
            source: Some(f.source.clone().unwrap_or_else(|| "lsp".to_string())),
            message: f.message.clone(),
            fix: None,
        })
        .collect()
}

// Applies `diagnostic`'s fix, if it has one. `delete_range` clears
// `buf.diagnostics`, so the caller re-runs `diagnose_buffer` for
// correct offsets.
pub(crate) fn apply_fix(buf: &mut TextBuffer, diagnostic: &lint::Diagnostic) -> bool {
    let Some(fix) = &diagnostic.fix else { return false };
    let from = diagnostic_position(buf, fix.start);
    let to = diagnostic_position(buf, fix.end);
    let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from, to };
    buf.delete_range(&range);
    buf.insert_text(from, &fix.replacement);
    true
}

// Every fix, as a pre-save hook needs. Right to left by start, as
// `tool.rs`'s `apply_fixes` does: offsets are into the original text,
// so a later splice never moves one still to come. An overlapping fix
// is skipped.
fn apply_all_fixes(buf: &mut TextBuffer, diagnostics: &[lint::Diagnostic]) -> usize {
    let mut candidates: Vec<&lint::Diagnostic> = diagnostics.iter().filter(|d| d.fix.is_some()).collect();
    crate::sort::sort_by_key(&mut candidates, |d| std::cmp::Reverse(d.fix.as_ref().unwrap().start));
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

// A language's pre-save hook: the buffer's text in, diagnostics with
// fixes out, the shape of `Linter::check`. `Err` leaves the file alone,
// for a hook that can't make sense of the text (`BashFormatter::check`
// on a parse error).
type PreSaveHook = fn(&str) -> Result<Vec<lint::Diagnostic>, String>;

fn bash_format_hook(text: &str) -> Result<Vec<lint::Diagnostic>, String> {
    Ok(strip_buffer_implicit_trailing_newline(text, BashFormatter.check(text)?))
}

// A hook writes its fixes for a file ending in one newline, but a
// buffer's text has none: `TextBuffer::save` adds it. Applied as is, a
// fix reaching the end would add a blank last line, so the newline is
// trimmed here. A fix left empty (the text was already formatted) is
// dropped, since `apply_fix` counts any fix as applied and the buffer
// would report `Formatted` instead of `AlreadyFormatted`.
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

// The pre-save hooks for a language; only bash has any.
fn pre_save_hooks(language: &str) -> &'static [PreSaveHook] {
    match language {
        snippet::DEFAULT_LANG => &[bash_format_hook],
        _ => &[],
    }
}

// Runs one hook on the buffer's text and applies its fixes, returning
// how many landed. Shared by `run_pre_save_hooks` and `format_buffer`.
fn run_one_hook(buf: &mut TextBuffer, hook: PreSaveHook) -> Result<usize, String> {
    let text = buffer_text(buf);
    let diagnostics = hook(&text)?;
    Ok(apply_all_fixes(buf, &diagnostics))
}

// Before `:w`/`:wq`/`:x`: runs each pre-save hook for this language in
// order, applying its fixes so they're in the same write. Each reads
// the text afresh, seeing the previous hook's edits. A hook that errors
// (a script that doesn't parse yet) is skipped: refusing to save would
// be worse than not reformatting.
pub(crate) fn run_pre_save_hooks(buf: &mut TextBuffer) {
    // A read-only buffer's `:w` is refused or writes the content elsewhere
    // as it stands; neither is a reason to reformat it.
    if buf.is_readonly() {
        return;
    }
    for hook in pre_save_hooks(&language_of(buf)) {
        let _ = run_one_hook(buf, *hook);
    }
}

// `:format`'s result. Unlike saving, it can report, so it tells "nothing
// to do" from "couldn't try".
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FormatOutcome {
    Formatted,
    AlreadyFormatted,
    // This language has no pre-save hook, as opposed to one that changed
    // nothing.
    NotSupported,
    // A hook refused (bash's on a parse error), with its message.
    Error(String),
}

// `:format`: the pre-save hooks run by hand, reporting the outcome.
// Stops at the first hook that errors, leaving that hook's changes
// unmade (hooks only change `buf` on success).
pub(crate) fn format_buffer(buf: &mut TextBuffer) -> FormatOutcome {
    let hooks = pre_save_hooks(&language_of(buf));
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

/// The file as it is on disk now, decoded as the buffer was, so a
/// latin-1 file compares with its own text.
pub(crate) fn disk_text(buf: &TextBuffer) -> Result<String, String> {
    let path = buf.path().ok_or_else(|| "no file name".to_string())?;
    Ok(crate::encoding::decode(&std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e))?).text)
}

/// The file as git has it at `rev`, or in the index for `None`. Empty
/// when the revision lacks it, so every line reads as new.
pub(crate) fn git_text(buf: &TextBuffer, rev: Option<&str>) -> Result<String, String> {
    let path = git_path(buf)?;
    Ok(crate::git::file_at_rev(&path, rev)?.unwrap_or_default())
}

// What every `:git` gutter needs first: a file, and a git to ask.
fn git_path(buf: &TextBuffer) -> Result<std::path::PathBuf, String> {
    let path = buf.path().ok_or_else(|| "no file name".to_string())?.to_path_buf();
    if !crate::git::available() {
        return Err("git executable not found".to_string());
    }
    Ok(path)
}

// `:git blame [REV]`: turns the blame column off (`Ok(false)`) or runs
// `git blame` and turns it on (`Ok(true)`). The blame describes a
// committed version, so it's aligned to the buffer by diffing with that
// content (`git::align_to`); a line with no counterpart gets `None`,
// drawn as its own marker (`render_blame_cell`).
pub(crate) fn toggle_git_blame(buf: &mut TextBuffer, rev: Option<&str>, trusted: bool) -> Result<bool, String> {
    if buf.blame.is_some() {
        buf.blame = None;
        return Ok(false);
    }
    let path = git_path(buf)?;
    // What the blame is indexed by: that revision's content, or with no REV
    // the file on disk, git blame's default.
    let old = match rev {
        Some(rev) => crate::git::file_at_rev(&path, Some(rev))?.unwrap_or_default(),
        None => {
            let bytes = std::fs::read(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
            crate::encoding::decode(&bytes).text
        }
    };
    // `trusted` is the caller's `git` capability decision, passed in so
    // this tests without a trust store. Blame runs the repository's
    // filter/textconv (`git::blame`), hence the gate.
    let blamed = crate::git::blame(&path, rev, trusted)?;
    let old_lines: Vec<&str> = old.lines().collect();
    let current = buf.text();
    let current_lines: Vec<&str> = current.lines().collect();
    let aligned =
        crate::git::align_to(&old_lines, &current_lines).into_iter().map(|from| from.and_then(|i| blamed.get(i).cloned())).collect::<Vec<_>>();
    buf.blame = Some(aligned);
    Ok(true)
}

// What a bare `:git show` means while blame is on: the commit the
// gutter names on the cursor's line. `None` while blame is off, so it
// means HEAD. A line with no commit is an error rather than HEAD, which
// the gutter never named.
pub(crate) fn blamed_commit_at_cursor(buf: &TextBuffer) -> Option<Result<String, String>> {
    let blame = buf.blame.as_ref()?;
    let (row, _) = buf.cursor();
    Some(match blame.get(row).cloned().flatten() {
        Some(line) if line.short_commit.chars().all(|c| c == '0') => Err("this line is not committed yet".to_string()),
        Some(line) => Ok(line.short_commit),
        None => Err("this line is not in the revision blame was run against".to_string()),
    })
}

// `:git diff [REV]`: toggles, like blame. The buffer's current text is
// compared, saved or not. A file the revision lacks diffs against
// nothing, so every line reads as added.
pub(crate) fn toggle_git_diff(buf: &mut TextBuffer, rev: Option<&str>) -> Result<bool, String> {
    if buf.diff.is_some() {
        buf.diff = None;
        return Ok(false);
    }
    let old = git_text(buf, rev)?;
    let old_lines: Vec<&str> = old.lines().collect();
    let current = buf.text();
    let current_lines: Vec<&str> = current.lines().collect();
    buf.diff = Some(crate::git::marks_from_diff(&old_lines, &current_lines));
    Ok(true)
}

// `:diff`: what changed since the last save, the buffer against the
// file on disk, with no git needed (`crate::diff` through
// `git::marks_from_diff`). It shares `buf.diff` with `:git diff`, so
// turning one on turns the other off.
pub(crate) fn toggle_buffer_diff(buf: &mut TextBuffer) -> Result<bool, String> {
    if buf.diff.is_some() {
        buf.diff = None;
        return Ok(false);
    }
    let on_disk = disk_text(buf)?;
    let disk_lines: Vec<&str> = on_disk.lines().collect();
    let current = buf.text();
    let current_lines: Vec<&str> = current.lines().collect();
    buf.diff = Some(crate::git::marks_from_diff(&disk_lines, &current_lines));
    Ok(true)
}

// The colour and attributes of a diagnostic's underline: the one place
// a severity becomes presentation. An exhaustive match, so a new
// severity must be handled here.
fn diagnostic_style(buf: &TextBuffer, severity: lint::Severity) -> (vt100::Color, vt100::CellAttrs) {
    let element = severity_element(severity);
    crate::theme::resolve(element, buf.colors.as_ref())
}

fn severity_element(severity: lint::Severity) -> crate::theme::Ui {
    match severity {
        lint::Severity::Error => crate::theme::Ui::Error,
        lint::Severity::Warning => crate::theme::Ui::Warning,
        lint::Severity::Info => crate::theme::Ui::Info,
        lint::Severity::Hint => crate::theme::Ui::Hint,
    }
}

// `spans_for_line` for diagnostics: the same offsets and clamp, each
// coloured by its own severity.
fn diagnostic_spans_for_line(buf: &TextBuffer, diagnostics: &[lint::Diagnostic], line_start: usize, line_len: usize) -> Vec<StyledSpan> {
    let line_end = line_start + line_len;
    diagnostics
        .iter()
        .filter(|d| d.start < line_end && d.end > line_start)
        .map(|d| {
            let (fg, attrs) = diagnostic_style(buf, d.severity);
            StyledSpan { start: d.start.saturating_sub(line_start), end: (d.end - line_start).min(line_len), fg, attrs }
        })
        .collect()
}

// The spans touching one line (`line_start`, `line_len` long), as
// offsets into that line. A span from an earlier line or continuing to
// a later one is clamped, not dropped, so a multi-line string is
// coloured on every line.
fn spans_for_line(spans: &[StyledSpan], line_start: usize, line_len: usize) -> Vec<StyledSpan> {
    let line_end = line_start + line_len;
    spans
        .iter()
        .filter(|s| s.start < line_end && s.end > line_start)
        .map(|s| StyledSpan { start: s.start.saturating_sub(line_start), end: (s.end - line_start).min(line_len), fg: s.fg, attrs: s.attrs })
        .collect()
}

// `spans_for_line` for links, so a URL across two wrapped rows is one
// link on both.
fn links_for_line(links: &[highlight::LinkSpan], line_start: usize, line_len: usize) -> Vec<highlight::LinkSpan> {
    let line_end = line_start + line_len;
    links
        .iter()
        .filter(|l| l.start < line_end && l.end > line_start)
        .map(|l| highlight::LinkSpan { start: l.start.saturating_sub(line_start), end: (l.end - line_start).min(line_len), url: l.url.clone() })
        .collect()
}

// Draws one screen row: cells `start_cell..end_cell` of a line, within
// `cols` columns. Every span is already in this window, in cells
// (`to_window`), so with a tabular display one place converts between
// characters and cells. Layers: syntax, then diagnostics (so an
// underline isn't hidden), then selection in reverse video (so it reads
// over any colour).
#[allow(clippy::too_many_arguments)]
fn render_row(
    out: &mut String,
    buf: &TextBuffer,
    display: &crate::bishedit::tabular::Row,
    start_cell: usize,
    end_cell: usize,
    cols: usize,
    line_styled: &[StyledSpan],
    diag_styled: &[StyledSpan],
    highlights: &[StyledSpan],
    // Attribute-only marks over the resolved colours, such as a language
    // server's same-symbol highlight. See `highlight::compose_attrs`.
    attr_overlay: &[StyledSpan],
    links: &[highlight::LinkSpan],
) {
    let limit = end_cell.min(display.cells.len());
    let mut chars: Vec<char> = Vec::with_capacity(cols);
    let mut used = 0;
    let mut at = start_cell.min(limit);
    while at < limit {
        let w = char_width(display.cells[at]);
        if used + w > cols {
            break;
        }
        chars.push(display.cells[at]);
        used += w;
        at += 1;
    }
    // With wrapping off, a line continuing past an edge says so: vim's
    // `listchars` `extends`/`precedes`, empty by default.
    let truncated_right = at < display.cells.len();
    let truncated_left = start_cell > 0;
    while used < cols {
        chars.push(' ');
        used += 1;
    }

    // Clamped to `chars.len()`, not `cols`: a wide character takes two
    // columns but one slot, so with one in the window `chars` is shorter
    // than `cols`.
    let clamp = |spans: &[StyledSpan]| -> Vec<StyledSpan> {
        spans
            .iter()
            .filter(|s| s.start < chars.len())
            .map(|s| StyledSpan { start: s.start, end: s.end.min(chars.len()), fg: s.fg, attrs: s.attrs })
            .collect()
    };
    let line_styled = clamp(line_styled);
    let diag_styled = clamp(diag_styled);
    let highlights = clamp(highlights);
    let attr_overlay = clamp(attr_overlay);

    let links: Vec<highlight::LinkSpan> = links
        .iter()
        .filter(|l| l.start < chars.len())
        .map(|l| highlight::LinkSpan { start: l.start, end: l.end.min(chars.len()), url: l.url.clone() })
        .collect();
    let mut cells = highlight::compose(&chars, &[&line_styled, &diag_styled, &highlights]);
    highlight::compose_attrs(&mut cells, &attr_overlay);
    // Inlay hints, which aren't in the file, are dimmed so they read as
    // annotation: cells with no source character that aren't blank. Padding
    // and tab spaces have no source either, but dimming a space shows
    // nothing. Not a span layer: spans address the line's characters, and a
    // hint has none.
    for (i, cell) in cells.iter_mut().enumerate() {
        if cell.ch != ' ' && display.source_at.get(start_cell + i).copied().flatten().is_none() {
            cell.attrs.dim = true;
        }
    }
    let mut rendered = highlight::render_linked(&cells, &links);
    if !buf.wrap.wrap {
        if truncated_right && !buf.wrap.extends.is_empty() {
            rendered = overlay_marker(&rendered, cols, &buf.wrap.extends, true);
        }
        if truncated_left && !buf.wrap.precedes.is_empty() {
            rendered = overlay_marker(&rendered, cols, &buf.wrap.precedes, false);
        }
    }
    out.push_str(&rendered);
}

// Puts `marker` in the row's last (or first) column, over content the
// reader can't see anyway.
fn overlay_marker(rendered: &str, cols: usize, marker: &str, at_end: bool) -> String {
    let width: usize = marker.chars().map(char_width).sum();
    if width == 0 || width > cols {
        return rendered.to_string();
    }
    let keep = if at_end { cols - width } else { width };
    let (head, tail) = split_rendered(rendered, keep);
    if at_end { format!("{head}\x1b[2m{marker}\x1b[0m") } else { format!("\x1b[2m{marker}\x1b[0m{tail}") }
}

// Splits already-styled text at a display column, keeping every SGR
// sequence it passes on both sides so neither half loses its colours.
fn split_rendered(rendered: &str, at: usize) -> (String, String) {
    let mut head = String::new();
    let mut tail = String::new();
    let mut width = 0;
    let mut chars = rendered.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            let mut esc = String::from(c);
            esc.push(chars.next().expect("just peeked"));
            for c2 in chars.by_ref() {
                esc.push(c2);
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
            head.push_str(&esc);
            tail.push_str(&esc);
            continue;
        }
        if width < at {
            width += char_width(c);
            head.push(c);
        } else {
            tail.push(c);
        }
    }
    (head, tail)
}

// The pattern this frame marks matches of, if any. Like repl.rs's
// `active_search_pattern`, duplicated since each context drives a
// different `Buffer`.
fn active_search_pattern(vk: &VimKeys, buf: &TextBuffer) -> Option<String> {
    let pending = vk.pending_display();
    if let Some(rest) = pending.strip_prefix('/').or_else(|| pending.strip_prefix('?')) {
        // A pattern being typed shows regardless of `:noh`: it's live
        // feedback, not a leftover highlight.
        return if rest.is_empty() { None } else { Some(rest.to_string()) };
    }
    if !vk.search_highlight_on() {
        return None;
    }
    if vk.last_search_is_word() {
        motion::word_under_cursor(buf, buf.cursor())
    } else {
        let text = vk.last_search_text();
        if text.is_empty() { None } else { Some(text.to_string()) }
    }
}

// A closed fold's row: vim's fold text, dashed out to the pane, in the
// line-number colour so it reads as the editor's.
fn fold_row(buf: &TextBuffer, line: usize, end: usize, width: usize) -> String {
    let level = buf.folds.level(line, end);
    let first: String = buf.line_chars(line).into_iter().map(|c| if c == '\t' { ' ' } else { c }).collect();
    let text = crate::bishedit::fold::fold_text(level, end - line + 1, &first);
    let mut shown = String::new();
    let mut cols = 0;
    for ch in text.chars() {
        let w = crate::bishedit::unicode_width::char_width(ch);
        if cols + w > width {
            break;
        }
        cols += w;
        shown.push(ch);
    }
    shown.push_str(&"-".repeat(width - cols));
    let sgr = crate::theme::sgr(crate::theme::Ui::LineNumber, buf.colors.as_ref());
    format!("{sgr}{shown}\x1b[0m")
}

// The bracket under the cursor and its partner, or neither. Insert and
// Replace leave the cursor past what was typed, so the bracket behind
// it counts, lighting a pair as it's closed, as in vim. Nothing with
// `matchbrackets` off, which also skips the search.
fn matching_brackets(buf: &TextBuffer, mode: EditorMode) -> Vec<(usize, usize)> {
    if !buf.matchbrackets {
        return Vec::new();
    }
    let (line, col) = buf.cursor();
    let here = motion::bracket_pair_at(buf, (line, col));
    let behind = (mode != EditorMode::Normal && col > 0).then(|| motion::bracket_pair_at(buf, (line, col - 1))).flatten();
    match here.or(behind) {
        Some((at, partner)) => vec![at, partner],
        None => Vec::new(),
    }
}

// The frame as one escape-coded string, content rows and then the cursor;
// the status row is drawn separately (`repl::render_global_status_row`).
// Separate from repl.rs's `render_normal_mode_frame`, which reads a
// `ScreenBuffer` directly. `row_origin`/`col_origin` place the pane's
// row and column 0: the pane's position on the terminal, or 0/0 for a
// session's own pane-relative grid (`freeze_editor_frame`).
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
    // What `hlsearch` draws, once per frame.
    let search_pattern = active_search_pattern(vk, buf);
    // Once per frame; see `matching_brackets`.
    let matched_brackets = matching_brackets(buf, mode);
    // At least one column for content, however wide the gutter, or
    // `content_cols` would underflow.
    let gutter_width = total_gutter_width(buf).min(rect.cols.saturating_sub(1));
    let content_cols = rect.cols - gutter_width;
    // How far rendering is scrolled right (see `Buffer::viewport_left`),
    // kept near the cursor by `scroll_to_show_cursor`, not clamped here.
    let hoffset = buf.viewport_left();
    let active = if mode == EditorMode::Normal { crate::repl::active_visual_range(vk, buf) } else { None };
    // Once for the whole buffer, so multi-line constructs are seen whole.
    let (whole_styled, whole_links) = buffer_spans(buf, color_overrides);
    // `hyperlinks` off keeps a link's underline but emits nothing
    // clickable.
    let whole_links = if buf.hyperlinks { whole_links } else { Vec::new() };
    // Once per frame: every row must agree on the columns.
    let layout = tabular_layout(buf);
    let starts = line_starts(buf);
    let mut out = String::new();
    let rows = visible_rows(buf, content_cols, content_rows);
    for r in 0..content_rows {
        out.push_str(&format!("\x1b[{};{}H\x1b[K", row_origin + r + 1, col_origin + 1));
        let Some(row) = rows.get(r) else {
            // Past the end of the buffer the gutter keeps its width, so the
            // content doesn't shift.
            render_gutter(&mut out, buf, &starts, total, false);
            continue;
        };
        let line = row.line;
        render_gutter(&mut out, buf, &starts, line, row.first);
        if let Some(end) = row.fold {
            out.push_str(&fold_row(buf, line, end, content_cols));
            continue;
        }
        {
            // The char index this row's window starts at, the rebase point
            // for `selection_columns_in_line`. With wrapping the segment
            // says where the row starts, and there's no horizontal scroll.
            let wrapping = buf.wrap.wrap;
            let display = display_row(buf, line, layout.as_ref());
            let prefix = continuation_prefix(buf, row.seg.indent);
            let avail = content_cols.saturating_sub(row.seg.indent);
            // Where this row starts and stops in drawn cells: the wrap
            // segment, or the horizontal scroll, which is in cells.
            let (start_cell, end_cell) = if wrapping {
                (display.cell_of[row.seg.start.min(buf.line_len(line))], display.cell_of[row.seg.end.min(buf.line_len(line))])
            } else {
                (cell_at_col(&display.cells, hoffset), display.cells.len())
            };
            let line_len = buf.line_len(line);
            let search_matches = match &search_pattern {
                Some(pattern) => motion::find_matches_in_line(buf, line, pattern),
                None => Vec::new(),
            };
            let mut highlights: Vec<StyledSpan> = Vec::new();
            for range in buf.selections.iter().chain(active.iter()) {
                let Some((start, end)) = selection_columns_in_line(range, line, 0, line_len + 1) else { continue };
                // A linewise selection reaches the pane's edge, padding
                // included: it selects the line.
                let span =
                    if range.shape == motion::MotionShape::Linewise { Some((0, avail)) } else { to_window(&display, start_cell, avail, start, end) };
                if let Some((start, end)) = span {
                    highlights.push(StyledSpan {
                        start,
                        end,
                        fg: vt100::Color::Default,
                        attrs: vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() },
                    });
                }
            }
            // Search matches, underlined, so a match inside a reverse-video
            // selection reads as both.
            for (start, end) in search_matches.iter().copied() {
                let Some((start, end)) = to_window(&display, start_cell, avail, start, end) else { continue };
                highlights.push(StyledSpan {
                    start,
                    end,
                    fg: vt100::Color::Default,
                    attrs: vt100::CellAttrs { underline: true, ..vt100::CellAttrs::default() },
                });
            }
            // A live `abbr` snippet's placeholders, marked as at the shell
            // prompt (editor.rs's `snippet_layer`): reverse video on the
            // active one, underline on the rest; dropped when scrolled out
            // of the window.
            for hole in buf.snippet_holes.iter().filter(|h| h.line == line) {
                let Some((start, end)) = to_window(&display, start_cell, avail, hole.start, hole.end) else {
                    continue;
                };
                let attrs = if hole.active {
                    vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() }
                } else {
                    vt100::CellAttrs { underline: true, ..vt100::CellAttrs::default() }
                };
                highlights.push(StyledSpan { start, end, fg: vt100::Color::Default, attrs });
            }
            let map = |spans: Vec<StyledSpan>| -> Vec<StyledSpan> {
                spans
                    .into_iter()
                    .filter_map(|s| {
                        to_window(&display, start_cell, avail, s.start, s.end).map(|(start, end)| StyledSpan { start, end, fg: s.fg, attrs: s.attrs })
                    })
                    .collect()
            };
            let line_styled = map(spans_for_line(&whole_styled, starts[line], line_len));
            // The server's `documentHighlight`, on a separate axis so an
            // occurrence keeps its colour and gains an underline.
            let mut line_marks = map(spans_for_line(&buf.document_highlights, starts[line], line_len));
            // On that axis too: a bracket keeps its colour and gains the
            // mark.
            line_marks.extend(matched_brackets.iter().filter(|(at, _)| *at == line).filter_map(|(_, col)| {
                to_window(&display, start_cell, avail, *col, *col + 1).map(|(start, end)| StyledSpan {
                    start,
                    end,
                    fg: vt100::Color::Default,
                    attrs: vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() },
                })
            }));
            let diag_styled = map(diagnostic_spans_for_line(buf, &buf.diagnostics, starts[line], line_len));
            let links: Vec<highlight::LinkSpan> = links_for_line(&whole_links, starts[line], line_len)
                .into_iter()
                .filter_map(|l| {
                    to_window(&display, start_cell, avail, l.start, l.end).map(|(start, end)| highlight::LinkSpan { start, end, url: l.url })
                })
                .collect();
            out.push_str(&prefix);
            render_row(&mut out, buf, &display, start_cell, end_cell, avail, &line_styled, &diag_styled, &highlights, &line_marks, &links);
        }
    }

    let (cl, cc) = buf.cursor();
    // From the drawn rows, so on a wrapped line the cursor goes on its
    // segment's row.
    let cursor_row = rows.iter().position(|row| row.line == cl && row.seg.contains(cc));
    let screen_row = match cursor_row {
        Some(r) => r,
        None => rows.iter().rposition(|row| row.line == cl).unwrap_or(0),
    }
    .min(content_rows.saturating_sub(1));
    // The cursor's display column, through the drawn cells: wide
    // characters and tabular padding before it make that differ from its
    // char index.
    let cursor_display = display_row(buf, cl, layout.as_ref());
    let cursor_cell = cursor_display.cell_of[cc.min(buf.line_len(cl))];
    let cursor_abs = col_at_cell(&cursor_display.cells, cursor_cell);
    let (row_start, row_indent) = match rows.get(screen_row) {
        Some(row) if buf.wrap.wrap => (row.seg.start, row.seg.indent),
        _ => (0, 0),
    };
    let row_start_col = col_at_cell(&cursor_display.cells, cursor_display.cell_of[row_start.min(buf.line_len(cl))]);
    let screen_col = gutter_width
        + row_indent
        + if buf.wrap.wrap {
            cursor_abs.saturating_sub(row_start_col).min(content_cols.saturating_sub(row_indent + 1))
        } else {
            cursor_abs.saturating_sub(hoffset).min(content_cols.saturating_sub(1))
        };
    // On a closed fold's row the cursor sits at the row's start, as in vim.
    let screen_col = if rows.get(screen_row).is_some_and(|row| row.fold.is_some()) { gutter_width } else { screen_col };
    out.push_str(&format!("\x1b[{};{}H\x1b[?25h", row_origin + screen_row + 1, col_origin + screen_col + 1));
    out.push_str(cursor_shape(buf, mode, vk));
    out
}

// DECSCUSR (`CSI Ps SP q`): the cursor's shape shows the mode without
// reading the status line. Sent with every frame, since a job's output
// or another program may have changed it meanwhile; four bytes.
pub(crate) fn cursor_shape(buf: &TextBuffer, mode: EditorMode, vk: &VimKeys) -> &'static str {
    if !buf.cursorshape {
        return "";
    }
    match mode {
        // A bar sits between characters, where an insertion goes.
        EditorMode::Insert => "\x1b[6 q",
        // An underline: this character is about to be replaced.
        EditorMode::Replace => "\x1b[4 q",
        // Normal mode's cursor is on a character: a block. Mid-operator (`d`,
        // `c`, `y` awaiting a motion) an underline, since the next key means
        // something different.
        EditorMode::Normal if !vk.is_idle_except_count() && !vk.is_visual() => "\x1b[4 q",
        EditorMode::Normal => "\x1b[2 q",
    }
}

/// Back to the terminal's default, for whoever gives the terminal back.
pub(crate) const CURSOR_SHAPE_RESET: &str = "\x1b[0 q";

pub fn render_editor_frame(
    buf: &TextBuffer,
    vk: &VimKeys,
    mode: EditorMode,
    rect: Rect,
    term_rows: usize,
    term_cols: usize,
    color_overrides: Option<&highlight::ColorOverrides>,
) {
    let mut out = crate::repl::render_global_status_row(&status_text(buf, vk, mode, term_cols), term_rows);
    out.push_str(&build_editor_frame(buf, vk, mode, rect, rect.row, rect.col, color_overrides));
    // An editor paints the terminal, never the pane's grid, so this is the
    // only record of what it shows (`repl::record_paint`).
    crate::repl::record_paint(rect, &out);
    print!("{}", out);
    let _ = io::stdout().flush();
}

// Draws this editor's state into `screen` instead of the terminal, at
// pane-relative positions, so a compositor redraw of a pane whose top
// frame is a detached `Frame::Edit` shows its last state. Like
// `freeze_idle_prompt`, but the content spans the whole pane.
pub fn freeze_editor_frame(
    screen: &Rc<RefCell<vt100::Screen>>,
    buf: &TextBuffer,
    vk: &VimKeys,
    rect: Rect,
    color_overrides: Option<&highlight::ColorOverrides>,
) {
    let framed = build_editor_frame(buf, vk, EditorMode::Normal, rect, 0, 0, color_overrides);
    screen.borrow_mut().feed(framed.as_bytes());
}

#[cfg(test)]
mod click_position_tests {
    use super::*;

    fn rect() -> Rect {
        Rect { row: 2, col: 10, rows: 8, cols: 40 }
    }

    fn buf(text: &str) -> TextBuffer {
        let mut b = TextBuffer::new_unnamed(8);
        b.insert_text((0, 0), text);
        b.set_cursor(0, 0);
        b
    }

    // The gutter `build_editor_frame` draws, read rather than hardcoded so
    // a new gutter column doesn't silently break these tests.
    fn gutter(b: &TextBuffer) -> usize {
        total_gutter_width(b).min(rect().cols - 1)
    }

    #[test]
    fn a_click_lands_on_the_character_under_it() {
        let b = buf("alpha
bravo
charlie");
        let g = gutter(&b);
        // Third row of the pane, fourth character.
        assert_eq!(position_at_screen(&b, rect(), 2 + 2, 10 + g + 3), Some((2, 3)));
    }

    // `position_at_screen` maps a gutter click to the line's first
    // character; `gutter_at_screen` says what was pointed at.
    #[test]
    fn the_gutter_is_told_apart_from_the_text() {
        let b = buf("alpha\nbravo\ncharlie");
        let g = gutter(&b);
        // The diagnostic column is always drawn (two cells), so it's leftmost
        // here; the line numbers fill the rest of the gutter.
        assert_eq!(gutter_at_screen(&b, rect(), 2 + 2, 10), Some((2, GutterKind::Diagnostic)));
        assert_eq!(gutter_at_screen(&b, rect(), 2 + 2, 10 + g - 1), Some((2, GutterKind::LineNumber)));
        // One past the gutter is the text.
        assert_eq!(gutter_at_screen(&b, rect(), 2 + 2, 10 + g), None);
        // Where `position_at_screen` starts, so every cell of a row is
        // accounted for once.
        assert_eq!(position_at_screen(&b, rect(), 2 + 2, 10 + g), Some((2, 0)));
    }

    #[test]
    fn each_gutter_column_is_found_at_its_own_offset() {
        let mut b = buf("alpha\nbravo\ncharlie");
        b.breakpoints.insert(1);
        b.diff = Some(std::collections::HashMap::from([(0, crate::git::DiffMark::Added)]));
        // Left to right, in `GUTTER_COLUMNS` order. The pane's first row is
        // 2.
        let mut at = 10;
        for expected in [GutterKind::Breakpoint, GutterKind::Diff, GutterKind::Diagnostic, GutterKind::LineNumber] {
            let width = GUTTER_COLUMNS.iter().find(|c| c.kind == expected).map(|c| (c.width)(&b)).expect("a column of that kind");
            assert!(width > 0, "{expected:?} should be drawn for this buffer");
            assert_eq!(gutter_at_screen(&b, rect(), 2, at), Some((0, expected)), "at column {at}");
            assert_eq!(gutter_at_screen(&b, rect(), 2, at + width - 1), Some((0, expected)), "last cell of {expected:?}");
            at += width;
        }
    }

    // A column not drawn can't be pointed at, or a zero-width column would
    // swallow the next one's first cell.
    #[test]
    fn a_column_of_no_width_is_never_the_answer() {
        let b = buf("alpha\nbravo");
        assert!(b.breakpoints.is_empty() && !b.debug_attached, "so the breakpoint column has no width");
        let hit = gutter_at_screen(&b, rect(), 2, 10);
        assert_ne!(hit.map(|(_, k)| k), Some(GutterKind::Breakpoint), "the first cell belongs to whatever is actually drawn there");
    }

    // The breakpoint column stays for the whole debug session, rather than
    // shifting the file sideways with the first and last breakpoint.
    #[test]
    fn the_breakpoint_column_is_there_for_the_whole_session() {
        let mut b = buf("alpha\nbravo");
        assert_eq!(breakpoint_column_width(&b), 0);
        b.debug_attached = true;
        assert_eq!(breakpoint_column_width(&b), 2, "present before any breakpoint exists");
        b.breakpoints.insert(1);
        assert_eq!(breakpoint_column_width(&b), 2, "and unchanged by setting one");
        b.debug_attached = false;
        assert_eq!(breakpoint_column_width(&b), 2, "a breakpoint alone still earns it");
    }

    #[test]
    fn a_click_outside_the_pane_is_not_this_panes_business() {
        let b = buf("alpha
bravo");
        assert_eq!(position_at_screen(&b, rect(), 1, 15), None, "above");
        assert_eq!(position_at_screen(&b, rect(), 2, 9), None, "left");
        assert_eq!(position_at_screen(&b, rect(), 2, 50), None, "right");
        assert_eq!(position_at_screen(&b, rect(), 2 + 8, 15), None, "below");
    }

    #[test]
    fn a_click_below_the_last_line_hits_nothing() {
        let b = buf("only one line");
        assert_eq!(position_at_screen(&b, rect(), 2 + 3, 15), None);
    }

    #[test]
    fn a_click_in_the_gutter_or_past_the_end_snaps_to_the_line() {
        let b = buf("alpha
bravo");
        let g = gutter(&b);
        assert_eq!(position_at_screen(&b, rect(), 2 + 1, 10), Some((1, 0)), "in the gutter -> line start");
        // Past "bravo"'s five characters: its last one.
        assert_eq!(position_at_screen(&b, rect(), 2 + 1, 10 + g + 30), Some((1, 4)));
    }

    #[test]
    fn a_click_follows_the_viewport_rather_than_the_top_of_the_buffer() {
        let mut b = buf("a
b
c
d
e
f
g
h
i
j");
        b.set_viewport_top(4);
        let g = gutter(&b);
        assert_eq!(position_at_screen(&b, rect(), 2, 10 + g), Some((4, 0)), "the pane's first row is viewport_top");
        assert_eq!(position_at_screen(&b, rect(), 2 + 2, 10 + g), Some((6, 0)));
    }

    #[test]
    fn a_click_past_a_wide_character_counts_columns_not_chars() {
        // Two double-width characters, then ASCII: display column 4 is the
        // third character, not the fourth.
        let b = buf("你好ab");
        let g = gutter(&b);
        assert_eq!(position_at_screen(&b, rect(), 2, 10 + g + 4), Some((0, 2)));
        assert_eq!(position_at_screen(&b, rect(), 2, 10 + g + 2), Some((0, 1)));
    }
}

#[cfg(test)]
mod edit_args_tests {
    use super::*;

    fn parse(line: &str) -> Result<Vec<EditTarget>, String> {
        let args: Vec<String> = line.split_whitespace().map(String::from).collect();
        parse_edit_args(&args)
    }

    fn paths(line: &str) -> Vec<Option<String>> {
        parse(line).unwrap().into_iter().map(|t| t.path).collect()
    }

    #[test]
    fn no_arguments_is_one_fresh_unnamed_buffer() {
        assert_eq!(paths(""), vec![None]);
    }

    #[test]
    fn every_file_argument_becomes_its_own_target() {
        assert_eq!(paths("a.sh b.sh c.sh"), vec![Some("a.sh".into()), Some("b.sh".into()), Some("c.sh".into())]);
    }

    #[test]
    fn a_typo_ed_flag_is_an_error_rather_than_a_filename() {
        // A typo'd flag is an error, not a file named `--hxe`.
        assert!(parse("--hxe data.bin").is_err());
    }

    #[test]
    fn a_double_dash_lets_a_dash_leading_filename_through() {
        assert_eq!(paths("-- --weird.txt"), vec![Some("--weird.txt".into())]);
    }

    #[test]
    fn a_bare_dash_is_a_filename_not_a_flag() {
        assert_eq!(paths("-"), vec![Some("-".into())]);
    }

    #[test]
    fn hex_binds_to_the_file_that_follows_it_and_no_others() {
        let t = parse("script.sh --hex core.bin notes.txt").unwrap();
        assert_eq!(t.len(), 3);
        assert!(!t[0].hex, "the file before --hex is still text");
        assert!(t[1].hex, "--hex applies to the file right after it");
        assert!(!t[2].hex, "and stops there, rather than latching on");
    }

    #[test]
    fn flags_compose_on_the_same_file() {
        let t = parse("--hex --readonly core.bin").unwrap();
        assert_eq!(t.len(), 1);
        assert!(t[0].hex && t[0].readonly);
    }

    #[test]
    fn a_trailing_flag_opens_a_fresh_buffer_of_that_kind() {
        // `e --hex` alone: an empty byte buffer.
        let t = parse("--hex").unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].path, None);
        assert!(t[0].hex);
    }

    #[test]
    fn a_double_dash_stops_flag_parsing_for_good() {
        let t = parse("-- --hex").unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].path, Some("--hex".into()), "after --, even a real flag is a filename");
        assert!(!t[0].hex);
    }
}

#[cfg(test)]
mod delete_char_forward_cluster_tests {
    use super::*;

    fn buf(text: &str) -> TextBuffer {
        let mut b = TextBuffer::new_unnamed(10);
        b.insert_text((0, 0), text);
        b.set_cursor(0, 0);
        b
    }

    fn line(b: &TextBuffer, row: usize) -> String {
        b.line_chars(row).into_iter().collect()
    }

    #[test]
    fn x_deletes_a_whole_cluster_in_one_press() {
        // `a`, MAN+ZWJ+WOMAN (one cluster of 3 chars), `b`.
        let mut b = buf("a\u{1F468}\u{200D}\u{1F469}b");
        b.set_cursor(0, 1); // on the cluster's own start
        let mut registers = Registers::new_for_test();
        delete_char_forward(&mut b, &mut registers, None, None);
        assert_eq!(line(&b, 0), "ab", "the whole 3-char cluster should be gone, not just its first codepoint");
    }

    #[test]
    fn x_with_a_count_deletes_that_many_whole_clusters() {
        let mut b = buf("a\u{1F468}\u{200D}\u{1F469}bc");
        b.set_cursor(0, 1);
        let mut registers = Registers::new_for_test();
        delete_char_forward(&mut b, &mut registers, Some(2), None);
        assert_eq!(line(&b, 0), "ac", "should delete the cluster AND 'b' -- two whole units, not two raw codepoints");
    }

    #[test]
    fn deleted_text_written_to_the_register_is_the_whole_cluster() {
        let mut b = buf("\u{1F468}\u{200D}\u{1F469}");
        b.set_cursor(0, 0);
        let mut registers = Registers::new_for_test();
        delete_char_forward(&mut b, &mut registers, None, None);
        let yanked = registers.read(None);
        assert_eq!(yanked.text, "\u{1F468}\u{200D}\u{1F469}");
    }

    #[test]
    fn ordinary_ascii_x_is_unaffected() {
        let mut b = buf("hello");
        b.set_cursor(0, 1);
        let mut registers = Registers::new_for_test();
        delete_char_forward(&mut b, &mut registers, Some(3), None);
        assert_eq!(line(&b, 0), "ho");
    }
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
        // WordForward to line 1 is charwise exclusive; `>` still shifts whole
        // lines (`Op::Indent`).
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

    // Drives one `KeyOutcome` as `run_normal_mode_navigation` does, for a
    // motion or an Insert-mode excursion (`run_insert_mode` with the same
    // `vk`).
    fn apply(buf: &mut TextBuffer, vk: &mut VimKeys, registers: &mut Registers, outcome: KeyOutcome) {
        match outcome {
            KeyOutcome::Motion(m, count) => motion::apply_motion(buf, m, count),
            KeyOutcome::EnterInsert(cmd) => {
                resolve_insert_start(buf, cmd);
                run_insert_mode(buf, vk, rect(), registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();
            }
            other => panic!("unexpected outcome in this test: {other:?}"),
        }
    }

    #[test]
    fn a_block_yank_puts_back_as_a_rectangle() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "aaa111\nbbb222\nccc333\nzz");
        let mut registers = Registers::new_for_test();
        buf.selections =
            vec![crate::bishedit::motion::MotionRange { shape: crate::bishedit::motion::MotionShape::Blockwise, from: (0, 0), to: (2, 2) }];
        let text = crate::bishedit::motion::extract_text(&buf, &buf.selections[0]);
        assert_eq!(text, "aaa\nbbb\nccc");
        registers.record_yank(None, crate::bishedit::registers::RegisterValue { text, shape: RegisterShape::Block });
        buf.selections.clear();

        // Put after the cursor on the short last line: the rectangle goes
        // downward, and a too-short line is padded to the column.
        buf.set_cursor(3, 0);
        put(&mut buf, &mut registers, false, None, None);
        assert_eq!(text_of(&buf), "aaa111\nbbb222\nccc333\nzaaaz\n bbb\n ccc");
    }

    // `cc` empties the line rather than removing it, so `ccZ` on
    // `abc\ndef` gives two lines, as in vim.
    #[test]
    fn change_lines_empties_the_lines_and_leaves_one_behind() {
        let mut registers = Registers::new_for_test();
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "abc\ndef");
        buf.set_cursor(0, 0);
        change_lines(&mut buf, &mut registers, None, None);
        assert_eq!(text_of(&buf), "\ndef");
        assert_eq!(buf.cursor(), (0, 0));

        // A count covers that many lines and still leaves one.
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "a\nb\nc");
        buf.set_cursor(0, 0);
        change_lines(&mut buf, &mut registers, Some(2), None);
        assert_eq!(text_of(&buf), "\nc");

        // The register gets the lines linewise, as from `dd`, so `p` puts a
        // line back.
        let recorded = registers.read(None);
        assert_eq!((recorded.text.as_str(), recorded.shape), ("a\nb\n", RegisterShape::Line));

        // The cursor lands after the rebuilt indent, as with `o` and `O`.
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "    abc\ndef");
        buf.set_cursor(0, 0);
        change_lines(&mut buf, &mut registers, None, None);
        assert_eq!(text_of(&buf), "    \ndef");
        assert_eq!(buf.cursor(), (0, 4));
    }

    // Copied, never guessed: no `smartindent`.
    #[test]
    fn autoindent_copies_the_lines_own_leading_whitespace() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "no indent\n    four spaces\n\tone tab\n   \n");
        assert_eq!(autoindent_for(&buf, 0), "");
        assert_eq!(autoindent_for(&buf, 1), "    ");
        assert_eq!(autoindent_for(&buf, 2), "\t");
        // Whitespace-only gives nothing.
        assert_eq!(autoindent_for(&buf, 3), "");
    }

    #[test]
    fn enter_and_open_line_both_carry_the_indent() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "fn main() {\n    let x = 1;\n}");
        buf.set_cursor(1, 14);
        insert_into(&mut buf, &[Key::Enter, Key::Char('y')]);
        assert_eq!(text_of(&buf), "fn main() {\n    let x = 1;\n    y\n}");

        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "fn main() {\n    let x = 1;\n}");
        buf.set_cursor(1, 0);
        open_line(&mut buf, false);
        assert_eq!(buf.cursor(), (2, 4), "`o` lands past the copied indent, not at column zero");
        insert_into(&mut buf, &[Key::Char('y')]);
        assert_eq!(text_of(&buf), "fn main() {\n    let x = 1;\n    y\n}");

        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "fn main() {\n    let x = 1;\n}");
        buf.set_cursor(1, 0);
        open_line(&mut buf, true);
        insert_into(&mut buf, &[Key::Char('y')]);
        assert_eq!(text_of(&buf), "fn main() {\n    y\n    let x = 1;\n}");
    }

    // The same bytes land differently typed and pasted, and only the paste
    // brackets tell them apart.
    #[test]
    fn a_bracketed_paste_keeps_its_own_indentation() {
        let pasted = |keys: Vec<Key>| {
            let mut buf = TextBuffer::new_unnamed(10);
            buf.insert_text((0, 0), "    start");
            buf.set_cursor(0, 9);
            insert_into(&mut buf, &keys);
            text_of(&buf)
        };
        let body: Vec<Key> = "if a {\n    b();\n}".chars().map(|c| if c == '\n' { Key::Enter } else { Key::Char(c) }).collect();

        let mut bracketed = vec![Key::PasteStart];
        bracketed.extend(body.clone());
        bracketed.push(Key::PasteEnd);
        assert_eq!(pasted(bracketed), "    startif a {\n    b();\n}", "verbatim -- the paste brought its own indent");

        // Typed, every Enter copies the line above, so the closing brace
        // takes `b();`'s indent: the staircase, which is right for typing.
        assert_eq!(pasted(body), "    startif a {\n        b();\n        }", "typed, the same characters get indented as you go");
    }

    #[test]
    fn the_editor_knows_what_a_search_highlight_should_cover() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "alpha beta\ngamma beta");
        let mut vk = VimKeys::new();
        assert_eq!(active_search_pattern(&vk, &buf), None, "nothing searched for yet");

        for key in [Key::Char('/'), Key::Char('b'), Key::Char('e')] {
            vk.feed(key);
        }
        assert_eq!(active_search_pattern(&vk, &buf).as_deref(), Some("be"), "a pattern being typed is shown as it grows");
        vk.feed(Key::Enter);
        assert_eq!(active_search_pattern(&vk, &buf).as_deref(), Some("be"));

        vk.suppress_search_highlight();
        assert_eq!(active_search_pattern(&vk, &buf), None, "`:noh`");

        // A new pattern still shows as it's typed, suppressed or not: it's
        // feedback, not a leftover highlight.
        for key in [Key::Char('/'), Key::Char('g')] {
            vk.feed(key);
        }
        assert_eq!(active_search_pattern(&vk, &buf).as_deref(), Some("g"));

        // Backspace edits the pattern, and the highlight follows it.
        vk.feed(Key::Char('a'));
        assert_eq!(active_search_pattern(&vk, &buf).as_deref(), Some("ga"));
        vk.feed(Key::Backspace);
        assert_eq!(active_search_pattern(&vk, &buf).as_deref(), Some("g"));
    }

    // `insert_with` on a buffer the caller prepared: a selection, a cursor
    // placed.
    fn insert_into(buf: &mut TextBuffer, keys: &[Key]) {
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        vk.start_recording('a');
        for key in keys {
            vk.record_key(*key);
        }
        vk.stop_recording();
        assert!(vk.queue_macro_replay('a', 1));
        run_insert_mode(buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();
    }

    #[test]
    fn insert_mode_honours_home_and_end() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "alpha beta");
        buf.set_cursor(0, 4);
        insert_into(&mut buf, &[Key::End, Key::Char('!'), Key::Home, Key::Char('>')]);
        assert_eq!(text_of(&buf), ">alpha beta!", "End went one past the last char, Home to column zero");
    }

    #[test]
    fn insert_mode_delete_removes_forward_and_joins_lines() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text(
            (0, 0),
            "abc
def",
        );
        buf.set_cursor(0, 1);
        insert_into(&mut buf, &[Key::Delete]);
        assert_eq!(
            text_of(&buf),
            "ac
def",
            "the character *after* the cursor goes"
        );

        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text(
            (0, 0),
            "abc
def",
        );
        buf.set_cursor(0, 3);
        insert_into(&mut buf, &[Key::Delete]);
        assert_eq!(text_of(&buf), "abcdef", "at end of line the newline goes, joining the next");
        assert_eq!(buf.cursor(), (0, 3), "and the cursor does not move");

        // At the very end of the buffer there's nothing to delete.
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "abc");
        buf.set_cursor(0, 3);
        insert_into(&mut buf, &[Key::Delete]);
        assert_eq!(text_of(&buf), "abc");
    }

    #[test]
    fn insert_mode_typing_replaces_a_standing_selection() {
        let select = |buf: &mut TextBuffer| {
            buf.selections =
                vec![crate::bishedit::motion::MotionRange { shape: crate::bishedit::motion::MotionShape::Exclusive, from: (0, 6), to: (0, 10) }];
            buf.set_cursor(0, 10);
        };

        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "alpha beta gamma");
        select(&mut buf);
        insert_into(&mut buf, &[Key::Char('X')]);
        assert_eq!(text_of(&buf), "alpha X gamma");
        assert!(buf.selections.is_empty(), "and the selection is gone, not left standing over new text");

        // Backspace and Delete over a selection, pinned too.
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "alpha beta gamma");
        select(&mut buf);
        insert_into(&mut buf, &[Key::Backspace]);
        assert_eq!(text_of(&buf), "alpha  gamma");

        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "alpha beta gamma");
        select(&mut buf);
        insert_into(&mut buf, &[Key::Delete]);
        assert_eq!(text_of(&buf), "alpha  gamma");
    }

    // An `InsertServices` offering exactly what it was given, to drive
    // Ctrl-N and the accept without a language server.
    struct FixedCompletions(Vec<crate::bishedit::completion::EditorCompletion>);

    impl InsertServices for FixedCompletions {
        fn idle(&mut self, _buf: &mut TextBuffer) -> Option<IdleRedraw> {
            None
        }

        fn complete(&mut self, _buf: &TextBuffer, _row: usize, _col: usize) -> Vec<crate::bishedit::completion::EditorCompletion> {
            self.0.clone()
        }

        fn awaits_live_input(&self) -> bool {
            false
        }
    }

    fn insert_completing(keys: &[Key], items: Vec<crate::bishedit::completion::EditorCompletion>) -> TextBuffer {
        let mut buf = TextBuffer::open(std::path::Path::new("main.rs"), 10).unwrap();
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        let mut services = FixedCompletions(items);
        vk.start_recording('a');
        for key in keys {
            vk.record_key(*key);
        }
        vk.stop_recording();
        assert!(vk.queue_macro_replay('a', 1));
        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut services, false, 24, 80, None, &[], &[]).unwrap();
        buf
    }

    fn completion(label: &str, insert: &str, snippet: bool) -> crate::bishedit::completion::EditorCompletion {
        crate::bishedit::completion::EditorCompletion {
            label: label.to_string(),
            detail: String::new(),
            insert: insert.to_string(),
            replace: None,
            snippet,
        }
    }

    #[test]
    fn a_snippet_completion_splices_in_tentatively() {
        let items = vec![completion("gamma", "gamma(${1:x}, $2)$0", true)];
        let mut keys = chars("ga");
        keys.push(Key::CtrlN);
        keys.push(Key::Enter);
        let buf = insert_completing(&keys, items.clone());
        assert_eq!(text_of(&buf), "gamma(x, $2)", "the tabstops are still holes, not text");
        assert_eq!(buf.snippet_holes.len(), 3);
        assert!(buf.snippet_holes[0].active, "the caret is in the first one");
        assert_eq!(buf.cursor(), (0, 6));

        // Driven to the end it reads as if typed, `$0` included.
        let mut keys = chars("ga");
        keys.push(Key::CtrlN);
        keys.push(Key::Enter);
        keys.extend(chars("1"));
        keys.push(Key::Tab);
        keys.extend(chars("2"));
        keys.push(Key::CtrlY);
        keys.extend(chars(";"));
        let buf = insert_completing(&keys, items);
        assert_eq!(text_of(&buf), "gamma(1, 2);", "`$0` put the caret after the closing paren");
    }

    // A snippet flagged with no tabstops is plain text in snippet notation,
    // so `\$` is a literal dollar.
    #[test]
    fn a_snippet_completion_with_no_tabstops_is_plain_text() {
        let mut keys = chars("co");
        keys.push(Key::CtrlN);
        keys.push(Key::Enter);
        let buf = insert_completing(&keys, vec![completion("cost", "cost \\$5", true)]);
        assert_eq!(text_of(&buf), "cost $5");
        assert!(buf.snippet_holes.is_empty());
        // Unflagged, the same text is literal.
        let buf = insert_completing(&keys, vec![completion("cost", "cost \\$5", false)]);
        assert_eq!(text_of(&buf), "cost \\$5");
    }

    // Insert mode with `abbrs`, fed `keys` through the macro-replay queue,
    // which `vk.next_key` serves before any terminal.
    fn insert_with(path: Option<&str>, keys: &[Key], abbrs: &[Abbr]) -> TextBuffer {
        // A nonexistent path is never written; it's there so `language_of`
        // has an extension.
        let mut buf = match path {
            Some(p) => TextBuffer::open(std::path::Path::new(p), 10).unwrap(),
            None => TextBuffer::new_unnamed(10),
        };
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        vk.start_recording('a');
        for key in keys {
            vk.record_key(*key);
        }
        vk.stop_recording();
        assert!(vk.queue_macro_replay('a', 1));
        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], abbrs).unwrap();
        buf
    }

    fn chars(text: &str) -> Vec<Key> {
        text.chars().map(Key::Char).collect()
    }

    // Ctrl+G leaves Insert mode like Escape, as Ctrl+C and Ctrl+Space do.
    // Ctrl+Space still works where a terminal sends it; macOS and Linux
    // input methods take it first.
    #[test]
    fn ctrl_g_leaves_insert_mode_as_escape_does_and_so_does_ctrl_space() {
        for leave in [Key::CtrlG, Key::CtrlSpace, Key::Escape, Key::CtrlC] {
            let mut keys = chars("hi");
            keys.push(leave);
            keys.extend(chars("x"));
            let buf = insert_with(None, &keys, &[]);
            // Insert mode has ended by the time `x` arrives, or "hix" would
            // be left.
            assert_eq!(text_of(&buf), "hi", "{leave:?} should have left insert mode");
        }
    }

    #[test]
    fn insert_mode_expands_an_abbreviation_whose_language_matches_the_file() {
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("pl", "println!()") }];
        let buf = insert_with(Some("main.rs"), &chars("pl "), &abbrs);
        assert_eq!(text_of(&buf), "println!() ", "expanded, plus the space that triggered it");
    }

    #[test]
    fn insert_mode_ignores_an_abbreviation_for_a_different_language() {
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("pl", "println!()") }];
        let buf = insert_with(Some("notes.md"), &chars("pl "), &abbrs);
        assert_eq!(text_of(&buf), "pl ");
    }

    #[test]
    fn insert_mode_expands_anywhere_in_a_line_not_only_in_command_position() {
        // No command-position gate in a file, unlike the shell prompt.
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("pl", "println!()") }];
        let buf = insert_with(Some("main.rs"), &chars("let x = pl "), &abbrs);
        assert_eq!(text_of(&buf), "let x = println!() ");
    }

    #[test]
    fn insert_mode_drives_a_whole_snippet_with_tab_and_ctrl_y() {
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("f", "fn $1($2) {}") }];
        let mut keys = chars("f ");
        keys.extend(chars("main"));
        keys.push(Key::Tab);
        keys.extend(chars("argc: usize"));
        keys.push(Key::CtrlY);
        let buf = insert_with(Some("main.rs"), &keys, &abbrs);
        assert_eq!(text_of(&buf), "fn main(argc: usize) {}");
        assert!(buf.snippet_holes.is_empty(), "accepting clears what the renderer marks");
    }

    #[test]
    fn insert_mode_snippet_enter_advances_then_accepts_without_a_newline() {
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("f", "fn $1($2) {}") }];
        let mut keys = chars("f ");
        keys.extend(chars("main"));
        keys.push(Key::Enter);
        keys.extend(chars("x: u8"));
        keys.push(Key::Enter);
        let buf = insert_with(Some("main.rs"), &keys, &abbrs);
        assert_eq!(text_of(&buf), "fn main(x: u8) {}", "no newline anywhere -- both Enters belonged to the snippet");
    }

    #[test]
    fn insert_mode_snippet_ctrl_e_cancels_back_to_the_abbreviation_name() {
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("f", "fn $1() {}") }];
        let mut keys = chars("f ");
        keys.extend(chars("main"));
        keys.push(Key::CtrlE);
        let buf = insert_with(Some("main.rs"), &keys, &abbrs);
        assert_eq!(text_of(&buf), "f");
    }

    #[test]
    fn typing_after_an_accept_continues_from_the_end_of_the_snippet() {
        // The snippet moves the real cursor through its model, so the
        // multi-cursor list must be resynced, or the next key lands where the
        // abbreviation started.
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("f", "fn $1()") }];
        let mut keys = chars("f ");
        keys.extend(chars("main"));
        keys.push(Key::CtrlY);
        keys.extend(chars(" {}"));
        let buf = insert_with(Some("main.rs"), &keys, &abbrs);
        assert_eq!(text_of(&buf), "fn main() {}");
    }

    #[test]
    fn replace_mode_does_not_expand_abbreviations() {
        // Replace mode keeps the line's length.
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("pl", "println!()") }];
        let mut buf = TextBuffer::open(std::path::Path::new("main.rs"), 10).unwrap();
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        vk.start_recording('a');
        for key in chars("pl ") {
            vk.record_key(key);
        }
        vk.stop_recording();
        assert!(vk.queue_macro_replay('a', 1));
        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, true, 24, 80, None, &[], &abbrs).unwrap();
        assert_eq!(text_of(&buf), "pl ");
    }

    #[test]
    fn a_live_snippet_marks_its_tabstops_for_the_renderer() {
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("f", "fn $1($2)") }];
        // No accept or cancel: Insert mode ends at EOF with the snippet live,
        // so the marks are as last written.
        let buf = insert_with(Some("main.rs"), &chars("f "), &abbrs);
        assert_eq!(buf.snippet_holes.len(), 2);
        assert!(buf.snippet_holes[0].active && !buf.snippet_holes[1].active);
        assert_eq!((buf.snippet_holes[0].start, buf.snippet_holes[0].end), (3, 5));
    }

    // A multi-line snippet stays multi-line in a file, unlike at the prompt.
    #[test]
    fn a_multi_line_snippet_spans_real_lines_in_a_file() {
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("f", "fn ${1:name}() {\n    $0\n}") }];
        let mut keys = chars("f ");
        keys.extend(chars("main"));
        keys.push(Key::CtrlY);
        keys.extend(chars("todo!()"));
        let buf = insert_with(Some("main.rs"), &keys, &abbrs);
        // `$0` put the caret on the indented middle line, where the typing
        // landed.
        assert_eq!(text_of(&buf), "fn main() {\n    todo!()\n}");
    }

    // Before accepting, the holes are marked on their own lines; the
    // renderer draws per line.
    #[test]
    fn a_multi_line_snippets_holes_are_marked_line_by_line() {
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("f", "fn ${1:name}() {\n    $0\n}") }];
        let buf = insert_with(Some("main.rs"), &chars("f "), &abbrs);
        assert_eq!(text_of(&buf), "fn name() {\n    \n}");
        assert_eq!(buf.snippet_holes.len(), 2);
        assert_eq!((buf.snippet_holes[0].line, buf.snippet_holes[0].start, buf.snippet_holes[0].end), (0, 3, 7));
        assert!(buf.snippet_holes[0].active);
        // `$0` is a position, not a span: an empty hole where the caret
        // will land.
        assert_eq!((buf.snippet_holes[1].line, buf.snippet_holes[1].start, buf.snippet_holes[1].end), (1, 4, 4));
    }

    // A macro spanning an Insert-mode excursion (`A;<Esc>`) and a motion
    // (`j`), recorded with `record_key` and replayed for real: the
    // excursion's reads must come from the replay queue, or each would
    // see EOF after the `A` and never insert the `;`.
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

        // `2@a` replays the sequence twice, landing on "one" then "two", and
        // leaves "three".
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

// `c` on several selections types into every gap, through
// `run_insert_mode`'s `extra_cursors`. Driven directly, bypassing
// repl.rs's `c` handler, with keys replayed from a macro since there's
// no terminal.
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

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[(1, 0), (2, 0)], &[]).unwrap();

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

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[(1, 1)], &[]).unwrap();

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

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[(1, 1)], &[]).unwrap();

        assert_eq!(buf.line_count(), 4);
        assert_eq!(line(&buf, 0), "a");
        assert_eq!(line(&buf, 1), "b");
        assert_eq!(line(&buf, 2), "c");
        assert_eq!(line(&buf, 3), "d");
    }

    #[test]
    fn two_gaps_on_the_same_row_do_not_corrupt_each_others_position() {
        // Gaps at columns 0 and 6 of one row: each X lands at its own gap,
        // not shifted by the first insertion.
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), " place ");
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Char('X'), Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[(0, 6)], &[]).unwrap();

        assert_eq!(line(&buf, 0), "X placeX ");
    }
}

// Leaving Insert mode past a line's last character pulls the cursor
// back onto it: Normal mode can't sit there.
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

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(buf.cursor(), (0, 1), "cursor must land on 'b', not past it");
    }

    #[test]
    fn typing_to_the_end_of_a_line_then_escaping_also_clamps() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Char('h'), Key::Char('i'), Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(buf.cursor(), (0, 1), "cursor must land on the 'i', not past it");
    }

    #[test]
    fn the_caret_mark_still_records_the_true_one_past_end_insert_position() {
        // `gi` resumes exactly where Insert mode left off, unaffected by the
        // clamp.
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "ab");
        buf.set_cursor(0, 2);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(buf.get_mark('^'), Some((0, 2)));
    }

    #[test]
    fn escape_on_a_wholly_empty_line_stays_at_column_zero() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(buf.cursor(), (0, 0));
    }
}

// Ctrl-W in Insert mode deletes the word before the cursor
// (`:help i_CTRL-W`); Normal mode's `<C-w>` is a separate dispatch.
#[cfg(test)]
mod insert_mode_ctrl_w_tests {
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

    fn chars(s: &str) -> Vec<Key> {
        s.chars().map(Key::Char).collect()
    }

    #[test]
    fn deletes_the_word_before_the_cursor() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        let mut keys = chars("hello world");
        keys.push(Key::CtrlW);
        scripted(&mut vk, &keys);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(line(&buf, 0), "hello ");
        assert_eq!(buf.cursor(), (0, 6));
    }

    #[test]
    fn repeated_ctrl_w_keeps_deleting_further_back() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        let mut keys = chars("hello world");
        keys.push(Key::CtrlW);
        keys.push(Key::CtrlW);
        scripted(&mut vk, &keys);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(line(&buf, 0), "", "both words should be gone");
    }

    #[test]
    fn deletes_pre_existing_text_not_just_what_was_typed_this_session() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "existing word");
        buf.set_cursor(0, buf.line_len(0));
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::CtrlW]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(line(&buf, 0), "existing ");
    }

    #[test]
    fn at_column_zero_with_nothing_before_it_is_a_safe_no_op() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::CtrlW, Key::Char('x')]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(line(&buf, 0), "x");
    }
}

// Alt-Left/Alt-Right in Insert mode: `b` and `w`, moving only.
#[cfg(test)]
mod insert_mode_alt_word_motion_tests {
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
    fn leaving_insert_mode_steps_the_cursor_left_as_vim_does() {
        let leave_at = |text: &str, start: usize, typed: &str| {
            let mut buf = TextBuffer::new_unnamed(10);
            buf.insert_text((0, 0), text);
            buf.set_cursor(0, start);
            let mut vk = VimKeys::new();
            let mut registers = Registers::new_for_test();
            let mut keys: Vec<Key> = typed.chars().map(Key::Char).collect();
            keys.push(Key::Escape);
            scripted(&mut vk, &keys);
            run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();
            buf.cursor().1
        };
        // Typed in the middle: lands on the last character typed.
        assert_eq!(leave_at("abcdef", 3, "zz"), 4);
        // Typed at the end: the append position steps back onto the last
        // character.
        assert_eq!(leave_at("abc", 3, "!"), 3);
        // Column 0 can't go further left.
        assert_eq!(leave_at("abcdef", 0, ""), 0);
        // An empty line has nothing to sit on.
        assert_eq!(leave_at("", 0, ""), 0);
    }

    #[test]
    fn alt_left_moves_the_cursor_back_a_word_without_deleting() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "hello world");
        buf.set_cursor(0, buf.line_len(0));
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::AltLeft, Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(buf.line_chars(0).into_iter().collect::<String>(), "hello world", "nothing should be deleted");
        // The motion landed at 6, and leaving Insert mode steps back one,
        // as in vim.
        assert_eq!(buf.cursor(), (0, 5), "cursor should land at the start of 'world' (`b`), less Escape's step back");
    }

    #[test]
    fn alt_right_moves_the_cursor_forward_a_word_without_deleting() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "hello world");
        buf.set_cursor(0, 0);
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::AltRight, Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(buf.line_chars(0).into_iter().collect::<String>(), "hello world", "nothing should be deleted");
        // The motion landed at 6, and leaving Insert mode steps back one,
        // as in vim.
        assert_eq!(buf.cursor(), (0, 5), "cursor should land at the start of 'world' (`w`), less Escape's step back");
    }

    #[test]
    fn alt_left_then_typing_inserts_at_the_new_word_start() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "hello world");
        buf.set_cursor(0, buf.line_len(0));
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::AltLeft, Key::Char('X'), Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(buf.line_chars(0).into_iter().collect::<String>(), "hello Xworld");
    }
}

#[cfg(test)]
mod diagnose_tests {
    use super::*;
    use std::borrow::Cow;

    /// A buffer opened from a real `.bash` temp file, since
    /// `diagnose_buffer`'s language comes from the path. The directory is
    /// returned with it because it owns the file, which must outlive this
    /// call.
    fn temp_bash_buffer(tag: &str, text: &str) -> (crate::tempdir::TempDir, TextBuffer) {
        let dir = crate::tempdir::TempDir::new(&format!("fileeditor-diag-test-{tag}"));
        let path = dir.join("script.bash");
        std::fs::write(&path, text).unwrap();
        (dir, TextBuffer::open(&path, 10).unwrap())
    }

    #[test]
    fn diagnose_buffer_runs_the_bash_linter_against_a_bash_file() {
        let (_dir, buf) = temp_bash_buffer("basic", "echo $foo\n");
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
        let diags = vec![lint::Diagnostic {
            start: 5,
            end: 9,
            severity: lint::Severity::Warning,
            code: Cow::Borrowed("unquoted-expansion"),
            source: None,
            message: String::new(),
            fix: None,
        }];
        let b = TextBuffer::new_unnamed(10);
        let spans = diagnostic_spans_for_line(&b, &diags, 0, 20);
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start, spans[0].end), (5, 9));
        assert!(diagnostic_spans_for_line(&b, &diags, 10, 20).is_empty());
    }

    // One gutter mark per line, however many findings: the worst, which is
    // why `Severity` derives `Ord`.
    #[test]
    fn the_gutter_mark_takes_the_worst_severity_on_the_line() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "one two\nthree");
        let diag = |start, end, severity| lint::Diagnostic {
            start,
            end,
            severity,
            code: Cow::Borrowed("x"),
            source: None,
            message: String::new(),
            fix: None,
        };
        buf.diagnostics = vec![diag(0, 3, lint::Severity::Hint), diag(4, 7, lint::Severity::Error), diag(4, 7, lint::Severity::Warning)];
        let starts = line_starts(&buf);
        assert_eq!(line_severity(&buf, &starts, 0), Some(lint::Severity::Error));
        // Nothing touches the second line.
        assert_eq!(line_severity(&buf, &starts, 1), None);
        // The mark is drawn in the severity's colour.
        let cell = render_diagnostic_cell(&buf, &starts, 0, 2).unwrap();
        let (fg, _) = crate::theme::resolve(crate::theme::Ui::Error, None);
        assert!(cell.starts_with(&vt100::sgr_codes(fg, vt100::Color::Default, vt100::CellAttrs::default())), "{cell:?}");
        assert!(render_diagnostic_cell(&buf, &starts, 1, 2).is_none());
    }

    #[test]
    fn a_real_edit_clears_previously_computed_diagnostics() {
        let (_dir, mut buf) = temp_bash_buffer("clears", "echo $foo\n");
        buf.diagnostics = diagnose_buffer(&buf);
        assert!(!buf.diagnostics.is_empty());
        buf.insert_text((0, 0), "x");
        assert!(buf.diagnostics.is_empty());
    }

    // Applied last first: an earlier edit shifting the text would
    // invalidate later ranges.
    #[test]
    fn a_batch_of_edits_applies_without_invalidating_its_own_ranges() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "one two three");
        // In ascending order, as a server sends them.
        let edits = vec![
            lsp::TextEdit { start: (0, 0), end: (0, 3), text: "ONE".to_string() },
            lsp::TextEdit { start: (0, 4), end: (0, 7), text: "TWO!!".to_string() },
            lsp::TextEdit { start: (0, 8), end: (0, 13), text: "3".to_string() },
        ];
        assert_eq!(apply_text_edits(&mut buf, &edits, lsp::PositionEncoding::Utf32), 3);
        assert_eq!(buffer_text(&buf), "ONE TWO!! 3");
    }

    #[test]
    fn an_edit_may_span_lines_and_may_insert_them() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "a\nb\nc");
        // Mid-line 0 to mid-line 2 replaced with two lines, as a formatter
        // sends.
        let edits = vec![lsp::TextEdit { start: (0, 1), end: (2, 1), text: "X\nY".to_string() }];
        assert_eq!(apply_text_edits(&mut buf, &edits, lsp::PositionEncoding::Utf32), 1);
        assert_eq!(buffer_text(&buf), "aX\nY");
    }

    #[test]
    fn a_pure_insertion_and_a_pure_deletion_both_work() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "ac");
        // Empty range, text: an insertion.
        assert_eq!(
            apply_text_edits(&mut buf, &[lsp::TextEdit { start: (0, 1), end: (0, 1), text: "b".to_string() }], lsp::PositionEncoding::Utf32),
            1
        );
        assert_eq!(buffer_text(&buf), "abc");
        // A range, no text: a deletion.
        assert_eq!(apply_text_edits(&mut buf, &[lsp::TextEdit { start: (0, 1), end: (0, 2), text: String::new() }], lsp::PositionEncoding::Utf32), 1);
        assert_eq!(buffer_text(&buf), "ac");
        // Neither: nothing, counted as nothing, so a formatter that changed
        // nothing can say so.
        assert_eq!(apply_text_edits(&mut buf, &[lsp::TextEdit { start: (0, 1), end: (0, 1), text: String::new() }], lsp::PositionEncoding::Utf32), 0);
        assert_eq!(buffer_text(&buf), "ac");
    }

    #[test]
    fn a_server_position_becomes_a_flat_offset_and_back_again() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "abc\ndefgh\nij");
        let starts = line_starts(&buf);
        let utf32 = lsp::PositionEncoding::Utf32;
        // The inverse of `diagnostic_position`, checked against it.
        for offset in [0usize, 3, 4, 8, 9, 11] {
            let (line, col) = diagnostic_position(&buf, offset);
            assert_eq!(diagnostic_offset(&buf, &starts, line, col, utf32), offset, "offset {offset}");
        }
        // Past the end clamps to the end rather than dropping the finding.
        assert_eq!(diagnostic_offset(&buf, &starts, 99, 0, utf32), 12);
        assert_eq!(diagnostic_offset(&buf, &starts, 1, 99, utf32), starts[1] + 5);
    }

    #[test]
    fn a_utf16_column_lands_on_the_right_character() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "a\u{1f30d}bc");
        let starts = line_starts(&buf);
        // The emoji is one char to bish and two UTF-16 units, so `b` is
        // char 2 but UTF-16 column 3.
        assert_eq!(diagnostic_offset(&buf, &starts, 0, 3, lsp::PositionEncoding::Utf16), 2);
        assert_eq!(diagnostic_offset(&buf, &starts, 0, 2, lsp::PositionEncoding::Utf32), 2);
    }

    #[test]
    fn a_servers_findings_become_diagnostics_this_editor_can_draw() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "let x = 1\nlet y = 2");
        let findings = vec![
            lsp::Finding {
                start: (1, 4),
                end: (1, 5),
                severity: 1,
                code: "E0308".to_string(),
                source: Some("rustc".to_string()),
                message: "mismatched types".to_string(),
            },
            lsp::Finding { start: (0, 0), end: (0, 3), severity: 4, code: String::new(), source: None, message: "unused".to_string() },
        ];
        let diagnostics = diagnostics_from_server(&buf, &findings, lsp::PositionEncoding::Utf32);
        assert_eq!(diagnostics[0].start, 14, "line 1 starts at 10, plus 4 columns");
        assert_eq!(diagnostics[0].end, 15);
        assert_eq!(diagnostics[0].severity, lint::Severity::Error);
        assert_eq!(diagnostics[0].label(), "rustc:E0308");
        assert_eq!(diagnostics[1].severity, lint::Severity::Hint);
        // Always sourced, so a relayed finding can be told from bish's own.
        assert_eq!(diagnostics[1].source.as_deref(), Some("lsp"));
        // A server's diagnostic has no edit: a fix is a separate code-action
        // request.
        assert!(diagnostics.iter().all(|d| d.fix.is_none()));
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
        // `$foo` is at offsets 5..9.
        let diagnostic = lint::Diagnostic {
            start: 5,
            end: 9,
            severity: lint::Severity::Warning,
            code: Cow::Borrowed("unquoted-expansion"),
            source: None,
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
        let diagnostic = lint::Diagnostic {
            start: 5,
            end: 9,
            severity: lint::Severity::Warning,
            code: Cow::Borrowed("unquoted-expansion"),
            source: None,
            message: String::new(),
            fix: None,
        };
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
    fn scroll_horizontally_moves_the_view_and_stops_at_the_widest_visible_line() {
        let mut b = buf(&format!("{}\nshort", "x".repeat(40)));
        scroll_horizontally(&mut b, 6, 10);
        assert_eq!(b.viewport_left(), 6);
        scroll_horizontally(&mut b, 6, 10);
        assert_eq!(b.viewport_left(), 12);
        // 40 columns in a 10-column pane: 30 is as far as there's anything to
        // see.
        for _ in 0..20 {
            scroll_horizontally(&mut b, 6, 10);
        }
        assert_eq!(b.viewport_left(), 30);
    }

    #[test]
    fn scroll_horizontally_stops_at_zero_going_back() {
        let mut b = buf(&format!("{}\n", "x".repeat(40)));
        scroll_horizontally(&mut b, 12, 10);
        scroll_horizontally(&mut b, -6, 10);
        assert_eq!(b.viewport_left(), 6);
        scroll_horizontally(&mut b, -30, 10);
        assert_eq!(b.viewport_left(), 0);
    }

    // The cursor comes to the view, not the view to the cursor.
    #[test]
    fn scroll_horizontally_brings_the_cursor_along() {
        let mut b = buf(&format!("{}\n", "x".repeat(40)));
        b.set_cursor(0, 0);
        scroll_horizontally(&mut b, 12, 10);
        assert_eq!(b.viewport_left(), 12);
        assert_eq!(b.cursor(), (0, 12), "the cursor was dragged to the left edge of the new view");
        scroll_horizontally(&mut b, -6, 10);
        assert_eq!(b.cursor(), (0, 12), "still in view, so left where it was");
    }

    #[test]
    fn scroll_horizontally_does_nothing_while_wrapping() {
        let mut b = buf(&format!("{}\n", "x".repeat(40)));
        b.wrap.wrap = true;
        scroll_horizontally(&mut b, 12, 10);
        assert_eq!(b.viewport_left(), 0);
    }

    #[test]
    fn scroll_horizontally_does_nothing_when_every_line_fits() {
        let mut b = buf("short\nalso short\n");
        scroll_horizontally(&mut b, 12, 40);
        assert_eq!(b.viewport_left(), 0);
        assert_eq!(b.cursor(), (0, 0));
    }

    fn tall(lines: usize, height: usize) -> TextBuffer {
        let text: String = (1..=lines).map(|i| format!("line {i}\n")).collect();
        let mut b = TextBuffer::new_unnamed(height);
        b.insert_text((0, 0), &text);
        b.set_cursor(0, 0);
        b
    }

    fn drawn(buf: &TextBuffer, line: usize) -> String {
        display_row(buf, line, tabular_layout(buf).as_ref()).cells.iter().collect()
    }

    #[test]
    fn the_cursor_shape_says_which_mode_you_are_in() {
        let b = buf("x\n");
        let vk = VimKeys::new();
        assert_eq!(cursor_shape(&b, EditorMode::Normal, &vk), "\x1b[2 q", "block");
        assert_eq!(cursor_shape(&b, EditorMode::Insert, &vk), "\x1b[6 q", "bar");
        assert_eq!(cursor_shape(&b, EditorMode::Replace, &vk), "\x1b[4 q", "underline");
    }

    #[test]
    fn cursorshape_off_emits_nothing_at_all() {
        let mut b = buf("x\n");
        b.cursorshape = false;
        let vk = VimKeys::new();
        for mode in [EditorMode::Normal, EditorMode::Insert, EditorMode::Replace] {
            assert_eq!(cursor_shape(&b, mode, &vk), "");
        }
    }

    // Carets sit between characters, so from before `a` to before `c` is
    // `ab`.
    #[test]
    fn an_insert_mode_selection_covers_what_is_between_the_carets() {
        let b = buf("abcdef\n");
        let range = selection_between(&b, (0, 0), (0, 2)).unwrap();
        assert_eq!(range.from, (0, 0));
        assert_eq!(range.to, (0, 1), "`ab`, not `abc`");
        assert_eq!(range.shape, motion::MotionShape::Inclusive, "the same shape Visual mode produces");
    }

    #[test]
    fn an_insert_mode_selection_works_either_way_round() {
        let b = buf("abcdef\n");
        assert_eq!(selection_between(&b, (0, 4), (0, 1)), selection_between(&b, (0, 1), (0, 4)));
    }

    // A caret with nothing swept is a cursor, or a click would leave a
    // selection for Backspace.
    #[test]
    fn a_caret_that_has_not_moved_is_not_a_selection() {
        let b = buf("abcdef\n");
        assert_eq!(selection_between(&b, (0, 3), (0, 3)), None);
    }

    #[test]
    fn a_selection_ending_at_a_line_start_ends_on_the_previous_line() {
        let b = buf("abc\ndef\n");
        let range = selection_between(&b, (0, 1), (1, 0)).unwrap();
        assert_eq!(range.from, (0, 1));
        assert_eq!(range.to, (0, 3));
    }

    #[test]
    fn a_tab_draws_to_the_next_tabstop() {
        let mut b = buf("\tone\n");
        b.tabstop = 4;
        assert_eq!(drawn(&b, 0), "    one");
        b.tabstop = 8;
        assert_eq!(drawn(&b, 0), "        one");
    }

    // To the next stop, not a fixed width.
    #[test]
    fn a_tab_fills_only_what_is_left_of_its_stop() {
        let mut b = buf("ab\tc\n");
        b.tabstop = 4;
        assert_eq!(drawn(&b, 0), "ab  c", "two columns left of the stop");
        let mut b = buf("abc\tx\n");
        b.tabstop = 4;
        assert_eq!(drawn(&b, 0), "abc x", "one");
    }

    // Each space of a tab maps back to the tab.
    #[test]
    fn every_column_of_a_tab_maps_back_to_the_tab() {
        let mut b = buf("\tx\n");
        b.tabstop = 4;
        let row = display_row(&b, 0, tabular_layout(&b).as_ref());
        assert_eq!(row.source_at[..4], [Some(0), Some(0), Some(0), Some(0)]);
        assert_eq!(row.source_at[4], Some(1));
        assert_eq!(row.cell_of[0], 0, "the tab starts at the first column");
        assert_eq!(row.cell_of[1], 4, "and `x` starts after all of it");
    }

    #[test]
    fn display_column_counts_what_is_drawn() {
        let mut b = buf("\t\tx\n");
        b.tabstop = 4;
        assert_eq!(display_column(&b, 0, 0), 0);
        assert_eq!(display_column(&b, 0, 1), 4);
        assert_eq!(display_column(&b, 0, 2), 8);
    }

    #[test]
    fn expandtab_off_indents_with_a_tab() {
        let mut b = buf("one\n");
        b.expandtab = false;
        indent_rows(&mut b, 0, 0);
        assert_eq!(buffer_text(&b), "\tone\n");
        outdent_rows(&mut b, 0, 0);
        assert_eq!(buffer_text(&b), "one\n");
    }

    #[test]
    fn shiftwidth_decides_how_far_an_indent_goes() {
        let mut b = buf("one\n");
        b.shiftwidth = 2;
        indent_rows(&mut b, 0, 0);
        assert_eq!(buffer_text(&b), "  one\n");
    }

    // One tab is a whole indent, as in vim.
    #[test]
    fn outdent_takes_one_indents_worth_of_columns_not_characters() {
        let mut b = buf("\t\tone\n");
        b.tabstop = 4;
        b.shiftwidth = 4;
        outdent_rows(&mut b, 0, 0);
        assert_eq!(buffer_text(&b), "\tone\n");
    }

    #[test]
    fn scrolloff_keeps_lines_visible_below_the_cursor() {
        let mut b = tall(100, 10);
        b.wrap.scrolloff = 3;
        b.set_cursor(20, 0);
        scroll_to_show_cursor(&mut b, 40);
        // The cursor sits three rows above the bottom.
        assert_eq!(b.viewport_top(), 20 + 3 + 1 - 10);
    }

    #[test]
    fn scrolloff_keeps_lines_visible_above_the_cursor() {
        let mut b = tall(100, 10);
        b.wrap.scrolloff = 3;
        b.set_viewport_top(40);
        b.set_cursor(41, 0);
        scroll_to_show_cursor(&mut b, 40);
        assert_eq!(b.viewport_top(), 38, "three lines kept above it");
    }

    // At the ends the margin gives way, or the first line could never be
    // at the top.
    #[test]
    fn scrolloff_gives_way_at_the_ends_of_the_buffer() {
        let mut b = tall(100, 10);
        b.wrap.scrolloff = 3;
        b.set_cursor(0, 0);
        scroll_to_show_cursor(&mut b, 40);
        assert_eq!(b.viewport_top(), 0);
        let last = b.line_count() - 1;
        b.set_cursor(last, 0);
        scroll_to_show_cursor(&mut b, 40);
        assert_eq!(b.viewport_top(), last + 1 - 10, "the last line is on the last row");
    }

    #[test]
    fn a_scrolloff_wider_than_the_pane_is_capped() {
        let mut b = tall(100, 10);
        b.wrap.scrolloff = 200;
        b.set_cursor(50, 0);
        scroll_to_show_cursor(&mut b, 40);
        let top = b.viewport_top();
        assert!(top <= 50 && 50 < top + 10, "the cursor is still on screen: top={top}");
    }

    #[test]
    fn scrolloff_off_is_exactly_what_it_always_was() {
        let mut b = tall(100, 10);
        b.set_cursor(20, 0);
        scroll_to_show_cursor(&mut b, 40);
        assert_eq!(b.viewport_top(), 11, "the cursor on the last row");
    }

    #[test]
    fn relativenumber_numbers_by_distance_and_keeps_the_cursors_own() {
        let mut b = tall(10, 10);
        b.set_cursor(4, 0);
        assert_eq!(line_number_text(&b, 0), "1", "absolute while it is off");
        b.relativenumber = true;
        assert_eq!(line_number_text(&b, 4), "5", "the cursor's own line keeps its number");
        assert_eq!(line_number_text(&b, 3), "1");
        assert_eq!(line_number_text(&b, 5), "1");
        assert_eq!(line_number_text(&b, 0), "4");
        assert_eq!(line_number_text(&b, 9), "5");
    }

    #[test]
    fn the_gutter_keeps_its_width_under_relativenumber() {
        let mut b = tall(100, 10);
        let absolute = line_number_width(&b);
        b.relativenumber = true;
        assert_eq!(line_number_width(&b), absolute);
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
        // The cursor (column 20) is the last visible column: scroll just
        // enough, as vim does.
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
        // Scrolled 10 right in a 10-wide window: 15..25 becomes 5..10,
        // clamped at the edge.
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

    // A buffer with a path ending in `ext`, which `language_of` reads; no
    // file needed. `text` has no trailing newline, as an opened file's
    // lines don't (`TextBuffer::save` adds one), so the test sees what
    // `strip_buffer_implicit_trailing_newline` handles.
    fn buf_with_ext(text: &str, ext: &str) -> TextBuffer {
        let path = format!("/tmp/bish-fileeditor-pre-save-hook-test.{ext}");
        let mut buf = TextBuffer::open(std::path::Path::new(&path), 10).unwrap();
        buf.insert_text((0, 0), text);
        buf.set_cursor(0, 0);
        buf
    }

    // The highlighter's scanners count bytes and this pass counts
    // characters, so one non-ASCII character would shift every span after
    // it; hence the assertion is on a `let` below the ellipsis.
    #[test]
    fn a_wide_character_does_not_shift_the_highlighting_under_it() {
        let text = "let a = '\u{2026}';\nlet b = 1;\n";
        let buf = buf_with_ext(text, "rs");
        let chars: Vec<char> = text.chars().collect();
        let spans = buffer_highlight_spans(&buf, None);
        let painted: Vec<String> = spans.iter().map(|s| chars[s.start..s.end].iter().collect()).collect();
        assert!(painted.iter().all(|p| p == "let" || p == "'\u{2026}'" || p == "1"), "{painted:?}");
        assert_eq!(painted.iter().filter(|p| *p == "let").count(), 2, "both `let`s, and the second one is the point: {painted:?}");
    }

    fn named(path: &str) -> TextBuffer {
        TextBuffer::from_text(std::path::Path::new(path), "x\n", 10)
    }

    // Checked against `vim -u NONE -i NONE -N`: `%:r` keeps `sub/a.b`, `%:h`
    // of a bare name is `.`, `.hidden` has no extension.
    #[test]
    fn percent_is_the_file_with_vims_own_modifiers() {
        let buf = named("sub/a.b.c");
        let expand = |line: &str| expand_filename_macros(line, &buf).unwrap();
        assert_eq!(expand("echo %"), "echo sub/a.b.c");
        assert_eq!(expand("echo %:t"), "echo a.b.c");
        assert_eq!(expand("echo %:r"), "echo sub/a.b");
        assert_eq!(expand("echo %:e"), "echo c");
        assert_eq!(expand("echo %:h"), "echo sub");
        // Left to right, each modifier on the last one's result.
        assert_eq!(expand("echo %:t:r"), "echo a.b");
        // Twice in one line, and not only at a word boundary.
        assert_eq!(expand("cp % %.bak"), "cp sub/a.b.c sub/a.b.c.bak");

        let bare = named("noext");
        assert_eq!(expand_filename_macros("echo %:h", &bare).unwrap(), "echo .");
        assert_eq!(expand_filename_macros("echo %:r", &bare).unwrap(), "echo noext");

        let hidden = named(".hidden");
        assert_eq!(expand_filename_macros("echo %:r", &hidden).unwrap(), "echo .hidden");
    }

    // `\%` is a literal `%`, the only escape touched; other backslashes are
    // the shell's.
    #[test]
    fn a_backslash_makes_a_percent_literal() {
        let buf = named("f.txt");
        assert_eq!(expand_filename_macros("echo \\%", &buf).unwrap(), "echo %");
        assert_eq!(expand_filename_macros("printf '\\%s' %", &buf).unwrap(), "printf '%s' f.txt");
        // Not ours: reaches the shell intact.
        assert_eq!(expand_filename_macros("echo \\n %", &buf).unwrap(), "echo \\n f.txt");
    }

    // Refused rather than run with a hole where the name goes, as in vim.
    #[test]
    fn an_empty_expansion_is_an_error_not_an_empty_string() {
        assert!(expand_filename_macros("echo %", &TextBuffer::new_unnamed(10)).is_err());
        // `:e` of a name without extension is empty, which vim treats as no
        // name.
        assert!(expand_filename_macros("echo %:e", &named("noext")).is_err());
        assert!(expand_filename_macros("echo %:e", &named(".hidden")).is_err());
    }

    // `#` stays as typed, as does a colon that isn't a modifier.
    #[test]
    fn what_is_not_a_macro_is_left_alone() {
        let buf = named("f.txt");
        let expand = |line: &str| expand_filename_macros(line, &buf).unwrap();
        assert_eq!(expand("echo % # a comment"), "echo f.txt # a comment");
        assert_eq!(expand("echo %:2"), "echo f.txt:2");
        assert_eq!(expand("echo %:"), "echo f.txt:");
        assert_eq!(expand("grep -n x %:t:9"), "grep -n x f.txt:9");
    }

    // The dispatch: the editor asks the right highlighter, and a language
    // with none renders plain rather than as bash. The highlighters have
    // their own tests.
    #[test]
    fn buffer_highlighting_follows_the_buffers_own_language() {
        let json = buf_with_ext(r#"{"a": 1}"#, "json");
        assert!(!buffer_highlight_spans(&json, None).is_empty(), "a .json buffer is highlighted");

        let bash = buf_with_ext("if true; then echo hi; fi", "bash");
        assert!(!buffer_highlight_spans(&bash, None).is_empty(), "a .bash buffer still is");

        let markdown = buf_with_ext("# Title\n\n*emphasis*\n", "md");
        assert!(!buffer_highlight_spans(&markdown, None).is_empty(), "and a .md buffer");

        // A man section number is an extension like any other.
        let roff = buf_with_ext(".SH NAME\nls \\- list directory contents\n", "1");
        assert!(!buffer_highlight_spans(&roff, None).is_empty(), "and a .1 buffer");
        assert_eq!(language_of(&roff), "roff");

        let toml = buf_with_ext("[package]\nname = \"bish\"\n", "toml");
        assert!(!buffer_highlight_spans(&toml, None).is_empty(), "and a .toml buffer");

        // Valid bash, but a `.zig` file isn't claimed by anything, so it
        // stays plain.
        let unclaimed = buf_with_ext("if true; then echo hi; fi", "zig");
        assert!(buffer_highlight_spans(&unclaimed, None).is_empty(), "a language with no highlighter renders plain");
    }

    // A zip member and a gzip'd file both open read-only, named by the path
    // asked for.
    #[test]
    fn compressed_paths_open_as_readonly_buffers_holding_the_decompressed_text() {
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-archive-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let zip = dir.join("sample.zip");
        std::fs::write(&zip, include_bytes!("testdata/sample.zip")).unwrap();
        let gz = dir.join("sample.txt.gz");
        std::fs::write(&gz, include_bytes!("testdata/sample.txt.gz")).unwrap();

        let member = crate::archive::join(&zip, "dir/inner.json");
        let session = EditSession::open(Some(&member), 10).unwrap();
        assert_eq!(buffer_text(&session.buffer), "{\"a\": 1}");
        assert!(session.buffer.is_readonly(), "a zip member can't be written back");
        assert_eq!(session.buffer.path(), Some(std::path::Path::new(&member)));

        let path = gz.display().to_string();
        let session = EditSession::open(Some(&path), 10).unwrap();
        assert_eq!(buffer_text(&session.buffer), "compressed text\nsecond line");
        assert!(session.buffer.is_readonly(), "a gzip file can't be written back");

        // An ordinary file beside them is untouched.
        let plain = dir.join("plain.txt");
        std::fs::write(&plain, "ordinary\n").unwrap();
        let session = EditSession::open(Some(&plain.display().to_string()), 10).unwrap();
        assert!(!session.buffer.is_readonly());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_member_is_an_error_rather_than_an_empty_new_buffer() {
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-archive-missing-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let zip = dir.join("sample.zip");
        std::fs::write(&zip, include_bytes!("testdata/sample.zip")).unwrap();

        // A nonexistent path opens as a new file (vim's `:e newfile`); a
        // nonexistent member can't, since it can't be created.
        let opened = EditSession::open(Some(&crate::archive::join(&zip, "nope.txt")), 10);
        let err = match opened {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a nonexistent member must not open as a blank buffer"),
        };
        assert!(err.contains("no such member"), "{err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // The wrap options are on the buffer (`TextBuffer::wrap`), so a test
    // sets them and checks the layout without a terminal.
    fn wrapped_buf(text: &str, wrap: crate::bishedit::wrap::Options) -> TextBuffer {
        wrapped_buf_h(text, wrap, 10)
    }

    // The viewport height is fixed at construction, so scrolling tests give
    // it.
    fn wrapped_buf_h(text: &str, wrap: crate::bishedit::wrap::Options, height: usize) -> TextBuffer {
        let mut buf = TextBuffer::new_unnamed(height);
        buf.insert_text((0, 0), text);
        buf.set_cursor(0, 0);
        buf.wrap = wrap;
        buf
    }

    fn wrap_on() -> crate::bishedit::wrap::Options {
        crate::bishedit::wrap::Options { wrap: true, showbreak: String::new(), breakindent: false, linebreak: false, ..Default::default() }
    }

    #[test]
    fn visible_rows_break_a_long_line_across_screen_rows() {
        let buf = wrapped_buf("abcdefghij\nok", wrap_on());
        let rows = visible_rows(&buf, 4, 10);
        let shape: Vec<(usize, bool, usize, usize)> = rows.iter().map(|r| (r.line, r.first, r.seg.start, r.seg.end)).collect();
        assert_eq!(
            shape,
            vec![(0, true, 0, 4), (0, false, 4, 8), (0, false, 8, 10), (1, true, 0, 2)],
            "three rows for the long line, one for the line that fits"
        );
    }

    #[test]
    fn without_wrapping_every_line_is_exactly_one_row() {
        let buf = wrapped_buf("abcdefghij\nok", crate::bishedit::wrap::Options::default());
        let rows = visible_rows(&buf, 4, 10);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.first), "each row opens its own line");
    }

    // The frame and its click inverse must agree.
    #[test]
    fn a_click_on_a_wrapped_row_finds_the_character_under_it() {
        let buf = wrapped_buf("abcdefghij", wrap_on());
        // 4 columns wide after the gutter.
        let gutter = total_gutter_width(&buf);
        let rect = Rect { row: 0, col: 0, rows: 12, cols: gutter + 4 };
        // Row 0 col 0 is `a`, row 1 col 0 is `e` (the second segment starts
        // at char 4), row 2 col 1 is `j`.
        assert_eq!(position_at_screen(&buf, rect, 0, gutter), Some((0, 0)));
        assert_eq!(position_at_screen(&buf, rect, 1, gutter), Some((0, 4)));
        assert_eq!(position_at_screen(&buf, rect, 2, gutter + 1), Some((0, 9)));
    }

    // A line taller than the pane scrolls within itself, or a minified file's
    // cursor could be out of reach.
    #[test]
    fn scrolling_can_move_within_a_line_taller_than_the_pane() {
        let mut buf = wrapped_buf_h(&"x".repeat(40), wrap_on(), 3);
        // 4 columns wide: 10 rows for this line, in a 3-row pane.
        buf.set_cursor(0, 39);
        scroll_to_show_cursor(&mut buf, 4);
        assert_eq!(buf.viewport_top(), 0, "still the only line there is");
        assert_eq!(buf.viewport_sub(), 7, "but scrolled seven rows into it");
        let rows = visible_rows(&buf, 4, 3);
        assert!(rows.iter().any(|r| r.seg.contains(39)), "the cursor's own row is on screen");

        // Back to the line's top.
        buf.set_cursor(0, 0);
        scroll_to_show_cursor(&mut buf, 4);
        assert_eq!(buf.viewport_sub(), 0);
    }

    #[test]
    fn wrapping_never_scrolls_horizontally() {
        let mut buf = wrapped_buf(&"x".repeat(40), wrap_on());
        buf.set_viewport_left(20);
        buf.set_cursor(0, 39);
        scroll_to_show_cursor(&mut buf, 4);
        assert_eq!(buf.viewport_left(), 0, "a wrapped line has no off-screen right edge");
    }

    // `sidescrolloff` only applies without wrapping.
    #[test]
    fn sidescrolloff_keeps_columns_visible_either_side_of_the_cursor() {
        let mut buf = wrapped_buf(&"x".repeat(80), crate::bishedit::wrap::Options { sidescrolloff: 5, ..Default::default() });
        buf.set_cursor(0, 40);
        scroll_to_show_cursor(&mut buf, 20);
        // The cursor sits 5 columns short of the right edge:
        // `viewport_left + 20 - 1 - 5 == 40`.
        assert_eq!(buf.viewport_left(), 26);

        buf.set_cursor(0, 26);
        scroll_to_show_cursor(&mut buf, 20);
        assert_eq!(buf.viewport_left(), 21, "and five short of the left edge too");
    }

    #[test]
    fn a_wrapped_frame_puts_the_cursor_on_its_own_row() {
        let mut buf = wrapped_buf_h("abcdefghij", wrap_on(), 6);
        buf.set_cursor(0, 9);
        let gutter = total_gutter_width(&buf);
        let rect = Rect { row: 0, col: 0, rows: 8, cols: gutter + 4 };
        let frame = build_editor_frame(&buf, &VimKeys::new(), EditorMode::Normal, rect, 0, 0, None);
        // Char 9 is on the third segment (8..10), its second column.
        assert!(frame.contains(&format!("\x1b[3;{}H", gutter + 2)), "expected the cursor on row 3 column {}, got:\n{frame:?}", gutter + 2);
    }

    // The language server's same-symbol marks compose over the colour
    // rather than replacing it, unlike the search and selection layers.
    #[test]
    fn document_highlights_underline_without_taking_the_colour_off() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "alpha beta alpha");
        buf.set_cursor(0, 0);
        let rect = Rect { row: 0, col: 0, rows: 4, cols: 40 };
        let plain = build_editor_frame(&buf, &VimKeys::new(), EditorMode::Normal, rect, 0, 0, None);
        assert!(!plain.contains("\x1b[4m") && !plain.contains(";4m"), "nothing underlined yet:\n{plain:?}");

        buf.document_highlights = vec![
            highlight::StyledSpan {
                start: 0,
                end: 5,
                fg: vt100::Color::Default,
                attrs: vt100::CellAttrs { underline: true, ..vt100::CellAttrs::default() },
            },
            highlight::StyledSpan {
                start: 11,
                end: 16,
                fg: vt100::Color::Default,
                attrs: vt100::CellAttrs { underline: true, bold: true, ..vt100::CellAttrs::default() },
            },
        ];
        let marked = build_editor_frame(&buf, &VimKeys::new(), EditorMode::Normal, rect, 0, 0, None);
        // A read is underlined; a write underlined and bold.
        assert!(marked.contains("\x1b[0;4malpha"), "expected an underlined read:\n{marked:?}");
        assert!(marked.contains("\x1b[0;1;4malpha"), "expected a bold underlined write:\n{marked:?}");
        // The word between them is unmarked.
        assert!(marked.contains("\x1b[0m beta "), "{marked:?}");
        // And the marks go when the buffer's do.
        buf.document_highlights.clear();
        let cleared = build_editor_frame(&buf, &VimKeys::new(), EditorMode::Normal, rect, 0, 0, None);
        assert_eq!(cleared, plain, "clearing the marks restores exactly the unmarked frame");
    }

    // What `%` would jump to, and the bracket it would jump from.
    #[test]
    fn the_bracket_under_the_cursor_and_the_one_it_pairs_with_are_both_marked() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "f(a, g(b))");
        let rect = Rect { row: 0, col: 0, rows: 4, cols: 40 };
        let frame = |buf: &TextBuffer, mode| build_editor_frame(buf, &VimKeys::new(), mode, rect, 0, 0, None);

        // On `f`, not a bracket: nothing, though `%` would jump to the last
        // `)`.
        buf.set_cursor(0, 0);
        let plain = frame(&buf, EditorMode::Normal);
        assert!(!plain.contains(";7m"), "nothing marked yet:\n{plain:?}");

        // On the outer `(`: it and its `)`, not the inner pair.
        buf.set_cursor(0, 1);
        let marked = frame(&buf, EditorMode::Normal);
        assert!(marked.contains("\x1b[0;7m(\x1b[0ma, g(b)"), "the opening bracket marked:\n{marked:?}");
        assert!(marked.contains("\x1b[0;7m)\x1b[0m"), "the closing bracket marked:\n{marked:?}");
        assert_eq!(marked.matches("\x1b[0;7m").count(), 2, "one mark each, and no more:\n{marked:?}");

        // In Insert mode the bracket behind the cursor counts.
        buf.set_cursor(0, 2);
        assert_eq!(frame(&buf, EditorMode::Insert).matches("\x1b[0;7m").count(), 2, "the pair behind the cursor");
        assert!(!frame(&buf, EditorMode::Normal).contains(";7m"), "and in Normal mode it does not");

        // `bishopt --set matchbrackets off`: nothing.
        buf.set_cursor(0, 1);
        buf.matchbrackets = false;
        assert!(!frame(&buf, EditorMode::Normal).contains(";7m"), "nothing marked with the option off");
        assert!(!frame(&buf, EditorMode::Insert).contains(";7m"), "in Insert mode either");
    }

    // A spliced hint keeps every promise of `Row`, which the cursor,
    // clicks, scrolling, selections and spans read the line through.
    #[test]
    fn an_inlay_hint_is_drawn_between_characters_without_moving_any_of_them() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "let x = 1");
        buf.set_cursor(0, 0);
        let plain = display_row(&buf, 0, None);
        assert_eq!(plain.cells.iter().collect::<String>(), "let x = 1");

        // `: u32` after `x`, character 5.
        buf.inlay_hints = vec![(0, 5, ": u32".to_string())];
        let row = display_row(&buf, 0, None);
        assert_eq!(row.cells.iter().collect::<String>(), "let x: u32 = 1");
        // The hint's cells belong to no character, so they're dimmed and the
        // cursor steps over them in one press.
        assert_eq!(row.source_at[5..10], [None, None, None, None, None]);
        // Every real character maps to itself, both ways.
        for c in 0..buf.line_len(0) {
            assert_eq!(row.source_at[row.cell_of[c]], Some(c), "char {c}");
        }
        // The position after the last character still has an answer.
        assert_eq!(row.cell_of.len(), buf.line_len(0) + 1);
        assert_eq!(row.cell_of[buf.line_len(0)], row.cells.len());
    }

    #[test]
    fn a_hint_past_the_end_of_the_line_lands_at_the_end() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "let x = 1");
        buf.inlay_hints = vec![(0, 99, "  // hint".to_string())];
        let row = display_row(&buf, 0, None);
        assert_eq!(row.cells.iter().collect::<String>(), "let x = 1  // hint");
    }

    #[test]
    fn several_hints_on_one_line_keep_their_order_however_they_arrive() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "f(a, b)");
        // Out of order on purpose: a server needn't sort.
        buf.inlay_hints = vec![(0, 5, "two: ".to_string()), (0, 2, "one: ".to_string())];
        let row = display_row(&buf, 0, None);
        assert_eq!(row.cells.iter().collect::<String>(), "f(one: a, two: b)");
    }

    #[test]
    fn turning_hints_off_draws_the_line_exactly_as_it_is() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "let x = 1");
        buf.inlay_hints = vec![(0, 5, ": u32".to_string())];
        buf.inlayhints = false;
        let row = display_row(&buf, 0, None);
        assert_eq!(row.cells.iter().collect::<String>(), "let x = 1");
        assert_eq!(row.cell_of, (0..=9).collect::<Vec<_>>());
    }

    // Hints splice after tab expansion, so a tab's width follows the source.
    #[test]
    fn a_hint_does_not_shift_the_tab_stops_of_the_line_it_is_on() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.tabstop = 4;
        buf.insert_text((0, 0), "ab\tc");
        let without = display_row(&buf, 0, None);
        buf.inlay_hints = vec![(0, 4, "!!".to_string())];
        let with = display_row(&buf, 0, None);
        // `ab`, two spaces to the stop, `c`, either way.
        assert_eq!(without.cells.iter().collect::<String>(), "ab  c");
        assert_eq!(with.cells.iter().collect::<String>(), "ab  c!!");
    }

    // The hint is dim; the code around it is untouched.
    #[test]
    fn hint_cells_are_dimmed_and_the_real_characters_are_not() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "let x = 1");
        buf.inlay_hints = vec![(0, 5, ": u32".to_string())];
        buf.set_cursor(0, 0);
        let rect = Rect { row: 0, col: 0, rows: 4, cols: 40 };
        let frame = build_editor_frame(&buf, &VimKeys::new(), EditorMode::Normal, rect, 0, 0, None);
        assert!(frame.contains("\x1b[0;2m:"), "expected the hint dimmed:\n{frame:?}");
        assert!(frame.contains("let x"), "the code before it keeps its own styling:\n{frame:?}");
    }

    fn csv_buf(text: &str) -> TextBuffer {
        let mut buf = TextBuffer::open(std::path::Path::new("/tmp/bish-tabular-test.csv"), 10).unwrap();
        buf.insert_text((0, 0), text);
        buf.set_cursor(0, 0);
        buf.tabular = crate::bishedit::tabular::style("csv");
        buf
    }

    // What the pane draws, styling stripped, so a test reads as the screen
    // does.
    fn drawn(buf: &TextBuffer, cols: usize) -> Vec<String> {
        let rect = Rect { row: 0, col: 0, rows: 12, cols };
        let frame = build_editor_frame(buf, &VimKeys::new(), EditorMode::Normal, rect, 0, 0, None);
        let gutter = total_gutter_width(buf);
        // A cursor-position escape starts a row; other escapes are styling.
        let chars: Vec<char> = frame.chars().collect();
        let mut rows: Vec<String> = Vec::new();
        let mut current: Option<String> = None;
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '\x1b' && chars.get(i + 1) == Some(&'[') {
                let mut j = i + 2;
                while j < chars.len() && !chars[j].is_ascii_alphabetic() {
                    j += 1;
                }
                if chars.get(j) == Some(&'H') {
                    if let Some(text) = current.take() {
                        rows.push(text);
                    }
                    current = Some(String::new());
                }
                i = j + 1;
                continue;
            }
            if let Some(row) = current.as_mut() {
                row.push(chars[i]);
            }
            i += 1;
        }
        if let Some(text) = current.take() {
            rows.push(text);
        }
        rows.into_iter().map(|row| row.chars().skip(gutter).collect::<String>().trim_end().to_string()).filter(|row| !row.is_empty()).collect()
    }

    fn folded_buf(text: &str, start: usize, end: usize) -> TextBuffer {
        let mut buf = TextBuffer::open(std::path::Path::new("/tmp/bish-fold-render-test.txt"), 10).unwrap();
        buf.insert_text((0, 0), text);
        buf.set_cursor(0, 0);
        buf.folds.create(start, end);
        buf
    }

    #[test]
    fn a_closed_fold_draws_as_one_row_saying_what_it_holds() {
        let buf = folded_buf("fn main() {\n    body\n}\ntail", 0, 2);
        let rows = drawn(&buf, 40);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].starts_with("+--  3 lines: fn main() {--"), "{rows:?}");
        assert_eq!(rows[1], "tail");
    }

    #[test]
    fn a_closed_fold_is_one_row_to_scrolling_too() {
        let text: String = (0..30).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let mut buf = folded_buf(&text, 1, 20);
        buf.wrap.scrolloff = 0;
        buf.set_viewport_height(10);
        buf.set_cursor(25, 0);
        scroll_to_show_cursor(&mut buf, 40);
        assert_eq!(buf.viewport_top(), 0, "line 25 is only the seventh row");
        buf.set_cursor(29, 0);
        scroll_to_show_cursor(&mut buf, 40);
        assert_eq!(buf.viewport_top(), 1, "the fold's row, then lines 21 to 29, fill the ten rows");
    }

    #[test]
    fn relative_numbers_count_a_closed_fold_as_one() {
        let mut buf = folded_buf("0\n1\n2\n3\n4", 1, 3);
        buf.relativenumber = true;
        assert_eq!(line_number_text(&buf, 4), "2", "`2j` from line 0 reaches line 4");
    }

    #[test]
    fn a_csv_buffer_draws_its_columns_lined_up() {
        let buf = csv_buf("name,age,city\nalice,30,NYC\nbo,7,LA");
        assert_eq!(drawn(&buf, 60), vec!["name,  age, city", "alice, 30,  NYC", "bo,    7,   LA"]);
    }

    // The alignment is drawn, never stored: the buffer holds what was
    // typed.
    #[test]
    fn the_alignment_never_touches_the_text() {
        let mut buf = csv_buf("name,age\nalice,30");
        assert_eq!(buffer_text(&buf), "name,age\nalice,30");
        // A delimiter is still an ordinary character: adding one makes
        // another column.
        buf.insert_text((0, 4), ",");
        assert_eq!(buffer_text(&buf), "name,,age\nalice,30");
        assert_eq!(drawn(&buf, 60).len(), 2);
    }

    // A click on padding belongs to the character before it.
    #[test]
    fn a_click_on_padding_lands_on_the_character_before_it() {
        let buf = csv_buf("alice,30\nbo,7");
        let gutter = total_gutter_width(&buf);
        let rect = Rect { row: 0, col: 0, rows: 12, cols: gutter + 40 };
        // Row 1 draws `bo,    7`: `bo` at 0-1, the comma at 2, padding 3..6,
        // `7` at 7.
        assert_eq!(position_at_screen(&buf, rect, 1, gutter), Some((1, 0)));
        assert_eq!(position_at_screen(&buf, rect, 1, gutter + 2), Some((1, 2)), "the comma itself");
        assert_eq!(position_at_screen(&buf, rect, 1, gutter + 4), Some((1, 2)), "padding belongs to the comma");
        assert_eq!(position_at_screen(&buf, rect, 1, gutter + 7), Some((1, 3)), "the next field");
    }

    #[test]
    fn a_buffer_with_no_tabular_form_is_drawn_exactly_as_before() {
        let mut buf = csv_buf("name,age\nalice,30");
        buf.tabular = None;
        assert_eq!(drawn(&buf, 60), vec!["name,age", "alice,30"]);
    }

    // Wrapping and alignment answer the same question, so only one applies.
    #[test]
    fn wrapping_turns_the_alignment_off() {
        let mut buf = csv_buf("name,age\nalice,30");
        buf.wrap = crate::bishedit::wrap::Options { wrap: true, ..Default::default() };
        assert!(tabular_layout(&buf).is_none());
    }

    #[test]
    fn language_looks_through_a_gz_suffix() {
        assert_eq!(language_of(&buf_with_ext("x", "json.gz")), "json");
        assert_eq!(language_of(&buf_with_ext("x", "sh.gz")), "bash");
        assert_eq!(language_of(&buf_with_ext("x", "gz")), "text", "nothing underneath to go on");
    }

    #[test]
    fn language_of_names_a_file_type_rather_than_enumerating_one() {
        assert_eq!(language_of(&buf_with_ext("x", "bash")), "bash");
        assert_eq!(language_of(&buf_with_ext("x", "sh")), "bash", "a .sh file is bash here too, unlike the old .bash-only check");
        assert_eq!(language_of(&buf_with_ext("x", "rs")), "rust");
        // An unknown extension is its own name, so `abbr --lang=toml` works.
        assert_eq!(language_of(&buf_with_ext("x", "toml")), "toml");
        assert_eq!(language_of(&buf_with_ext("x", "TOML")), "toml", "case-folded, so `--lang=toml` matches either spelling");
        assert_eq!(language_of(&buf_with_ext("x", "txt")), "txt");
        assert_eq!(language_of(&TextBuffer::new_unnamed(10)), "text");
    }

    fn buf_named(name: &str) -> TextBuffer {
        TextBuffer::open(std::path::Path::new(&format!("/tmp/bish-fileeditor-name-test/{name}")), 10).unwrap()
    }

    #[test]
    fn the_ini_family_is_recognized_by_extension() {
        for name in ["settings.ini", "setup.cfg", "sshd.conf", "firefox.desktop", "nginx.service", "boot.timer", "wg0.netdev"] {
            assert_eq!(language_of(&buf_named(name)), "ini", "{name}");
        }
        assert_eq!(language_of(&buf_named("Desktop.INI")), "ini", "case-folded like every other extension");
    }

    // The most-used ones have no extension, hence the name table.
    #[test]
    fn the_extension_less_ini_files_are_recognized_by_name() {
        for name in [".gitconfig", ".editorconfig", ".npmrc", ".pylintrc", "pylintrc", ".flake8"] {
            assert_eq!(language_of(&buf_named(name)), "ini", "{name}");
        }
    }

    #[test]
    fn the_json_with_comments_family_is_recognized() {
        for name in ["settings.jsonc", "tsconfig.json", "tsconfig.app.json", "jsconfig.json", "devcontainer.json", ".eslintrc.json"] {
            assert_eq!(language_of(&buf_named(name)), "jsonc", "{name}");
        }
        assert_eq!(language_of(&buf_named(".vscode/launch.json")), "jsonc", "everything VS Code keeps in .vscode");
        // A plain `.json` stays strict.
        assert_eq!(language_of(&buf_named("package.json")), "json");
    }

    // URLs are found whatever the language.
    #[test]
    fn a_bare_url_becomes_a_link_in_any_buffer() {
        let buf = buf_with_ext("ticket https://bugs.example.com/42 here", "txt");
        let (_, links) = buffer_spans(&buf, None);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "https://bugs.example.com/42");
    }

    #[test]
    fn a_url_at_the_end_of_a_sentence_stops_before_the_full_stop() {
        let buf = buf_with_ext("see https://example.com/x.", "txt");
        let (_, links) = buffer_spans(&buf, None);
        assert_eq!(links[0].url, "https://example.com/x");
    }

    // A relative link resolves against the buffer's directory, so clicking
    // it goes somewhere.
    #[test]
    fn a_relative_link_target_is_resolved_against_the_buffer_directory() {
        let buf = buf_with_ext("[the plan](plan.md)", "md");
        let (_, links) = buffer_spans(&buf, None);
        let plan = links.iter().find(|l| l.url.ends_with("plan.md")).expect("the relative link");
        assert!(plan.url.starts_with("file:///"), "got {}", plan.url);
    }

    // With no directory, a relative target stays underlined and
    // unclickable.
    #[test]
    fn a_relative_link_in_an_unnamed_buffer_produces_no_link() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "[the plan](plan.md)");
        let (_, links) = buffer_spans(&buf, None);
        assert!(links.is_empty());
    }

    #[test]
    fn the_dotenv_family_is_recognized() {
        for name in [".env", ".env.local", ".env.production", ".env.test.local", ".env.example", "local.env", ".flaskenv"] {
            assert_eq!(language_of(&buf_named(name)), "dotenv", "{name}");
        }
    }

    #[test]
    fn envrc_is_bash_rather_than_dotenv() {
        assert_eq!(language_of(&buf_named(".envrc")), "bash");
    }

    #[test]
    fn toml_is_recognized_by_extension_and_by_the_names_that_hide_it() {
        assert_eq!(language_of(&buf_named("Cargo.toml")), "toml");
        assert_eq!(language_of(&buf_named("Pipfile")), "toml");
        assert_eq!(language_of(&buf_named("poetry.lock")), "toml");
        assert_eq!(language_of(&buf_named(".cargo/config")), "toml", "cargo's config, which is not git's");
    }

    #[test]
    fn a_bare_config_is_ini_only_inside_a_git_directory() {
        assert_eq!(language_of(&buf_named(".git/config")), "ini");
        assert_eq!(language_of(&buf_named("git/config")), "ini", "the one under ~/.config");
        assert_eq!(language_of(&buf_named("myapp/config")), "text", "anywhere else it could be anything");
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

    // The formatter's end-of-file fix intends one trailing newline, but the
    // buffer's text has none and `save` adds one, so applied as is it would
    // leave a blank last line on disk. Checked through a real `save()`.
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

    // Through a real file, as `:format` sees a file opened with `e`: an
    // already-formatted file must report `AlreadyFormatted`.
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

// `:git blame`. Everything but the no-path check needs git and a
// repository, and skips without git (`crate::git::available`).
#[cfg(test)]
mod git_blame_tests {
    use super::*;

    // A repo with two commits: "one\ntwo\n", then "one\nTWO\nthree\n".
    fn repo_with_history(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-git-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| crate::gittest::run(&dir, args);
        crate::gittest::init(&dir);
        std::fs::write(dir.join("f.txt"), "one\ntwo\n").unwrap();
        run(&["add", "f.txt"]);
        run(&["commit", "-q", "-m", "initial"]);
        std::fs::write(dir.join("f.txt"), "one\nTWO\nthree\n").unwrap();
        run(&["add", "f.txt"]);
        run(&["commit", "-q", "-m", "second"]);
        dir
    }

    #[test]
    fn toggle_git_blame_refuses_a_buffer_with_no_path() {
        let mut buf = TextBuffer::new_unnamed(10);
        let err = toggle_git_blame(&mut buf, None, true).unwrap_err();
        assert!(err.contains("no file name"), "{err}");
    }

    #[test]
    fn toggle_git_blame_toggles_on_then_off_for_a_real_git_repo_file() {
        if !crate::git::available() {
            return;
        }
        let dir = repo_with_history("blame-test");
        let mut buf = TextBuffer::open(&dir.join("f.txt"), 10).unwrap();
        assert!(!buf.is_dirty());
        assert!(toggle_git_blame(&mut buf, None, true).unwrap());
        let blame = buf.blame.as_ref().unwrap();
        assert_eq!(blame.len(), 3);
        let first = blame[0].as_ref().unwrap();
        assert_eq!(first.author, "Test User");
        assert_eq!(first.short_commit.len(), 8);
        assert!(!toggle_git_blame(&mut buf, None, true).unwrap());
        assert!(buf.blame.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // A line typed just now has no blame, and every line after it still
    // gets its own despite having moved down.
    #[test]
    fn toggle_git_blame_lines_up_with_a_dirty_buffer() {
        if !crate::git::available() {
            return;
        }
        let dir = repo_with_history("blame-dirty-test");
        let mut buf = TextBuffer::open(&dir.join("f.txt"), 10).unwrap();
        buf.insert_text((0, 0), "TYPED JUST NOW\n");
        assert!(buf.is_dirty());

        assert!(toggle_git_blame(&mut buf, None, true).unwrap());
        let blame = buf.blame.as_ref().unwrap();
        assert_eq!(blame.len(), 4);
        assert!(blame[0].is_none(), "the typed line has no blame");
        for (i, entry) in blame.iter().enumerate().skip(1) {
            assert_eq!(entry.as_ref().unwrap().author, "Test User", "line {i}");
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // Blame at an older revision against newer content: `three` didn't
    // exist then, so it has none, and the other two still line up.
    #[test]
    fn toggle_git_blame_at_a_revision_marks_lines_absent_from_it() {
        if !crate::git::available() {
            return;
        }
        let dir = repo_with_history("blame-rev-test");
        let mut buf = TextBuffer::open(&dir.join("f.txt"), 10).unwrap();

        assert!(toggle_git_blame(&mut buf, Some("HEAD~1"), true).unwrap());
        let blame = buf.blame.as_ref().unwrap();
        assert_eq!(blame.len(), 3);
        assert!(blame[0].is_some(), "`one` is unchanged since HEAD~1");
        assert!(blame[1].is_none(), "`TWO` only exists after HEAD~1");
        assert!(blame[2].is_none(), "`three` only exists after HEAD~1");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_commit_at_the_cursor_is_the_one_blame_names_for_that_line() {
        if !crate::git::available() {
            return;
        }
        let dir = repo_with_history("blame-show-test");
        let rev = |name: &str| {
            let out = crate::gittest::output(&dir, &["rev-parse", name]);
            String::from_utf8(out.stdout).unwrap().trim()[..8].to_string()
        };
        // A line on disk that no commit has, before blaming.
        std::fs::write(dir.join("f.txt"), "one\nTWO\nthree\nfour\n").unwrap();
        let mut buf = TextBuffer::open(&dir.join("f.txt"), 10).unwrap();
        assert_eq!(blamed_commit_at_cursor(&buf), None, "with blame off there is nothing to say, so `:git show` stays HEAD");

        assert!(toggle_git_blame(&mut buf, None, true).unwrap());
        buf.set_cursor(0, 0);
        assert_eq!(blamed_commit_at_cursor(&buf), Some(Ok(rev("HEAD~1"))), "`one` came in with the first commit");
        buf.set_cursor(1, 0);
        assert_eq!(blamed_commit_at_cursor(&buf), Some(Ok(rev("HEAD"))), "`TWO` with the second");
        buf.set_cursor(3, 0);
        assert_eq!(blamed_commit_at_cursor(&buf), Some(Err("this line is not committed yet".to_string())));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggle_git_blame_reports_an_unknown_revision() {
        if !crate::git::available() {
            return;
        }
        let dir = repo_with_history("blame-badrev-test");
        let mut buf = TextBuffer::open(&dir.join("f.txt"), 10).unwrap();
        let err = toggle_git_blame(&mut buf, Some("no-such-rev"), true).unwrap_err();
        assert!(err.contains("no-such-rev"), "{err}");
        assert!(buf.blame.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

// `:git diff`, skipping without git like the blame tests.
#[cfg(test)]
mod git_diff_tests {
    use super::*;
    use crate::git::DiffMark;

    #[test]
    fn toggle_git_diff_refuses_a_buffer_with_no_path() {
        let mut buf = TextBuffer::new_unnamed(10);
        let err = toggle_git_diff(&mut buf, None).unwrap_err();
        assert!(err.contains("no file name"), "{err}");
    }

    #[test]
    fn toggle_git_diff_toggles_on_then_off_and_marks_a_changed_line() {
        if !crate::git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-git-diff-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::gittest::init(&dir);
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        crate::gittest::run(&dir, &["add", "f.txt"]);
        crate::gittest::run(&dir, &["commit", "-q", "-m", "initial"]);
        // Changed on disk and reloaded, so it differs from the commit.
        std::fs::write(&path, "one\nCHANGED\nthree\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert!(!buf.is_dirty());
        assert!(toggle_git_diff(&mut buf, None).unwrap());
        assert_eq!(buf.diff.as_ref().unwrap().get(&1), Some(&DiffMark::Changed));
        assert_eq!(buf.diff.as_ref().unwrap().len(), 1);
        assert!(!toggle_git_diff(&mut buf, None).unwrap());
        assert!(buf.diff.is_none());

        // An unsaved edit counts too: the buffer is diffed, not the file.
        buf.insert_text((0, 0), "TYPED\n");
        assert!(toggle_git_diff(&mut buf, None).unwrap());
        let diff = buf.diff.as_ref().unwrap();
        assert_eq!(diff.get(&0), Some(&DiffMark::Added));
        assert_eq!(diff.get(&2), Some(&DiffMark::Changed));
        assert_eq!(diff.len(), 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // `:git diff HEAD~1` on a buffer matching HEAD: the markers describe
    // the older revision.
    #[test]
    fn toggle_git_diff_at_a_revision_marks_what_changed_since_it() {
        if !crate::git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-git-diff-rev-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        crate::gittest::init(&dir);
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        crate::gittest::run(&dir, &["add", "f.txt"]);
        crate::gittest::run(&dir, &["commit", "-q", "-m", "initial"]);
        std::fs::write(&path, "one\nTWO\nthree\n").unwrap();
        crate::gittest::run(&dir, &["add", "f.txt"]);
        crate::gittest::run(&dir, &["commit", "-q", "-m", "second"]);

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        // Against the index there's nothing.
        assert!(toggle_git_diff(&mut buf, None).unwrap());
        assert!(buf.diff.as_ref().unwrap().is_empty());
        assert!(!toggle_git_diff(&mut buf, None).unwrap());
        // Against HEAD~1, one changed hunk replacing `two` with two lines, as
        // `git diff -U0` reports (`@@ -2 +2,2 @@`).
        assert!(toggle_git_diff(&mut buf, Some("HEAD~1")).unwrap());
        let diff = buf.diff.as_ref().unwrap();
        assert_eq!(diff.get(&1), Some(&DiffMark::Changed));
        assert_eq!(diff.get(&2), Some(&DiffMark::Changed));
        assert_eq!(diff.len(), 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggle_git_diff_marks_every_line_added_for_an_untracked_file() {
        if !crate::git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-git-diff-untracked-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::gittest::init(&dir);
        let path = dir.join("new.txt");
        std::fs::write(&path, "a\nb\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        toggle_git_diff(&mut buf, None).unwrap();
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
        let err = toggle_git_diff(&mut buf, None).unwrap_err();
        assert!(err.to_lowercase().contains("git repository"), "{err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
