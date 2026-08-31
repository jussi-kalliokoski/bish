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
use crate::bishedit::highlight::{self, HighlightContext, Highlighter, StyledSpan};
use crate::bishedit::lint::{self, BashLinter, Linter};
use crate::lsp;
use crate::bishedit::motion;
use crate::bishedit::registers::{RegisterShape, RegisterValue, Registers};
use crate::bishedit::snippet::{self, Abbr, LiveSnippet, Snippet, SnippetHost};
use crate::bishedit::textbuffer;
use crate::bishedit::textbuffer::TextBuffer;
use crate::bishedit::unicode_width::char_width;
use crate::bishedit::vimkeys::{InsertCmd, Op, SurroundTarget, VimKeys};
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
    // own doc comment) that file. Compressed content (a gzip'd file, a
    // member inside a zip) opens read-only -- see `compressed_text`.
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

// The text behind a path that holds compressed content -- a member
// inside a zip (`some.zip!/dir/file.txt`) or a gzip'd file
// (`notes.txt.gz`) -- or `None` for an ordinary path, which is every
// caller's cue to read it the normal way.
//
// Read-only follows from the format, not from caution: writing either
// one back means compressing, which crate::inflate deliberately doesn't
// do (see its own module comment). Every caller marks the buffer
// readonly, so `:w` refuses with the same message any other read-only
// buffer gives rather than silently producing a corrupt archive.
//
// Bytes become text lossily. A member that isn't text at all comes out
// as replacement characters, which is a legible "this isn't text" and
// leaves `e --hex` as the way to actually look at it -- the same thing
// opening any binary file in this editor already does.
pub(crate) fn compressed_text(path: &str) -> Option<Result<String, String>> {
    compressed_bytes(path).map(|r| r.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

// compressed_text's own source, before the lossy text conversion -- what
// `e --hex` on the same path wants instead (hexedit::HexBuffer::
// from_bytes), since the whole point there is to see the real bytes.
pub(crate) fn compressed_bytes(path: &str) -> Option<Result<Vec<u8>, String>> {
    if let Some((archive, inner)) = crate::archive::split(path) {
        return Some(crate::archive::read_member(&archive, &inner));
    }
    let as_path = std::path::Path::new(path);
    match crate::archive::kind_of(as_path) {
        Some(crate::archive::Kind::Gzip) => Some(crate::archive::gunzip(as_path).map(|(_, bytes)| bytes)),
        // A zip or a tar names a directory of members, not content --
        // `e` browses one (repl::expand_browse_targets) rather than
        // reaching here. `e --hex some.zip` does reach here, and wants
        // the archive's own raw bytes, which is what reading the file
        // gives.
        Some(crate::archive::Kind::Zip) | Some(crate::archive::Kind::Tar) | None => None,
    }
}

// One file `e` was asked to open. `e` takes several at once (`e a.sh
// b.sh`), opening one editor frame per target on the focused pane's own
// frame stack -- the first one named ends up on top, and closing it
// (`:q`) reveals the next, which is exactly what that stack already does
// for every other frame kind (see repl.rs's `Frame`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EditTarget {
    // `None` is a fresh unnamed buffer -- a bare `e` with no arguments
    // at all, or flags with no file after them.
    pub path: Option<String>,
    // `--hex`: open this file as raw bytes in the hex editor
    // (hexedit::HexSession, a `Frame::Hex`) instead of as text.
    pub hex: bool,
    // `--readonly`: refuse every edit. Meaningful for both kinds of
    // buffer (`TextBuffer::set_readonly`, `HexBuffer::readonly`) --
    // opening something to look at without any chance of changing it by
    // accident is exactly as reasonable for a script as for a core dump.
    pub readonly: bool,
}

// `e`'s own argument parsing, shared by the builtin (via `ExecResult::
// Edit`) and `bish tool edit` so the two can't drift apart.
//
// Flags are *per-file*: they attach to the file that follows them, so
// `e script.sh --hex core.bin` opens one of each and `e --hex a.bin
// b.bin` opens only `a.bin` as hex. Prefix binding (rather than
// suffix, or a single mode for the whole command) is the only
// unambiguous rule once several files can be named at once, and it's the
// one every other command line already uses. Flags with no file after
// them apply to a fresh unnamed buffer, so `e --hex` opens an empty one
// to build a binary in.
//
// `--` ends option parsing, the usual way, so a file whose name really
// does start with `-` is still openable (`e -- -weird.txt`). An
// unrecognized leading-dash argument is an error rather than a filename:
// silently opening a buffer named `--hxe` because of a typo is a far
// worse outcome than being told the flag isn't one.
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
                // `-` alone is left alone: not a flag anywhere in this
                // codebase, and conventionally a filename-shaped
                // stand-in for a stream.
                other if other.starts_with('-') && other != "-" => {
                    return Err(format!("unrecognized option '{arg}'"));
                }
                _ => {}
            }
        }
        targets.push(EditTarget { path: Some(arg.clone()), ..std::mem::take(&mut pending) });
    }
    // Trailing flags (or no arguments at all) -- one unnamed buffer,
    // carrying whatever was set for it.
    if targets.is_empty() || pending != EditTarget::default() {
        targets.push(pending);
    }
    Ok(targets)
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
    if buf.wrap.wrap {
        scroll_wrapped(buf, content_cols);
        return;
    }
    let (line, col) = buf.cursor();
    let height = buf.viewport_height();
    // `scrolloff`: keep this many lines visible above and below the
    // cursor. Capped at half the pane for the same reason
    // `sidescrolloff` is -- a margin wider than that has no middle left
    // to keep the cursor in -- and clamped against the buffer's own ends,
    // since there is nothing to scroll past the first and last lines.
    let margin = buf.wrap.scrolloff.min(height.saturating_sub(1) / 2);
    let top = buf.viewport_top();
    let highest = buf.line_count().saturating_sub(height);
    if line < top + margin {
        buf.set_viewport_top(line.saturating_sub(margin));
    } else if line + margin >= top + height {
        buf.set_viewport_top((line + margin + 1 - height).min(highest));
    }
    // In drawn columns, so a tabular file scrolls by what is on screen
    // rather than by how many characters the line happens to hold.
    let display = display_row(buf, line, tabular_layout(buf).as_ref());
    let cursor_col = col_at_cell(&display.cells, display.cell_of[col.min(buf.line_len(line))]);
    let width = content_cols.max(1);
    // `sidescrolloff`: keep this many columns visible either side of the
    // cursor rather than letting it sit against the edge. Capped at half
    // the pane, since a margin wider than that has no middle to keep the
    // cursor in.
    let margin = buf.wrap.sidescrolloff.min(width.saturating_sub(1) / 2);
    if cursor_col < buf.viewport_left() + margin {
        buf.set_viewport_left(cursor_col.saturating_sub(margin));
    } else if cursor_col + margin >= buf.viewport_left() + width {
        buf.set_viewport_left(cursor_col + margin + 1 - width);
    }
}

// How far one horizontal wheel notch moves the view. More than
// MOUSE_WHEEL_LINES because a column is a much smaller step than a line
// -- three columns a notch would feel like nothing.
pub(crate) const MOUSE_WHEEL_COLUMNS: usize = 6;

// Moves the view sideways by `columns` (negative for left), the way the
// mouse's horizontal wheel and vim's own `zh`/`zl` do -- and then brings
// the *cursor* to the view rather than the view back to the cursor,
// which is the whole difference between this and
// `scroll_to_show_cursor` above.
//
// A no-op while `wrap` is on: a wrapped line has nothing off to the side
// to scroll into, by construction.
pub(crate) fn scroll_horizontally(buf: &mut TextBuffer, columns: isize, content_cols: usize) {
    if buf.wrap.wrap || columns == 0 {
        return;
    }
    let width = content_cols.max(1);
    let layout = tabular_layout(buf);
    // Only as far as there is something to see. The widest *visible*
    // line, not the widest in the file: it is the only thing the reader
    // could be scrolling towards, and it costs a pass over one screen
    // rather than over the whole buffer.
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

    // ...and drag the cursor along, so the next keystroke doesn't snap
    // the view straight back. Same `sidescrolloff` margin
    // scroll_to_show_cursor keeps, for the same reason.
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

// The same job in visual rows: which row the cursor is on, and whether
// the window has to move for it. Horizontal scroll is forced to zero --
// a wrapped line has no off-screen right edge to scroll to.
fn scroll_wrapped(buf: &mut TextBuffer, content_cols: usize) {
    buf.set_viewport_left(0);
    let (line, col) = buf.cursor();
    let height = buf.viewport_height().max(1);
    let seg = crate::bishedit::wrap::segment_of(&line_segments(buf, line, content_cols), col);

    // `scrolloff`, counted in *visual* rows here rather than lines: with
    // wrapping on, "two lines below the cursor" and "two rows below the
    // cursor" are different amounts of screen, and rows are what the
    // margin is actually protecting.
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

    // Above the window: the cursor's own row moves to `margin` rows
    // below the top -- with no margin, to the top itself.
    let above = line < buf.viewport_top() || (line == buf.viewport_top() && seg < buf.viewport_sub());
    if above || rows_between(buf, (buf.viewport_top(), buf.viewport_sub()), (line, seg), margin, content_cols) < margin {
        let top = back_from(buf, (line, seg), margin);
        buf.set_viewport_top(top.0);
        buf.set_viewport_sub(top.1);
        return;
    }
    // Below it: walk forward from the top, and if the cursor isn't
    // reached within the pane's height minus the margin, put it that
    // many rows above the bottom by walking back from it instead.
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

// How many visual rows `to` sits below `from`, giving up once it passes
// `limit` -- the caller only ever asks "is it at least this far", and a
// buffer can be long enough that walking the true distance is wasted.
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

fn next_row(buf: &TextBuffer, (line, sub): (usize, usize), content_cols: usize) -> (usize, usize) {
    if sub + 1 < line_segments(buf, line, content_cols).len() {
        (line, sub + 1)
    } else {
        (line + 1, 0)
    }
}

fn previous_row(buf: &TextBuffer, (line, sub): (usize, usize), content_cols: usize) -> (usize, usize) {
    if sub > 0 {
        return (line, sub - 1);
    }
    if line == 0 {
        return (0, 0);
    }
    (line - 1, line_segments(buf, line - 1, content_cols).len().saturating_sub(1))
}

// How many columns are actually left for `buf`'s own text after its
// gutter (line numbers, diagnostic markers) -- the exact formula build_
// editor_frame itself uses for `content_cols`, factored out so scroll_
// to_show_cursor's callers (run_insert_mode, and repl.rs's own copy for
// NavBuffer navigation) can compute the same width without duplicating
// the gutter-clamping arithmetic.
// The buffer position a click at real terminal row/column `row0`/`col0`
// (both 0-indexed) lands on, for a buffer drawn into `rect` -- the exact
// inverse of build_editor_frame's own placement, so the two can't
// disagree about which character is under the pointer: same gutter
// width, same `viewport_top`/`viewport_left`, same `char_at_col`
// translation from a display column to a char index (they differ the
// moment a line holds anything wide).
//
// `None` for a click outside `rect` or below the last row that holds
// content. A click in the gutter, or past the end of a line, resolves to
// the nearest real position on that line rather than nothing -- which is
// what every editor does, and what makes clicking in the rough vicinity
// of a line feel like it worked.
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
    // The same rows the frame drew, so a click can't land on a line the
    // screen doesn't show there.
    let rows = visible_rows(buf, content_cols, editor_content_rows(rect));
    let visual = rows.get(row)?;
    let line = visual.line;
    let x = (col0 - rect.col).saturating_sub(gutter);
    let chars = buf.line_chars(line);
    let display = display_row(buf, line, tabular_layout(buf).as_ref());
    let cell = if buf.wrap.wrap {
        // Within a wrapped row, the click is an offset into that row's
        // own segment, past the continuation prefix.
        let into = x.saturating_sub(visual.seg.indent);
        let base = col_at_cell(&display.cells, display.cell_of[visual.seg.start.min(chars.len())]);
        cell_at_col(&display.cells, base + into)
    } else {
        cell_at_col(&display.cells, buf.viewport_left() + x)
    };
    // A click landing on padding belongs to the character it was
    // inserted after -- padding is not part of the file, so there is
    // nothing else it could mean.
    let col = source_at_or_before(&display, cell);
    let col = if buf.wrap.wrap { col.min(visual.seg.end.saturating_sub(1).max(visual.seg.start)) } else { col };
    // Normal mode's cursor can never sit past a line's last character
    // (see run_insert_mode's own exit clamp for the same rule), so an
    // overshooting click lands on it instead.
    Some((line, col.min(chars.len().saturating_sub(1))))
}

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

// The completion popup: a list anchored under the cursor, the selected
// row marked.
//
// Drawn in reverse video like `render_hover_popup` just above, for the
// same reason -- it floats over real text, and reverse video is the one
// way to be legible over it without assuming anything about colours.
// The selection is marked with `>` *and* bold rather than by colour
// alone, so it is unambiguous on a terminal that has none.
pub(crate) fn render_completion_popup(items: &[crate::bishedit::completion::EditorCompletion], selected: usize, cursor_row: usize, cursor_col: usize, rect: Rect) -> String {
    let rows: Vec<(String, String)> = items.iter().map(|i| (i.label.clone(), i.detail.clone())).collect();
    render_popup_list(&rows, selected, cursor_row, cursor_col, rect)
}

/// The same widget over plain `(label, detail)` rows -- what a code
/// action picker needs, and what completion is underneath.
pub(crate) fn render_popup_list(items: &[(String, String)], selected: usize, cursor_row: usize, cursor_col: usize, rect: Rect) -> String {
    const MAX_ROWS: usize = 10;
    if items.is_empty() {
        return String::new();
    }
    // Scrolled so the selection is always on screen: a server can
    // answer with hundreds.
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
    let shape = match range.shape {
        motion::MotionShape::Linewise => RegisterShape::Line,
        motion::MotionShape::Blockwise => RegisterShape::Block,
        _ => RegisterShape::Char,
    };
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
// The selection between two caret positions, as the `Inclusive` range
// the rest of this editor already understands -- so an Insert-mode
// selection renders, yanks and deletes through exactly the same code a
// Visual one does.
//
// Carets sit *between* characters and a range covers characters, so the
// later end steps back one: dragging from before `a` to before `c`
// selects `ab`, not `abc`. `None` when the two carets are in the same
// place, which is a cursor rather than a selection.
fn selection_between(buf: &TextBuffer, a: (usize, usize), b: (usize, usize)) -> Option<motion::MotionRange> {
    let (from, to) = if a <= b { (a, b) } else { (b, a) };
    if from == to {
        return None;
    }
    let to = if to.1 > 0 {
        (to.0, to.1 - 1)
    } else {
        // The caret is at the start of a line, so the selection ends at
        // the end of the one before it.
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

// How many leading characters of `row` come to one indent's worth of
// columns -- what `<<` removes. A tab counts for a whole tabstop, and a
// line indented by less than a full level just loses what it has, which
// is vim's own rule.
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

// One indent's worth of characters to every *non-empty* line in `from_row..=to_row` --
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
        buf.insert_text((row, 0), &one_indent(buf));
    }
}

// `<{motion}`/`<<`/Visual `<`'s own counterpart: strips up to
// INDENT_WIDTH columns of leading whitespace from every line in range --
// vim's own "outdent removes at most one shiftwidth's worth" rule (a
// line indented less than that just loses whatever it has).
fn outdent_rows(buf: &mut TextBuffer, from_row: usize, to_row: usize) {
    for row in from_row..=to_row {
        // In *columns*, not characters: with `expandtab` off one tab is
        // a whole indent, so stripping `shiftwidth` characters would
        // strip a whole shiftwidth of tabs.
        let strip = outdent_chars(buf, row);
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

// `x`: deletes up to `count` *grapheme clusters* starting at the
// cursor, clamped to the end of the line -- vim's own primitive (see
// `vimkeys::apply_delete_forward`'s own doc comment on why this isn't
// quite reducible to `d{count}l`; that's the plain shell prompt's own
// separate, single-line, non-cluster-aware implementation of the same
// primitive -- kept out of scope here the same way this codebase's
// other multi-select/vim-feature work already draws that exact
// boundary, see e.g. multi-selection `c`'s own doc comment). Cluster-,
// not char-index-, counted: pressing `x` once on a ZWJ emoji sequence
// deletes the *whole* glyph in one press rather than leaving broken
// remnants behind, and `3x` deletes 3 whole clusters, not 3 raw
// codepoints (which could otherwise split a cluster right down the
// middle, deleting only part of it).
pub(crate) fn delete_char_forward(buf: &mut TextBuffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
    let (row, col) = buf.cursor();
    let len = buf.line_len(row);
    if len == 0 {
        return;
    }
    let chars = buf.line_chars(row);
    // Clamped to this cluster's own start first (not just the last
    // valid char index) -- defense in depth matching char_at_col's own
    // "never lands inside a wide char" convention, in case the cursor
    // ever got here sitting mid-cluster through some other path.
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
        // A block goes back as a rectangle: each of its lines at the
        // *same column* on consecutive lines, rather than spliced end to
        // end. Which is the whole difference between a block and the
        // same characters yanked charwise, and the reason `Block` is a
        // register shape rather than a detail of how it was selected.
        //
        // A line too short to reach that column is padded out to it, so
        // the rectangle stays a rectangle -- vim does the same.
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

/// The whitespace a new line opened from `row` should start with:
/// exactly `row`'s own leading whitespace, copied.
///
/// **Plain** autoindent, vim's `autoindent` and not its `smartindent` --
/// no opening a level after `{` or `then`, no closing one on `}`. The
/// virtue of copying is that it is never wrong in a way that surprises:
/// it puts the caret where the eye already is, and it cannot mis-guess a
/// language's block structure because it never guesses.
///
/// Empty when the line is only whitespace, so pressing Enter on a blank
/// line does not leave a trail of trailing spaces behind.
pub(crate) fn autoindent_for(buf: &TextBuffer, row: usize) -> String {
    let chars = buf.line_chars(row);
    if chars.iter().all(|c| c.is_whitespace()) {
        return String::new();
    }
    chars.iter().take_while(|c| **c == ' ' || **c == '\t').collect()
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
// What one idle tick reported. `None` is "nothing happened"; `Some` is
// "repaint, into *this* geometry".
//
// The two travel together on purpose. A caller holding a live
// `TextBuffer`/`VimKeys` has to know when to repaint, because
// `compositor_redraw` (already run inside `on_idle` itself) paints a
// driven `Frame::Edit` pane blank -- see service_background_jobs's own
// doc comment. But the commonest reason to be told that is a terminal
// resize, which is exactly the moment the geometry captured before
// Insert mode began stopped describing anything real. Reporting only
// *whether* to repaint -- as this did -- meant Insert mode faithfully
// repainted a 38-row frame into a 20-row terminal.
#[derive(Clone, Copy)]
pub(crate) struct IdleRedraw {
    pub(crate) rect: Rect,
    pub(crate) term_rows: usize,
    pub(crate) term_cols: usize,
}

/// What Insert mode needs from whoever is driving it.
///
/// One object rather than two closures because both of them need the
/// same mutable state (the session table, the window list), and two
/// closures capturing it are two simultaneous unique borrows -- which
/// the compiler is right to refuse. Methods on one object take that
/// borrow once.
///
/// `idle` is called while nothing is arriving on stdin; `complete` when
/// Ctrl-N asks what could go at the cursor. A caller with no language
/// server implements the second as "nothing", which is what makes
/// Ctrl-N a no-op rather than an error in a plain editor.
pub(crate) trait InsertServices {
    fn idle(&mut self, buf: &mut TextBuffer) -> Option<IdleRedraw>;
    fn complete(&mut self, _buf: &TextBuffer, _row: usize, _col: usize) -> Vec<crate::bishedit::completion::EditorCompletion> {
        Vec::new()
    }
}

/// For a caller with nothing to offer: no idle work, no completions.
/// Only this module's own tests today, which is why it is test-only --
/// every real caller has a shell behind it.
#[cfg(test)]
pub(crate) struct NoInsertServices;

#[cfg(test)]
impl InsertServices for NoInsertServices {
    fn idle(&mut self, _buf: &mut TextBuffer) -> Option<IdleRedraw> {
        None
    }
}

// The completion popup while it is open.
//
// Fetched once, then filtered as you keep typing rather than re-asked
// -- which is what makes it feel instant, and is honest about what it
// is: a snapshot of what the server said at the moment you asked. A
// server that wanted to be re-asked says so with `isIncomplete`, which
// this client reads and ignores (see `lsp::completions`).
struct LiveCompletion {
    /// Everything the server offered, in its own order.
    all: Vec<crate::bishedit::completion::EditorCompletion>,
    /// Indices into `all` still matching what has been typed.
    shown: Vec<usize>,
    /// Index into `shown`.
    selected: usize,
    /// Where the word being completed starts, on `row`. Captured when
    /// the popup opens so the filter can see what has been typed since.
    row: usize,
    word_start: usize,
}

impl LiveCompletion {
    // Narrows to the candidates still matching the word under the
    // cursor. `false` when nothing matches any more, which is the
    // caller's cue to close: a popup with no rows is just a box.
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_insert_mode(
    buf: &mut TextBuffer,
    vk: &mut VimKeys,
    rect: Rect,
    registers: &mut Registers,
    // Takes the buffer, rather than closing over it: the caller needs
    // it mutably for the whole of this call, and an idle tick that has
    // to see the live text (document synchronization -- see repl.rs's
    // `sync_language_server_document`) can't borrow it a second time.
    // Handing it in at the one moment nothing else holds it is what
    // makes both possible.
    services: &mut dyn InsertServices,
    replace: bool,
    term_rows: usize,
    term_cols: usize,
    color_overrides: Option<&highlight::ColorOverrides>,
    extra_cursors: &[(usize, usize)],
    // The shell's whole abbreviation table, narrowed here to the ones
    // targeting this file's own language -- see `language_of` and
    // `abbr --lang=`. Empty for callers with no shell behind them.
    abbrs: &[Abbr],
) -> io::Result<()> {
    let mode = if replace { EditorMode::Replace } else { EditorMode::Insert };
    // Rebound as mutable: an idle tick may hand back new geometry (see
    // IdleRedraw), and every later render in this function has to use
    // it rather than what the caller measured before Insert mode began.
    let (mut rect, mut term_rows, mut term_cols) = (rect, term_rows, term_cols);
    let mut cursors: Vec<(usize, usize)> = std::iter::once(buf.cursor()).chain(extra_cursors.iter().copied()).collect();
    // Where a mouse drag started, while one is in progress. Insert mode
    // gets a selection of its own -- the one a GUI editor gives you --
    // rather than borrowing Visual mode's: Visual is a *mode*, and the
    // point here is to select without leaving the one you are typing in.
    let mut drag_anchor: Option<(usize, usize)> = None;
    // Resolved once per Insert-mode session: the file's language can't
    // change while it's being typed into, and neither can the table (the
    // caller snapshots it fresh on the way in -- see repl.rs).
    let abbrs = snippet::for_language(abbrs, &language_of(buf));
    let mut live: Option<LiveSnippet> = None;
    let mut completing: Option<LiveCompletion> = None;
    // Before the first frame, not just after every keystroke.
    //
    // Where insert starts is chosen by the *caller* -- `a` steps the
    // cursor past the last character, `o` opens a line below, `A` goes
    // to the end -- and any of those can land outside the viewport the
    // pane was last scrolled to. Without this the first thing typed
    // goes in at a position nobody can see: `a` at the end of a line
    // too long for the pane types off the right edge, and `o` under a
    // horizontally scrolled line types at column 0 while the viewport
    // is still scrolled right. Both were reported; both are this one
    // missing call, because the loop below has always scrolled *after*
    // a key and never before the first.
    buf.set_viewport_height(editor_content_rows(rect));
    scroll_to_show_cursor(buf, editor_content_cols(buf, rect));
    render_editor_frame(buf, vk, mode, rect, term_rows, term_cols, color_overrides);
    // `"."`'s own accumulator for this session -- see `Registers::
    // set_last_insert`'s own doc comment. Best-effort: a Backspace just
    // pops the most recently accumulated character regardless of whether
    // it's actually erasing something typed *this* session or older
    // pre-existing text it backed into -- real vim tracks that
    // distinction precisely; this doesn't.
    let mut inserted = String::new();
    // Whether the bytes arriving right now were pasted rather than
    // typed, bracketed by the terminal (see `Key::PasteStart`). Nothing
    // that helpfully reacts to typing runs while this is set:
    // autoindent would staircase every line, and `abbr` would expand a
    // word that happens to look like an abbreviation in someone else's
    // code.
    let mut pasting = false;
    // Asking the real terminal to bracket pastes for the duration.
    // `sync_bracketed_paste` (repl.rs) does the same thing for a
    // *foreground job* that asked for it; the two never overlap, since
    // no job is being driven while this loop owns the keyboard.
    let _paste_guard = term::BracketedPasteGuard::enable();
    loop {
        // Waits for a byte to actually be ready *before* calling
        // vk.next_key below, rather than passing `on_idle` as that
        // call's own on_idle closure -- vk.next_key(&mut self, ...)
        // holds `vk` (and this function needs `buf` too) exclusively
        // borrowed for its whole call, which would make it impossible
        // for `on_idle` to also redraw them from inside that call. See
        // run_normal_mode_navigation's own identical restructuring
        // (repl.rs) for the full reasoning -- same fix, same root cause.
        while !term::stdin_ready(editor::IDLE_POLL_MS) {
            if let Some(geometry) = services.idle(buf) {
                rect = geometry.rect;
                term_rows = geometry.term_rows;
                term_cols = geometry.term_cols;
                // A shrink can leave the cursor below the viewport, and
                // working that out needs the new height -- which is a
                // stored field with no resize hook of its own.
                buf.set_viewport_height(editor_content_rows(rect));
                scroll_to_show_cursor(buf, editor_content_cols(buf, rect));
                render_editor_frame(buf, vk, mode, rect, term_rows, term_cols, color_overrides);
            }
        }
        // Goes through `vk.next_key` (not a bare `read_key_idle`) so a
        // macro recorded/replayed from Normal mode -- see its own doc
        // comment -- still works across a full Insert-mode excursion:
        // `run_normal_mode_navigation` calls this function inline, with
        // the same `vk`, so nothing here needs its own bookkeeping. A
        // byte is already known ready (just confirmed above), so this
        // on_idle closure is never actually called.
        let key = match vk.next_key(|| editor::read_key_idle(&mut || {}))? {
            Some(k) => k,
            None => {
                buf.set_mark('^', buf.cursor());
                registers.set_last_insert(inserted);
                return Ok(());
            }
        };
        // Any key outside a live snippet's own vocabulary accepts it as
        // it stands and then means whatever it always meant -- the exact
        // rule read_line already follows for the same thing at the shell
        // prompt, and what keeps every arrow/motion/Escape arm below free
        // of snippet-aware special cases.
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

        // Any key the popup does not use closes it and then means
        // whatever it always meant -- the same rule a live snippet just
        // above follows, and what keeps every other arm free of
        // completion-aware special cases. `Char`/`Backspace` are not in
        // this list because they *narrow* the popup rather than
        // dismissing it: typing more of a word is how you pick from one.
        if completing.is_some()
            && !matches!(key, Key::CtrlN | Key::CtrlP | Key::Enter | Key::Tab | Key::Escape | Key::Char(_) | Key::Backspace)
        {
            completing = None;
        }

        // Typing with a selection standing replaces it -- select-and-type,
        // the other half of the Backspace/Delete arms below and what a
        // selection means everywhere one can exist. Done here rather than
        // as another guarded arm because the arms that insert differ only
        // in *what* they insert, and every one of them would need the
        // same four lines.
        //
        // Only a plain character, and only with no popup or snippet live:
        // those two own `Char` while they are up (it narrows a completion
        // and fills a tabstop), and Replace mode's whole contract is that
        // the line's length does not change.
        if !buf.selections.is_empty() && !replace && live.is_none() && completing.is_none() && matches!(key, Key::Char(_)) {
            let range = buf.selections[0];
            buf.selections.clear();
            buf.delete_range(&range);
            buf.set_cursor(range.from.0, range.from.1);
            cursors = vec![buf.cursor()];
        }

        match key {
            // --- the completion popup owns these while it is open ----
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
            // Escape closes the popup and stays in Insert mode -- the
            // one place Escape does not leave, matching every editor
            // with a completion menu.
            Key::Escape if completing.is_some() => {
                completing = None;
            }
            Key::Enter | Key::Tab if completing.is_some() => {
                let state = completing.take().unwrap();
                if let Some(item) = state.chosen().cloned() {
                    let (row, col) = buf.cursor();
                    // The server's own range when it named one; the word
                    // being typed otherwise.
                    let (row, start, end) = item.replace.unwrap_or((row, state.word_start, col));
                    let line_len = buf.line_len(row);
                    let (start, end) = (start.min(line_len), end.min(line_len).max(start.min(line_len)));
                    // A server's snippet completion becomes exactly the
                    // same live snippet an `abbr` does -- same keys,
                    // same marking, same accept -- so `fn ${1:name}()`
                    // arrives with the caret already in `name` instead
                    // of as literal punctuation to clean up.
                    match item.snippet.then(|| Snippet::parse(&item.insert)).flatten() {
                        Some(snip) => {
                            let replaced: String = buf.line_chars(row)[start..end].iter().collect();
                            live = Some(LiveSnippet::start(snip, row, start, replaced, buf));
                        }
                        None => {
                            // `flatten` and not the raw text: a server
                            // that flagged a snippet with no tabstops in
                            // it still wrote `\$` for a literal dollar.
                            let text = if item.snippet { snippet::flatten(&item.insert) } else { item.insert.clone() };
                            buf.replace_span((row, start), (row, end), &text);
                            buf.set_cursor(row, start + text.chars().count());
                            inserted.push_str(&text);
                        }
                    }
                    cursors[0] = buf.cursor();
                }
            }
            // Opening it: only with no snippet live, since Ctrl-N is
            // that feature's own "next placeholder" while one is.
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
            // --- a live `abbr` snippet owns these eight keys, exactly as
            // it does at the shell prompt (editor.rs) ------------------
            // Tab advances rather than indenting, Ctrl-E cancels back to
            // the abbreviation name, Ctrl-Y accepts, and Enter advances
            // except on the last placeholder in visit order, where it
            // accepts instead of inserting a newline.
            // Each of these resyncs `cursors[0]` from the buffer on the
            // way out: the snippet moves the real cursor through its own
            // model, and this function's other arms all insert at
            // `cursors[0]` -- leaving it behind is how a keystroke right
            // after an accept lands back where the abbreviation started
            // (found via pty, and exactly what the invariant every other
            // arm here already maintains exists to prevent).
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
            // Only ever eats what was typed into the active placeholder;
            // with nothing left in it, it stops rather than chewing into
            // the snippet's own literal text, which the model could not
            // put back.
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
            // Enter is the first of the two abbreviation triggers, same
            // as at the shell prompt: it expands rather than inserting a
            // newline, and a second Enter (now finding nothing to expand)
            // does the newline. Never in Replace mode, whose whole
            // contract is that the line's length doesn't change --
            // splicing an expansion in would break exactly that.
            // An `abbr` never expands out of pasted text: a word in
            // someone else's code that happens to match one of yours is
            // not an abbreviation you typed.
            Key::Enter if !replace && !pasting && expand_abbr(buf, &abbrs, &mut live) => {
                cursors[0] = buf.cursor();
            }
            Key::Enter => {
                // Autoindent, unless these characters were pasted --
                // a paste already carries its own indentation, and
                // adding to it is the staircase bracketed paste exists
                // to prevent.
                let indent = if pasting { String::new() } else { autoindent_for(buf, buf.cursor().0) };
                let text = format!("\n{indent}");
                apply_insert_to_all(buf, &mut cursors, &text);
                buf.set_cursor(cursors[0].0, cursors[0].1);
                inserted.push_str(&text);
            }
            // With `expandtab` on, spaces up to the next `shiftwidth`
            // boundary; with it off, one literal tab. Never overtypes
            // even in Replace mode, same as Enter just above.
            Key::Tab => {
                let text = if buf.expandtab {
                    let (line, col) = buf.cursor();
                    // To the next boundary in *drawn columns*, so a tab
                    // pressed after a literal tab lands where it looks
                    // like it should.
                    let at = display_column(buf, line, col);
                    " ".repeat(buf.shiftwidth - (at % buf.shiftwidth))
                } else {
                    "\t".to_string()
                };
                apply_insert_to_all(buf, &mut cursors, &text);
                buf.set_cursor(cursors[0].0, cursors[0].1);
                inserted.push_str(&text);
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
            // With a selection standing, Backspace deletes *it* and
            // leaves the caret where it was -- which is what Backspace
            // means everywhere else a selection can exist, and the one
            // gesture that makes selecting in Insert mode worth
            // anything.
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
            // Delete: Backspace's forward twin, and absent for the same
            // reason Home/End were -- the shell prompt has had it
            // (editor.rs's own `Key::Delete` arm) and this loop simply
            // had no arm, so the key did nothing at all. Selection first,
            // exactly as Backspace does, because a standing selection is
            // what either key means when there is one.
            //
            // `inserted` (the `.`-repeat accumulator) is deliberately
            // left alone: what Delete removes is *ahead* of the cursor
            // and so was never part of what this session typed. Same
            // best-effort acknowledgement `Key::CtrlW` already makes.
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
            // Ctrl-W: delete the word before the cursor -- real vim's own
            // Insert-mode convention (`:help i_CTRL-W`), same idea
            // editor::LineEditor::kill_word_backward already gives the
            // plain shell prompt. Reuses `Motion::WordBackward` +
            // `motion_range` -- the exact same primitive Normal mode's
            // own `db` operator is built on -- rather than a second,
            // bespoke word-boundary implementation, so this crosses a
            // line boundary backward exactly when `db`/plain `b` already
            // do. Scoped to the primary cursor only, matching this
            // function's existing "never combined with extra_cursors in
            // practice" convention for Replace mode's own Backspace/Char
            // arms above -- multi-selection `c` never needs a whole-word
            // delete mid-insert. `inserted` (the `.`-repeat accumulator)
            // is deliberately left untouched, same "best-effort, not
            // exact" acknowledgment this function's own doc comment
            // already makes for plain Backspace -- Ctrl-W can delete text
            // that predates this Insert-mode session, which there's no
            // clean way to un-accumulate.
            Key::CtrlW => {
                if let Some(range) = motion::motion_range(buf, motion::Motion::WordBackward, None) {
                    buf.delete_range(&range);
                    cursors[0] = buf.cursor();
                }
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
            // Home/End/Delete: keys the terminal sends that Insert mode
            // simply had no arm for, so they did nothing at all -- while
            // Normal mode has had all three (vimkeys.rs binds Home/End to
            // LineStart/LineEnd) and so has the shell prompt
            // (editor.rs's own Home/End/Delete arms). The asymmetry was
            // the bug: the one mode where a person reaches for Home is
            // the one that ignored it.
            //
            // `Home` is column 0, not first-non-blank -- vim's own
            // `<Home>`, with `^` still the other thing.
            // The paste brackets themselves insert nothing -- they only
            // say what the characters between them are.
            Key::PasteStart => pasting = true,
            Key::PasteEnd => pasting = false,
            Key::Home => {
                motion::apply_motion(buf, motion::Motion::LineStart, None);
                cursors[0] = buf.cursor();
            }
            // `Motion::LineEnd` clamps to the last real character, which
            // is Normal mode's meaning; Insert mode's cursor is allowed
            // one column past it (where the next typed char lands), so
            // this goes there directly -- the same reasoning `Key::Right`
            // just above already makes.
            Key::End => {
                let (row, _) = buf.cursor();
                buf.set_cursor(row, buf.line_len(row));
                cursors[0] = buf.cursor();
            }
            // Alt-Left/Alt-Right: real vim's own `b`/`w` word motions,
            // available in Insert mode too (not just Normal) -- the
            // conventional word-navigation binding most editors give
            // these two keys. `editor::read_line`'s own Alt-Left/Right
            // (the plain shell prompt) keeps its separate, pre-existing
            // meaning -- directory history browsing, only at an empty
            // buffer -- unrelated to this buffer-editing context and
            // deliberately left alone.
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
            // A click puts the caret where you clicked and drops any
            // selection, the same as clicking in anything else does.
            Key::Mouse(ev) if buf.mouse && ev.is_left_click() => {
                let (row0, col0) = ((ev.row as usize).saturating_sub(1), (ev.col as usize).saturating_sub(1));
                if let Some((line, col)) = position_at_screen(buf, rect, row0, col0) {
                    buf.selections.clear();
                    buf.set_cursor(line, col);
                    // Collapses any extra cursors: clicking says where
                    // you want to be, which is one place.
                    cursors = vec![(line, col)];
                    drag_anchor = Some((line, col));
                }
            }
            // ...and dragging from it selects, without leaving Insert
            // mode. `Inclusive`, so it renders and deletes exactly the
            // way a Visual selection already does -- there is one
            // selection concept in this buffer, not two.
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
            Key::Mouse(ev) if ev.is_scroll_down() => {
                motion::apply_motion(buf, motion::Motion::ScrollLineDown, Some(MOUSE_WHEEL_LINES));
                cursors[0] = buf.cursor();
            }
            Key::Mouse(ev) if ev.is_scroll_up() => {
                motion::apply_motion(buf, motion::Motion::ScrollLineUp, Some(MOUSE_WHEEL_LINES));
                cursors[0] = buf.cursor();
            }
            // The horizontal wheel, where the terminal sends one (see
            // MouseEvent::is_scroll_left). Unlike the vertical pair this
            // does move the cursor -- it has to, since a caret parked
            // off-screen is not somewhere you can type.
            Key::Mouse(ev) if ev.is_scroll_left() || ev.is_scroll_right() => {
                let columns = if ev.is_scroll_right() { MOUSE_WHEEL_COLUMNS as isize } else { -(MOUSE_WHEEL_COLUMNS as isize) };
                scroll_horizontally(buf, columns, editor_content_cols(buf, rect));
                cursors[0] = buf.cursor();
            }
            // Space is the other abbreviation trigger. Unlike a plain
            // expansion (where the space that ended the word is still
            // inserted right after it, matching fish), a *snippet*
            // swallows it: the caret is already parked inside the first
            // placeholder, where a space would be the first thing typed
            // into it rather than anything that ended the abbreviation.
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
                // Typing drops the selection rather than replacing it.
                // Deliberately not the GUI behaviour: this is still a
                // vim buffer, `u` is the only way back, and silently
                // eating a swept region on the next keystroke is the
                // kind of thing you only notice afterwards. Backspace
                // is the key that deletes it, and says so.
                buf.selections.clear();
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
        // Kept in step with the model on every iteration rather than at
        // each of the arms above: the buffer carries these purely so
        // whoever draws it can mark the placeholders (see
        // TextBuffer::snippet_holes), and one place to write them is one
        // place for them to go stale.
        buf.snippet_holes = live.as_ref().map(|live| snippet_holes(live, buf)).unwrap_or_default();
        // Narrowed after the key has been applied, not before: what
        // matters is the word as it stands now, and every arm above
        // that changes it has already run.
        if let Some(state) = completing.as_mut()
            && !state.refilter(buf)
        {
            completing = None;
        }
        scroll_to_show_cursor(buf, editor_content_cols(buf, rect));
        render_editor_frame(buf, vk, mode, rect, term_rows, term_cols, color_overrides);
        if let Some(state) = completing.as_ref() {
            let (row, col) = buf.cursor();
            let chars = buf.line_chars(row);
            let gutter = rect.cols.saturating_sub(editor_content_cols(buf, rect));
            let screen_row = rect.row + row.saturating_sub(buf.viewport_top());
            let screen_col = rect.col + gutter + crate::bishedit::unicode_width::col_of(&chars, col).saturating_sub(buf.viewport_left());
            print!("{}", render_completion_popup(&state.items(), state.selected, screen_row, screen_col, rect));
            let _ = io::stdout().flush();
        }
    }
}

// A live snippet's placeholders in the buffer's own (line, column)
// space, for the renderer.
fn snippet_holes(live: &LiveSnippet, buf: &TextBuffer) -> Vec<textbuffer::SnippetHole> {
    live.holes()
        .into_iter()
        // A hole that spans a line break is drawn on the line it starts
        // on, out to the end of what is there: the renderer marks spans
        // within one line, and a tabstop whose *default* contains a
        // newline is rare enough not to be worth a second shape.
        .map(|(start, end, active)| textbuffer::SnippetHole {
            line: start.0,
            start: start.1,
            end: if end.0 == start.0 { end.1 } else { buf.line_len(start.0) },
            active,
        })
        .collect()
}

// The file editor's own half of `abbr` expansion -- editor.rs's
// `expand_abbr_at_cursor` for a `TextBuffer` instead of a prompt line.
//
// One deliberate difference from the shell prompt's version: no
// command-position gate. That gate exists because an abbreviation typed
// as an *argument* to a real command shouldn't fire; a file has no
// command positions at all, and running bash's own word-role classifier
// over, say, a Rust file would be meaningless. So here an abbreviation
// expands wherever its name is the word ending at the cursor -- which is
// also how snippets work in every editor that has them.
//
// `true` if something expanded, which for Enter is also the caller's cue
// not to insert a newline as well.
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
            // `flatten` rather than the raw text: an expansion with no
            // *tabstops* can still carry `\$` or `${1}`-shaped noise,
            // and what goes in is what a finished snippet would have
            // left behind.
            let text = snippet::flatten(&abbr.expansion);
            buf.replace_span((row, word_start), (row, col), &text);
            buf.set_cursor(row, word_start + text.chars().count());
        }
    }
    true
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

// Delete, replicated across every tracked cursor -- apply_backspace_to_
// all's forward twin, and processed furthest-first for the same reason.
//
// The cursor does not move: what goes is the character *after* it, and
// at the end of a line that character is the newline, so the next line
// joins onto this one. A cursor at the very end of the buffer is left
// alone, matching the prompt's own `delete_forward` doing nothing there.
fn apply_delete_to_all(buf: &mut TextBuffer, cursors: &mut [(usize, usize)]) {
    let mut order: Vec<usize> = (0..cursors.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(cursors[i]));
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
        Some((RegisterShape::Block, _)) => "-- VISUAL BLOCK --",
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
    let mut right = format!("{},{}  {}/{}", line + 1, col + 1, line + 1, total);

    // What the language server is busy with, between the mode indicator
    // and the cursor position. On the right because that is where it is
    // out of the way of a `:` line being typed, and dropped outright
    // rather than truncated when the pane is too narrow for both it and
    // the position -- knowing where the cursor is matters more, every
    // time, than knowing a server is indexing.
    // A message the server asked to have shown wins the slot: it is
    // news, where progress is a state, and it is gone by the next
    // keypress either way.
    if let Some(progress) = buf.lsp_message.as_ref().or(buf.lsp_progress.as_ref()) {
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
    // A block covers the same columns on every row it spans, which is
    // what makes it a rectangle rather than a run -- so neither end
    // depends on which row this is.
    if let Some((left, right)) = motion::block_columns(range) {
        let start = left.saturating_sub(start_char);
        let end = (right + 1).saturating_sub(start_char).min(cols);
        return (end > start).then_some((start, end));
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

// `first` is whether this screen row is the *first* one of its buffer
// line. Only that row gets the line number, the diff marker and the
// rest: repeating them down a wrapped line would claim there are
// several lines where there is one.
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

// This buffer's column layout, or `None` when it has no tabular form --
// which covers almost every buffer, and every buffer while wrapping is
// on (the two are different answers to "this line is wider than the
// pane", and running both at once would align columns that have been
// broken across rows).
//
// Measured across the whole file, because a column that changed width
// as you scrolled would be worse than no alignment at all. That is a
// pass over every line per redraw -- and for markdown a whole parse of
// the document on top, though that is the same parse
// `buffer_highlight_spans` already runs for the same buffer on the same
// redraw. Like that one, worth caching only once someone can feel it.
pub(crate) fn tabular_layout(buf: &TextBuffer) -> Option<crate::bishedit::tabular::Layout> {
    let style = buf.tabular.filter(|_| !buf.wrap.wrap)?;
    let lines: Vec<Vec<char>> = (0..buf.line_count()).map(|l| buf.line_chars(l)).collect();
    let lines: Vec<&[char]> = lines.iter().map(|l| l.as_slice()).collect();
    match table_regions(buf) {
        Some(regions) => Some(crate::bishedit::tabular::measure_regions(&lines, style, &regions)),
        None => Some(crate::bishedit::tabular::measure(&lines, style)),
    }
}

// Which lines of a markdown buffer are table rows, as line ranges.
// `None` for every other language, meaning "the whole file is one
// table" -- true of a `.csv`, where there is nothing else in the file.
//
// A markdown document is the other shape: most of it is prose that must
// not be touched, and two tables in it are unrelated and must not be
// aligned to each other. Asking the real parser rather than
// pattern-matching lines for pipes is what keeps a pipe inside a fenced
// code block, or in a line of prose, from being mistaken for a column.
fn table_regions(buf: &TextBuffer) -> Option<Vec<std::ops::Range<usize>>> {
    if language_of(buf) != "markdown" {
        return None;
    }
    let text = buf.text();
    let doc = crate::markdown::parse(&text);
    let mut spans = Vec::new();
    collect_table_spans(&doc.blocks, &mut spans);

    // Byte offsets to line numbers, in one pass over the starts.
    let mut starts = vec![0usize];
    starts.extend(text.char_indices().filter(|(_, c)| *c == '\n').map(|(i, _)| i + 1));
    let line_of = |offset: usize| starts.partition_point(|s| *s <= offset).saturating_sub(1);
    Some(spans.into_iter().map(|s| line_of(s.start)..line_of(s.end.saturating_sub(1)) + 1).collect())
}

// Tables can sit inside a block quote or a list item, so this walks the
// whole tree rather than only the top level.
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

// One line as it will be drawn: its own characters plus whatever padding
// lines its columns up. Without a layout this is the line itself, which
// is what lets everything below run one code path.
pub(crate) fn display_row(
    buf: &TextBuffer,
    line: usize,
    layout: Option<&crate::bishedit::tabular::Layout>,
) -> crate::bishedit::tabular::Row {
    let chars = buf.line_chars(line);
    let row = match layout {
        Some(layout) => crate::bishedit::tabular::row(&chars, line, layout),
        None => crate::bishedit::tabular::Row::plain(&chars),
    };
    expand_tabs(row, buf.tabstop)
}

// A literal tab draws as spaces to the next `tabstop` boundary, and
// this is where that happens -- on the *drawn cells*, after the tabular
// layout and before anything reads them.
//
// That placement is the whole design: the cursor, click-to-position,
// horizontal scrolling, selections, syntax highlighting and diagnostics
// all already address the line through `Row`'s own two maps rather than
// through character indices (see `render_row`), so expanding a tab here
// makes every one of them tab-aware at once. Doing it in the renderer
// instead would leave each of those to work it out separately, which is
// exactly the drift the drawn-cell model exists to prevent.
//
// Each of a tab's spaces points back at the tab itself, so a click
// anywhere in it lands on the one character that is really there.
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
    // Rebuilt rather than shifted: a tab's spaces all name the same
    // source character, and the first of them is the one it maps to.
    let mut cell_of = vec![cells.len(); row.cell_of.len()];
    for (cell, source) in source_at.iter().enumerate().rev() {
        if let Some(source) = source
            && *source < cell_of.len()
        {
            cell_of[*source] = cell;
        }
    }
    // Anything with no cell of its own (past the end) points one past
    // it, and the maps stay monotonic.
    let mut last = cells.len();
    for slot in cell_of.iter_mut().rev() {
        if *slot > last {
            *slot = last;
        }
        last = *slot;
    }
    crate::bishedit::tabular::Row { cells, source_at, cell_of }
}

// The drawn column a character index sits at -- what Tab advances from
// and what `scroll_to_show_cursor` measures the margin against.
fn display_column(buf: &TextBuffer, line: usize, col: usize) -> usize {
    let display = display_row(buf, line, tabular_layout(buf).as_ref());
    col_at_cell(&display.cells, display.cell_of[col.min(buf.line_len(line))])
}

// Which character of the line a drawn cell belongs to. Padding belongs
// to the character it was inserted after, so a cursor placed on it
// lands somewhere real.
fn source_at_or_before(display: &crate::bishedit::tabular::Row, cell: usize) -> usize {
    let last = display.cells.len();
    for i in (0..=cell.min(last)).rev() {
        if let Some(Some(source)) = display.source_at.get(i) {
            return *source;
        }
    }
    0
}

// `char_at_col`/`col_of`, over drawn cells rather than the line's own
// characters. The two stop being the same thing the moment any padding
// is inserted, exactly as they already differ for a wide glyph.
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
// being drawn. Everything composed onto a row goes through this, so
// padding shifts a highlight exactly as far as it shifts the text under
// it. `None` when the span falls entirely outside the window.
fn to_window(
    display: &crate::bishedit::tabular::Row,
    start_cell: usize,
    avail: usize,
    start: usize,
    end: usize,
) -> Option<(usize, usize)> {
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
// opens that line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisualRow {
    pub(crate) line: usize,
    pub(crate) first: bool,
    pub(crate) seg: crate::bishedit::wrap::Segment,
}

// How a buffer line is broken across screen rows. With wrapping off
// this is always a single segment covering the whole line, and every
// caller below then applies `viewport_left` to it exactly as before --
// which is how one code path serves both modes.
pub(crate) fn line_segments(buf: &TextBuffer, line: usize, content_cols: usize) -> Vec<crate::bishedit::wrap::Segment> {
    crate::bishedit::wrap::segments(&buf.line_chars(line), content_cols, &buf.wrap)
}

// The rows a pane shows, from the buffer's own scroll position. Shared
// by rendering, by the click-to-position inverse, and by scrolling, so
// none of the three can disagree about what is on screen.
pub(crate) fn visible_rows(buf: &TextBuffer, content_cols: usize, content_rows: usize) -> Vec<VisualRow> {
    let mut rows = Vec::with_capacity(content_rows);
    let mut line = buf.viewport_top();
    let mut skip = buf.viewport_sub();
    while rows.len() < content_rows && line < buf.line_count() {
        for (i, seg) in line_segments(buf, line, content_cols).into_iter().enumerate() {
            if i < skip {
                continue;
            }
            if rows.len() >= content_rows {
                break;
            }
            rows.push(VisualRow { line, first: i == 0, seg });
        }
        skip = 0;
        line += 1;
    }
    rows
}

// What a continued row opens with: `showbreak`, then padding out to the
// indent the layout reserved for it. Dim, because it is the editor
// talking rather than the file.
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
    let author_width =
        blame.iter().flatten().map(|b| b.author.chars().count()).max().unwrap_or(0).clamp(1, BLAME_AUTHOR_MAX_WIDTH);
    8 + 1 + 10 + 1 + author_width + 1
}

// What a line git had nothing to say about shows instead of a commit --
// see TextBuffer::blame's own doc comment for when that happens. Marked
// rather than left blank, and in its own colour rather than the dim grey
// every real blame row uses, so "this line isn't in what you asked about"
// reads at a glance instead of looking like a rendering gap.
const BLAME_UNKNOWN: &str = "· N/A";

fn render_blame_cell(buf: &TextBuffer, _starts: &[usize], line: usize, width: usize) -> Option<String> {
    let blame = buf.blame.as_ref()?;
    let author_width = width.saturating_sub(8 + 1 + 10 + 1 + 1);
    match blame.get(line)? {
        Some(entry) => {
            let author: String = entry.author.chars().take(author_width).collect();
            Some(format!("\x1b[2m{} {} {:<aw$} \x1b[0m", entry.short_commit, entry.date, author, aw = author_width))
        }
        None => Some(format!("\x1b[33m{:<8}\x1b[0m {:<10} {:<aw$} ", BLAME_UNKNOWN, "", "", aw = author_width)),
    }
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

// The worst severity among the diagnostics whose own char-offset range
// (whole-buffer text offsets, same convention buffer_highlight_spans's
// own spans use) intersects this one line's span -- same predicate
// diagnostic_spans_for_line uses per-column below, just reduced to one
// answer for the coarser gutter marker, which has room for a single
// mark however many findings the line actually holds. `max` is the
// reduction because `lint::Severity` is declared least-severe-first
// specifically so it can be (see its own doc comment).
fn line_severity(buf: &TextBuffer, starts: &[usize], line: usize) -> Option<lint::Severity> {
    let line_start = starts[line];
    let line_end = line_start + buf.line_len(line);
    buf.diagnostics.iter().filter(|d| d.start < line_end && d.end > line_start).map(|d| d.severity).max()
}

fn render_diagnostic_cell(buf: &TextBuffer, starts: &[usize], line: usize, _width: usize) -> Option<String> {
    if line >= buf.line_count() {
        return None;
    }
    let severity = line_severity(buf, starts, line)?;
    // The gutter bullet takes the severity's own colour but not its
    // underline -- underlining a bullet would just look like a typo.
    let (fg, _) = crate::theme::resolve(severity_element(severity), buf.colors.as_ref());
    let sgr = vt100::sgr_codes(fg, vt100::Color::Default, vt100::CellAttrs::default());
    Some(format!("{sgr}\u{25cf}\x1b[0m "))
}

// Vim's own gutter-width convention: as many digits as the buffer's last
// line number needs, plus one trailing space of padding before the
// buffer's own content starts. Grows dynamically as the buffer gains
// lines (matching vim), rather than reserving a fixed width up front.
fn line_number_width(buf: &TextBuffer) -> usize {
    // Sized for the widest number this column can ever draw, not for
    // whatever it happens to be drawing now: with `relativenumber` the
    // distances are small but the cursor's own line still shows its
    // absolute number, so the column can't shrink to fit the offsets.
    // A gutter that changed width as the cursor moved would shift every
    // line of the pane sideways on every `j`.
    buf.line_count().to_string().len() + 1
}

fn render_line_number_cell(buf: &TextBuffer, _starts: &[usize], line: usize, width: usize) -> Option<String> {
    if line >= buf.line_count() {
        return None;
    }
    let sgr = crate::theme::sgr(crate::theme::Ui::LineNumber, buf.colors.as_ref());
    Some(format!("{sgr}{:>pad$} \x1b[0m", line_number_text(buf, line), pad = width.saturating_sub(1)))
}

// What the gutter puts on this line: its own number, or -- with
// `relativenumber` on -- how far it is from the cursor's, which is what
// makes `12j` or `d8k` something you read off the screen instead of
// counting. The cursor's own line keeps its absolute number either way:
// a `0` there would be the one number on screen that is not a motion
// count, and vim only shows one because it lets you turn `number` off,
// which bish does not.
fn line_number_text(buf: &TextBuffer, line: usize) -> String {
    let (cursor, _) = buf.cursor();
    if !buf.relativenumber || line == cursor {
        return (line + 1).to_string();
    }
    line.abs_diff(cursor).to_string()
}

// Language detection, v1: a bare extension check, not a content sniff --
// no shebang or content fallback yet, which is a natural follow-up.
//
// A language *name*, not a file-type enum, because `abbr --lang=` is a
// glob written by hand against it (`--lang=rust`, `--lang='*script'`,
// `--lang='!(bash)'`) -- an enum could only ever offer the handful of
// languages this crate happens to know about, while a name lets
// `--lang=toml` work with nothing here having heard of TOML.
//
// The table below is therefore only for extensions whose name genuinely
// *differs* from the language's; everything else is its own lowercased
// extension. A buffer with no file at all -- a fresh `e` with no
// argument -- is "text", which is also a language you can write
// abbreviations for.
const LANGUAGE_BY_EXTENSION: &[(&str, &str)] = &[
    ("sh", "bash"),
    ("bash", "bash"),
    ("rs", "rust"),
    ("py", "python"),
    ("rb", "ruby"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("ts", "typescript"),
    ("md", "markdown"),
    ("yml", "yaml"),
    ("kt", "kotlin"),
    ("hs", "haskell"),
    ("ex", "elixir"),
    ("exs", "elixir"),
    // Man page sections. `ls.1`, `printf.3`, `crontab.5` are all roff
    // source, and `language_of` looks through a `.gz` first, so a page
    // straight out of /usr/share/man lands here too.
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
    // The INI family. `.conf` is the loosest of these -- plenty of
    // `.conf` files (nginx's, apache's) are nothing of the sort -- but
    // it is by far the most common extension people actually put INI
    // in, and the cost of being wrong is that a brace-and-semicolon
    // config gets its words coloured as keys.
    ("ini", "ini"),
    ("cfg", "ini"),
    ("conf", "ini"),
    ("desktop", "ini"),
    // systemd units, which are INI with a stricter dialect. Several of
    // these are generic-looking words (`.path`, `.link`, `.target`) and
    // a file with one of those names that *isn't* a unit will be read
    // as INI; nothing but highlighting depends on this, and in practice
    // nothing else claims them.
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
    // JSON with comments. `.json` itself stays strict -- most `.json`
    // files really are, and colouring a stray `//` as a comment in one
    // that isn't would be saying it is valid there.
    ("jsonc", "jsonc"),
    // `local.env`, `production.env` -- the other way people name these.
    ("env", "dotenv"),
];

// The other half of recognizing a file: what it is *called*. Every entry
// above is an extension, and the most-used config files in the INI
// family don't have one -- `.gitconfig`, `.editorconfig` and `.npmrc`
// are all extension-less as far as `Path` is concerned (a leading dot
// makes the whole name the stem). Matched case-insensitively against
// the file name, ahead of the extension table.
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
    // TOML that doesn't say so. `Pipfile` and `poetry.lock` are both
    // TOML documents with names that give no hint of it.
    ("pipfile", "toml"),
    ("poetry.lock", "toml"),
    // The files everyone knows are JSON-with-comments even though their
    // extension says otherwise. TypeScript's own parser accepts
    // comments here, and every real `tsconfig.json` has them.
    ("tsconfig.json", "jsonc"),
    ("jsconfig.json", "jsonc"),
    ("devcontainer.json", "jsonc"),
    (".eslintrc.json", "jsonc"),
    // `.envrc` is direnv's, and direnv scripts are bash -- it sits in
    // this table specifically so the `.env` prefix rule below can't
    // claim it.
    (".envrc", "bash"),
    // python-dotenv reads this one by name. Here rather than in the
    // extension table because `Path` sees `.flaskenv` as all stem and no
    // suffix, the same as `.gitconfig`.
    (".flaskenv", "dotenv"),
];

pub(crate) fn language_of(buf: &TextBuffer) -> String {
    let Some(path) = buf.path() else { return "text".to_string() };
    // `.gz` says how the bytes are stored, not what they are -- what a
    // `notes.json.gz` buffer holds is JSON, and it should highlight like
    // it. The extension underneath is the answer for every purpose here,
    // so the compression suffix is simply stepped over.
    let path = match path.extension() {
        Some(ext) if ext.eq_ignore_ascii_case("gz") => std::path::Path::new(path.file_stem().unwrap_or(path.as_os_str())),
        _ => path,
    };
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if let Some((_, lang)) = LANGUAGE_BY_FILE_NAME.iter().find(|(n, _)| *n == name) {
        return (*lang).to_string();
    }
    // Some names only mean something in the directory they are in.
    let parent = path.parent().and_then(|p| p.file_name()).map(|d| d.to_string_lossy().to_lowercase());
    match parent.as_deref() {
        // `config` alone is far too common a name to claim, but inside a
        // `.git` directory (or the `git` one under `~/.config`) it is
        // the git config -- the file you are actually editing when you
        // type `e .git/config`.
        Some(".git") | Some("git") if name == "config" => return "ini".to_string(),
        // ...and inside `.cargo` it is cargo's, which is TOML. (Cargo
        // itself now prefers `config.toml`, which needs no help.)
        Some(".cargo") if name == "config" => return "toml".to_string(),
        // Every `.json` VS Code keeps in a project's `.vscode` accepts
        // comments, and the ones people hand-edit are full of them.
        Some(".vscode") if name.ends_with(".json") => return "jsonc".to_string(),
        _ => {}
    }
    // `tsconfig.json` is never alone for long: a real project grows
    // `tsconfig.app.json`, `tsconfig.node.json`, `tsconfig.build.json`.
    // All of them are the same format, so match the family rather than
    // listing the members.
    if (name.starts_with("tsconfig.") || name.starts_with("jsconfig.")) && name.ends_with(".json") {
        return "jsonc".to_string();
    }
    // The same argument, and a bigger family: `.env`, `.env.local`,
    // `.env.production`, `.env.test.local`, `.env.example`. Listing
    // them would be listing a convention rather than matching it.
    // `.envrc` is already spoken for above, since direnv's is a shell
    // script rather than one of these.
    if name == ".env" || name.starts_with(".env.") {
        return "dotenv".to_string();
    }
    let Some(ext) = path.extension() else {
        return "text".to_string();
    };
    let ext = ext.to_string_lossy().to_lowercase();
    LANGUAGE_BY_EXTENSION
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, lang)| (*lang).to_string())
        .unwrap_or(ext)
}

// The buffer's own text, lines joined by '\n' -- what a highlighter
// needs to see a construct that spans several physical lines (a bash
// heredoc body or multi-line double-quoted string, a JSON object) as
// the single thing it actually is, instead of each line's own run
// finding a dangling, unterminated piece of it with no idea what line
// came before. Recomputed on every redraw, same as buffer_highlight_spans
// below -- see that function's own doc comment on why that's an
// accepted, not-yet-a-problem cost rather than something cached here.
pub(crate) fn buffer_source(buf: &TextBuffer) -> String {
    buffer_text(buf)
}

pub(crate) fn buffer_text(buf: &TextBuffer) -> String {
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

// Which highlighter (if any) this buffer's language has. The table
// itself lives in bishedit::highlight, because a fenced code block
// inside a markdown document asks the same question about its info
// string and the two must not answer differently. `None` for a language
// with no highlighter, which renders exactly as it did before any of
// this existed: plain, uncoloured text.
fn highlighter_for(language: &str) -> Option<Box<dyn Highlighter>> {
    highlight::highlighter_for_language(language)
}

// Runs this buffer's own highlighter once against the *whole* buffer
// text (see buffer_text's own doc comment for why that, not one line at
// a time, is what actually fixes a multi-line construct).
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
// here has a live Shell to pull those from -- bash's own Flag/
// Subcommand/Link/InvalidCommand refinements that need them simply
// don't fire, same as there; JsonHighlighter ignores the context
// outright, having no use for any of it. `color_overrides` (`::bish hl`'s own palette -- see bishedit::
// highlight::ColorOverrides/SYN_COL_OPTIONS) is a separate, later step
// (picking a *color* for an already-classified span, not classifying
// it), threaded in from wherever a live Shell to read it from actually
// exists: repl.rs's own run_edit_frame, one level up from every caller
// of this whole chain.
//
// Not a full fix for every multi-line case in *bash*: `next_span`'s own doc
// comment (this same module) documents a pre-existing lexer position-
// tracking gap for a heredoc body that itself contains a $VAR/$(...)
// expansion -- content *after* such a heredoc in the same buffer can
// still come out mis-highlighted. Narrower and pre-existing either way,
// not something this change introduces or could fix without touching
// lexer.rs's own heredoc-body capture.
// The style layer on its own. Only the tests want it that way: the
// render path needs both layers and takes them together, because they
// come out of the same pass (see buffer_spans).
#[cfg(test)]
fn buffer_highlight_spans(buf: &TextBuffer, color_overrides: Option<&highlight::ColorOverrides>) -> Vec<StyledSpan> {
    buffer_spans(buf, color_overrides).0
}

// Both layers this buffer needs, from one run of the highlighter --
// they come out of the same spans, and running the highlighter twice per
// redraw to separate them would double the one real cost here.
//
// The link layer is where `HighlightSpan.link` finally goes somewhere:
// it has always been populated (bash's resolved file paths, markdown's
// link destinations) and has never been read. On top of those, every
// buffer gets a pass for bare URLs in its text -- a URL in a comment, in
// a `.env` value, in a commit message -- which is language-independent
// because being a URL is.
fn buffer_spans(
    buf: &TextBuffer,
    color_overrides: Option<&highlight::ColorOverrides>,
) -> (Vec<StyledSpan>, Vec<highlight::LinkSpan>) {
    let text = buffer_text(buf);
    let mut spans = match highlighter_for(&language_of(buf)) {
        Some(highlighter) => highlighter.highlight(&text, HighlightContext::default()),
        None => Vec::new(),
    };
    // Appended after the language's own spans so it paints over them:
    // a URL inside a string is a URL, and the underline that says so is
    // the only cue that it can be clicked.
    for found in crate::url::find(&text) {
        let url: String = text.chars().skip(found.start).take(found.end - found.start).collect();
        spans.push(highlight::HighlightSpan {
            start: found.start,
            end: found.end,
            kind: highlight::HighlightKind::Link,
            link: Some(url),
        });
    }
    let links = spans
        .iter()
        .filter_map(|s| Some(highlight::LinkSpan { start: s.start, end: s.end, url: absolute_link(buf, s.link.as_deref()?)? }))
        .collect();
    let mut styled: Vec<StyledSpan> = spans
        .into_iter()
        .map(|s| {
            let (fg, attrs) = highlight::resolve_style(s.kind, color_overrides);
            StyledSpan { start: s.start, end: s.end, fg, attrs }
        })
        .collect();
    // Last, so it paints over the lexer's guesses: a language server
    // knows which identifier is a parameter and which a constant, which
    // is the whole reason to ask. Only the tokens that resolved to a
    // colour are here at all (see repl::sync_semantic_tokens), so
    // everything the server named and bish has no colour for keeps
    // whatever the local highlighter made of it.
    styled.extend(buf.semantic_spans.iter().cloned());
    (styled, links)
}

// This buffer's own directory is what a relative link target is
// relative to; an unnamed buffer has none, and `url::absolute` gives no
// link rather than a guessed one.
fn absolute_link(buf: &TextBuffer, target: &str) -> Option<String> {
    crate::url::absolute(target, buf.path().and_then(|p| p.parent()))
}

// `:diag`'s own worker (see repl.rs's run_command_mode, the one caller):
// runs every diagnose tool configured for this buffer's language against
// the *whole* buffer text, for the same reason buffer_highlight_spans
// does (a multi-line construct needs to be seen as one thing). Which
// linters those are is a plain match on `language_of`: there is more
// than one language with something to say now, and the `Linter` trait
// was always shaped for that (concatenation is the default answer), it
// just never had a second one to prove it. Unlike
// buffer_highlight_spans this isn't
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
    // Which of bish's own linters have anything to say about this
    // buffer. A `Vec<&dyn Linter>` rather than a fixed-size array
    // because the answer now depends on the language -- which is the
    // shape this was always written for (see the `Linter` trait's own
    // "concatenation is the default answer" note), it just never had a
    // second language to prove it.
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

// `diagnostic_position`'s inverse: a `(line, character)` as a language
// server counts it, back to the flat char offset every diagnostic and
// highlight span in this codebase is addressed by.
//
// Two conversions in one, and both matter. The column is in whatever
// encoding the handshake settled on (`utf-32` is bish's own counting
// and needs no work; `utf-16` differs on any line holding an emoji),
// and the line has to be turned into an offset via the same
// `line_starts` prefix sum everything else here uses.
//
// Clamps rather than fails at every step. A server can and does name a
// position past the end of what bish currently holds -- it is answering
// about a revision the buffer may have moved past, and the alternative
// to clamping is discarding a finding that is very probably still
// pointing at the right place.
pub(crate) fn diagnostic_offset(buf: &TextBuffer, starts: &[usize], line: usize, character: usize, encoding: lsp::PositionEncoding) -> usize {
    let last = buf.line_count().saturating_sub(1);
    if line > last {
        return starts[last] + buf.line_len(last);
    }
    let chars = buf.line_chars(line);
    starts[line] + lsp::from_server_column(&chars, character, encoding)
}

// Applies a batch of `lsp::TextEdit`s to a buffer, returning how many
// actually changed anything.
//
// **Applied last-first.** LSP says a batch's ranges never overlap and
// must be applied as though simultaneously, which for a buffer that
// edits in place means starting from the end: an earlier edit that
// shifted everything after it would invalidate every later range.
// Exactly the rule `tool.rs`'s own `apply_fixes` already follows for a
// batch of `lint::Fix`es, one coordinate system over.
//
// Multi-line edits are the norm here, not the exception -- a formatter
// rewriting a block sends one edit spanning it -- so this splices with
// `delete_range` + `insert_text` rather than the single-line
// `replace_in_line` that completion's accept can get away with.
pub(crate) fn apply_text_edits(buf: &mut TextBuffer, edits: &[lsp::TextEdit], encoding: lsp::PositionEncoding) -> usize {
    // One edit with its ends already in this buffer's coordinates.
    struct Resolved<'a> {
        from: (usize, usize),
        to: (usize, usize),
        text: &'a str,
    }
    let starts = line_starts(buf);
    // Resolved against the *current* text before anything moves, which
    // is the whole reason the order below matters.
    let mut resolved: Vec<Resolved<'_>> = edits
        .iter()
        .map(|edit| {
            let from = position_of(buf, &starts, edit.start, encoding);
            let to = position_of(buf, &starts, edit.end, encoding);
            Resolved { from, to: to.max(from), text: edit.text.as_str() }
        })
        .collect();
    resolved.sort_by_key(|edit| std::cmp::Reverse(edit.from));
    let mut applied = 0;
    for Resolved { from, to, text } in resolved {
        let unchanged = from == to && text.is_empty();
        if unchanged {
            continue;
        }
        if from != to {
            buf.delete_range(&crate::bishedit::motion::MotionRange {
                shape: crate::bishedit::motion::MotionShape::Exclusive,
                from,
                to,
            });
        }
        if !text.is_empty() {
            buf.insert_text(from, text);
        }
        applied += 1;
    }
    applied
}

// A server position as this buffer's own `(row, col)`, clamped -- see
// `diagnostic_offset`, which this is the two-dimensional face of.
fn position_of(buf: &TextBuffer, starts: &[usize], position: (usize, usize), encoding: lsp::PositionEncoding) -> (usize, usize) {
    let offset = diagnostic_offset(buf, starts, position.0, position.1, encoding);
    diagnostic_position(buf, offset)
}

// A server's findings as diagnostics this editor can draw. The one
// place LSP's model and `lint::Diagnostic` meet -- which is here, and
// not in lsp.rs, because only something holding the actual text can
// convert a `(line, character)` into an offset.
//
// `fix` is always `None`: an LSP fix is a code action, which is a
// separate request and a multi-range `WorkspaceEdit` that `lint::Fix`'s
// deliberately-single-range shape cannot express. Nothing is silently
// lost by that -- a server's diagnostic carries no edit of its own.
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
            // Always `Some`, even when the server named no source: this
            // is what tells a relayed finding apart from one of bish's
            // own, which is what lets a new publication replace exactly
            // the right subset. The server's command stands in when it
            // didn't say.
            source: Some(f.source.clone().unwrap_or_else(|| "lsp".to_string())),
            message: f.message.clone(),
            fix: None,
        })
        .collect()
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

// Which hooks run before a save, by language -- bash is the only one
// with any tooling behind it today (see `language_of`), so it's the only
// entry with anything in its list. A future language adds its own arm
// here; a future bash-only hook (or one shared across every language,
// e.g. line-ending normalization) just joins this one slice -- no change
// needed to `run_pre_save_hooks` itself either way.
fn pre_save_hooks(language: &str) -> &'static [PreSaveHook] {
    match language {
        snippet::DEFAULT_LANG => &[bash_format_hook],
        _ => &[],
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
    // A read-only buffer's `:w` is either about to be refused, or is a
    // `:w SOMEWHERE-ELSE` writing this content out as it stands --
    // neither is a reason to reformat text the user was told they can't
    // change.
    if buf.is_readonly() {
        return;
    }
    for hook in pre_save_hooks(&language_of(buf)) {
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

// The two things every `:git` gutter needs before it can ask git
// anything: a buffer that has a file at all, and a git to ask.
fn git_path(buf: &TextBuffer) -> Result<std::path::PathBuf, String> {
    let path = buf.path().ok_or_else(|| "no file name".to_string())?.to_path_buf();
    if !crate::git::available() {
        return Err("git executable not found".to_string());
    }
    Ok(path)
}

// `:git blame [REV]`'s own worker (repl.rs's run_command_mode): toggles
// the gutter's blame column off if it's currently on (`Ok(false)`), or
// runs a fresh `git blame` and turns it on (`Ok(true)`) if it's currently
// off -- same "one command, two states" shape as vim's own `:GBlame`-
// style plugins, just built in.
//
// Blame describes some *committed* version of the file, which is not
// necessarily what's on screen: the buffer may have been edited since,
// and with a `REV` it is a different version outright. Rather than refuse
// the first case and mis-align the second, the blame is lined up against
// the buffer by diffing it with the content it was computed from (see
// git::align_to). A line with no counterpart there -- typed just now, or
// simply not present at that revision -- gets `None`, which the gutter
// renders as its own marker rather than leaving blank (see
// render_blame_cell).
pub(crate) fn toggle_git_blame(buf: &mut TextBuffer, rev: Option<&str>) -> Result<bool, String> {
    if buf.blame.is_some() {
        buf.blame = None;
        return Ok(false);
    }
    let path = git_path(buf)?;
    // What the blame that comes back is indexed by. With a REV that's
    // that revision's content; with none it's the working tree -- git
    // blame's own default, and the file on disk rather than anything
    // `git show` would return.
    let old = match rev {
        Some(rev) => crate::git::file_at_rev(&path, Some(rev))?.unwrap_or_default(),
        None => std::fs::read_to_string(&path).map_err(|e| format!("{}: {}", path.display(), e))?,
    };
    let blamed = crate::git::blame(&path, rev)?;
    let old_lines: Vec<&str> = old.lines().collect();
    let current = buf.text();
    let current_lines: Vec<&str> = current.lines().collect();
    let aligned = crate::git::align_to(&old_lines, &current_lines)
        .into_iter()
        .map(|from| from.and_then(|i| blamed.get(i).cloned()))
        .collect::<Vec<_>>();
    buf.blame = Some(aligned);
    Ok(true)
}

// `:git diff [REV]`'s own worker -- same "one command, two states" toggle
// shape as toggle_git_blame just above, and the same reason it needs no
// dirty-buffer refusal either: the comparison is between what git says
// the file held at `rev` and this buffer's own *current* content, so an
// unsaved buffer is simply part of what's being compared rather than
// something that would make the answer wrong. A file that isn't in that
// revision at all diffs against nothing, so every line reads as added --
// which is what `git diff --no-index` against /dev/null said for an
// untracked file before this, just arrived at without the special case.
pub(crate) fn toggle_git_diff(buf: &mut TextBuffer, rev: Option<&str>) -> Result<bool, String> {
    if buf.diff.is_some() {
        buf.diff = None;
        return Ok(false);
    }
    let path = git_path(buf)?;
    let old = crate::git::file_at_rev(&path, rev)?.unwrap_or_default();
    let old_lines: Vec<&str> = old.lines().collect();
    let current = buf.text();
    let current_lines: Vec<&str> = current.lines().collect();
    buf.diff = Some(crate::git::marks_from_diff(&old_lines, &current_lines));
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
// independent gutter column.
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
// its own small function so severity has exactly one place it turns into
// presentation. The match is exhaustive against the real enum rather
// than a wildcard, which is what made growing Severity from its original
// single `Warning` to LSP's four a one-line-per-variant change here
// instead of a search for every place severity might matter.
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

// diagnostic_spans_for_line's own sibling to spans_for_line (see that
// function's own doc comment for the shared shape/reasoning -- same
// whole-buffer-char-offset convention, same per-line clamp) -- reads
// buf.diagnostics instead of a precomputed highlight pass, and resolves
// each one's own color from diagnostic_style instead of a single shared
// kind-to-style mapping (a highlight pass is one Highlighter's own
// output, always styled uniformly by HighlightKind; diagnostics can vary
// per finding once Severity grows past its current one variant).
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

// spans_for_line's own sibling for the link layer -- same whole-buffer
// char offsets, same per-line clamp, so a URL that a wrapped line
// carries across two rows stays one link on both of them.
fn links_for_line(links: &[highlight::LinkSpan], line_start: usize, line_len: usize) -> Vec<highlight::LinkSpan> {
    let line_end = line_start + line_len;
    links
        .iter()
        .filter(|l| l.start < line_end && l.end > line_start)
        .map(|l| highlight::LinkSpan {
            start: l.start.saturating_sub(line_start),
            end: (l.end - line_start).min(line_len),
            url: l.url.clone(),
        })
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
// Draws one screen row: the slice `start_cell..end_cell` of a line's
// drawn cells, bounded by `cols` columns.
//
// Every span handed in is already expressed in this same window, in
// cells (see `to_window`). That is deliberate: with a tabular display a
// character index and a cell index are different numbers, and having
// one place convert between them -- rather than each layer doing its
// own arithmetic here -- is what keeps highlighting attached to the
// text it highlights.
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
    // Attribute-only marks composed *over* whatever colour the layers
    // above resolved to, rather than replacing it -- today, the
    // language server's own "these are the same symbol" answer. See
    // `highlight::compose_attrs`.
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
    // With wrapping off, a line that continues past either edge says so
    // -- vim's `listchars` `extends`/`precedes`, empty by default.
    let truncated_right = at < display.cells.len();
    let truncated_left = start_cell > 0;
    while used < cols {
        chars.push(' ');
        used += 1;
    }

    // Clamped against `chars.len()` (the real array length after the
    // width-bounded walk and padding above), not `cols` (the column
    // *budget* it was bounded to fit) -- the two only coincide when
    // there's no wide char anywhere in this window; whenever there is,
    // `chars.len() < cols` (a wide char spends 2 columns of budget for
    // only 1 array slot), and clamping to the wider `cols` instead would
    // hand `highlight::compose` a span end past the real end of `chars`.
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

// Puts `marker` in the row's last (or first) column, replacing whatever
// was drawn there -- which is the point: the column it covers is content
// the reader can't see anyway.
fn overlay_marker(rendered: &str, cols: usize, marker: &str, at_end: bool) -> String {
    let width: usize = marker.chars().map(char_width).sum();
    if width == 0 || width > cols {
        return rendered.to_string();
    }
    let keep = if at_end { cols - width } else { width };
    let (head, tail) = split_rendered(rendered, keep);
    if at_end {
        format!("{head}\x1b[2m{marker}\x1b[0m")
    } else {
        format!("\x1b[2m{marker}\x1b[0m{tail}")
    }
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
// The pattern whose matches this frame should mark, if any.
//
// Mirrors repl.rs's own `active_search_pattern` for `ScreenBuffer` --
// duplicated rather than shared for the reason that one already gives
// about its twin in editor.rs: the three contexts drive three different
// `Buffer` impls, and this is a few lines either way.
fn active_search_pattern(vk: &VimKeys, buf: &TextBuffer) -> Option<String> {
    let pending = vk.pending_display();
    if let Some(rest) = pending.strip_prefix('/').or_else(|| pending.strip_prefix('?')) {
        // A pattern still being typed is shown regardless of `:noh`:
        // that is live feedback about what you are typing, not the
        // leftover highlight `:noh` is about.
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
    // What `hlsearch` should be drawing right now, worked out once for
    // the whole frame rather than per row.
    let search_pattern = active_search_pattern(vk, buf);
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
    let (whole_styled, whole_links) = buffer_spans(buf, color_overrides);
    // `hyperlinks` off means the text and only the text: the underline
    // that marks a link stays (it is the language's own styling), but
    // nothing is emitted for a terminal to make clickable.
    let whole_links = if buf.hyperlinks { whole_links } else { Vec::new() };
    // Measured once for the whole frame, not per row -- every row has to
    // agree about where the columns are.
    let layout = tabular_layout(buf);
    let starts = line_starts(buf);
    let mut out = String::new();
    let rows = visible_rows(buf, content_cols, content_rows);
    for r in 0..content_rows {
        out.push_str(&format!("\x1b[{};{}H\x1b[K", row_origin + r + 1, col_origin + 1));
        let Some(row) = rows.get(r) else {
            // Past the end of the buffer: the gutter still reserves its
            // width so the content column doesn't jump.
            render_gutter(&mut out, buf, &starts, total, false);
            continue;
        };
        let line = row.line;
        render_gutter(&mut out, buf, &starts, line, row.first);
        {
            // The char index render_row's own window will start
            // rendering from for *this* row -- see selection_columns_
            // in_line's own doc comment for why this (not `hoffset`
            // itself) is the right rebase point once a line can have
            // wide chars in it. With wrapping on the segment already
            // says where the row starts, and there is no horizontal
            // scroll to apply.
            let wrapping = buf.wrap.wrap;
            let display = display_row(buf, line, layout.as_ref());
            let prefix = continuation_prefix(buf, row.seg.indent);
            let avail = content_cols.saturating_sub(row.seg.indent);
            // Where in the drawn cells this row starts and stops. With
            // wrapping that is whatever the layout segmented; without
            // it, the horizontal scroll -- which is a display column, so
            // it indexes cells rather than characters.
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
                // A linewise selection covers the row to the pane's own
                // edge, padding included -- it is selecting the line,
                // not a range of its characters.
                let span = if range.shape == motion::MotionShape::Linewise {
                    Some((0, avail))
                } else {
                    to_window(&display, start_cell, avail, start, end)
                };
                if let Some((start, end)) = span {
                    highlights.push(StyledSpan { start, end, fg: vt100::Color::Default, attrs: vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() } });
                }
            }
            // Search matches, underlined -- distinct from a selection's
            // reverse video just above, so a match inside a selection
            // still reads as both.
            //
            // The file editor had no `hlsearch` at all, while both of
            // bish's other Normal modes (the prompt's own, and normal
            // mode over a pane's scrollback) have always drawn it. This
            // is the one place people actually search, and it was the
            // one place that never showed what it found.
            for (start, end) in search_matches.iter().copied() {
                let Some((start, end)) = to_window(&display, start_cell, avail, start, end) else { continue };
                highlights.push(StyledSpan {
                    start,
                    end,
                    fg: vt100::Color::Default,
                    attrs: vt100::CellAttrs { underline: true, ..vt100::CellAttrs::default() },
                });
            }
            // A live `abbr` snippet's own placeholders, marked exactly as
            // the shell prompt marks them (editor.rs's `snippet_layer`):
            // reverse video on the one being typed into, underline on the
            // rest. Mapped into the drawn window like every other layer,
            // and dropped entirely when scrolling has carried the hole
            // off it.
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
                        to_window(&display, start_cell, avail, s.start, s.end)
                            .map(|(start, end)| StyledSpan { start, end, fg: s.fg, attrs: s.attrs })
                    })
                    .collect()
            };
            let line_styled = map(spans_for_line(&whole_styled, starts[line], line_len));
            // The server's `documentHighlight` answer, kept on its own
            // axis so an occurrence keeps its colour and gains an
            // underline rather than losing one for the other.
            let line_marks = map(spans_for_line(&buf.document_highlights, starts[line], line_len));
            let diag_styled = map(diagnostic_spans_for_line(buf, &buf.diagnostics, starts[line], line_len));
            let links: Vec<highlight::LinkSpan> = links_for_line(&whole_links, starts[line], line_len)
                .into_iter()
                .filter_map(|l| {
                    to_window(&display, start_cell, avail, l.start, l.end)
                        .map(|(start, end)| highlight::LinkSpan { start, end, url: l.url })
                })
                .collect();
            out.push_str(&prefix);
            render_row(&mut out, buf, &display, start_cell, end_cell, avail, &line_styled, &diag_styled, &highlights, &line_marks, &links);
        }
    }

    let (cl, cc) = buf.cursor();
    // Found in the rows actually drawn rather than computed from the
    // line number, so a wrapped line puts the cursor on the row its own
    // segment landed on.
    let cursor_row = rows.iter().position(|row| row.line == cl && row.seg.contains(cc));
    let screen_row = match cursor_row {
        Some(r) => r,
        None => rows.iter().rposition(|row| row.line == cl).unwrap_or(0),
    }
    .min(content_rows.saturating_sub(1));
    // The cursor's own real display column, not its char index -- see
    // bishedit::unicode_width's own doc comment for why those two
    // differ once any wide/zero-width char precedes it on this line.
    // Through the drawn cells, not the line's own characters: with a
    // tabular display the two differ by however much padding precedes
    // the cursor.
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
    out.push_str(&format!("\x1b[{};{}H\x1b[?25h", row_origin + screen_row + 1, col_origin + screen_col + 1));
    out.push_str(cursor_shape(buf, mode, vk));
    out
}

// DECSCUSR (`CSI Ps SP q`): what the terminal's own cursor looks like, so
// which mode you are in is legible without reading the status line --
// the thing every modal editor's users learn to rely on and the one
// piece of mode feedback that survives not looking at the bottom of the
// screen.
//
// Emitted with every frame rather than once on entering a mode: a frame
// is the only thing that reliably runs on every change, and a job's
// output or another program's own escape sequences can have moved the
// cursor's shape underneath us in between. It costs four bytes.
pub(crate) fn cursor_shape(buf: &TextBuffer, mode: EditorMode, vk: &VimKeys) -> &'static str {
    if !buf.cursorshape {
        return "";
    }
    match mode {
        // A bar sits *between* characters, which is where an insertion
        // point actually is.
        EditorMode::Insert => "\x1b[6 q",
        // An underline says "this character is about to be replaced",
        // which is exactly what Replace mode does to it.
        EditorMode::Replace => "\x1b[4 q",
        // Normal mode's cursor is *on* a character, so a block. Except
        // mid-operator (`d`, `c`, `y` waiting for a motion), where an
        // underline says the keystroke you type next means something
        // different from usual -- the state it is easiest to forget you
        // are in.
        EditorMode::Normal if !vk.is_idle_except_count() && !vk.is_visual() => "\x1b[4 q",
        EditorMode::Normal => "\x1b[2 q",
    }
}

/// Back to whatever the terminal draws by default -- for whoever is
/// giving the terminal back.
pub(crate) const CURSOR_SHAPE_RESET: &str = "\x1b[0 q";

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

    // The gutter build_editor_frame draws for this buffer, which every
    // expectation below has to agree with -- read from the same place
    // rather than hardcoded, so a new gutter column can't quietly make
    // these tests describe a layout that no longer exists.
    fn gutter(b: &TextBuffer) -> usize {
        total_gutter_width(b).min(rect().cols - 1)
    }

    #[test]
    fn a_click_lands_on_the_character_under_it() {
        let b = buf("alpha
bravo
charlie");
        let g = gutter(&b);
        // Third row of the pane, fourth character of that line.
        assert_eq!(position_at_screen(&b, rect(), 2 + 2, 10 + g + 3), Some((2, 3)));
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
        // Well past "bravo"'s own five characters -> its last one.
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
        // Two double-width chars then ASCII: display column 4 is the
        // *third* char, which is exactly the distinction char_at_col
        // exists for -- a naive col-minus-gutter would say char 4.
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
        // The whole point of rejecting these: `e --hxe data.bin` quietly
        // creating a file called `--hxe` would be much worse than a
        // message saying the flag isn't one.
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
        // `e --hex` with no file: an empty byte buffer to build a small
        // binary in, not a silently-ignored flag.
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
        // 'a' + MAN+ZWJ+WOMAN (a 3-char cluster) + 'b'
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
                run_insert_mode(buf, vk, rect(), registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();
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
    // `abbr` expansion inside the file editor's own Insert mode, driven
    // through the same macro-replay queue the test just below uses --
    // there's no real terminal here to type at, and `run_insert_mode`
    // reads through `vk.next_key`, which serves a queued replay first.
    #[test]
    fn a_block_yank_puts_back_as_a_rectangle() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "aaa111\nbbb222\nccc333\nzz");
        let mut registers = Registers::new_for_test();
        buf.selections = vec![crate::bishedit::motion::MotionRange {
            shape: crate::bishedit::motion::MotionShape::Blockwise,
            from: (0, 0),
            to: (2, 2),
        }];
        let text = crate::bishedit::motion::extract_text(&buf, &buf.selections[0]);
        assert_eq!(text, "aaa\nbbb\nccc");
        registers.record_yank(None, crate::bishedit::registers::RegisterValue { text, shape: RegisterShape::Block });
        buf.selections.clear();

        // Put after the cursor on the short last line: the rectangle is
        // rebuilt downward, and a line too short to reach the column is
        // padded out to it.
        buf.set_cursor(3, 0);
        put(&mut buf, &mut registers, false, None, None);
        assert_eq!(text_of(&buf), "aaa111\nbbb222\nccc333\nzaaaz\n bbb\n ccc");
    }

    // Plain autoindent: copy, never guess. `smartindent` is what opens a
    // level after `{`, and it is deliberately not this.
    #[test]
    fn autoindent_copies_the_lines_own_leading_whitespace() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "no indent\n    four spaces\n\tone tab\n   \n");
        assert_eq!(autoindent_for(&buf, 0), "");
        assert_eq!(autoindent_for(&buf, 1), "    ");
        assert_eq!(autoindent_for(&buf, 2), "\t");
        // Whitespace-only: nothing, so Enter on a blank line does not
        // leave a trail of trailing spaces behind it.
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

    // The reason bracketed paste ships with autoindent rather than after
    // it: the same bytes have to land differently depending on whether
    // they were typed or pasted, and without the brackets a terminal
    // cannot say which.
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

        // Typed, every Enter copies the line above it -- so the closing
        // brace inherits `b();`'s own indent rather than the `if`'s.
        // That is the staircase, and it is the correct behaviour for
        // text someone is actually typing.
        assert_eq!(pasted(body), "    startif a {\n        b();\n        }", "typed, the same characters get indented as you go");
    }

    // The file editor never drew search matches at all, while both of
    // bish's other Normal modes always have.
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

        // ...but typing a *new* pattern still shows as you type it,
        // suppressed or not: that is feedback about the keystrokes, not
        // the leftover highlight `:noh` is about.
        for key in [Key::Char('/'), Key::Char('g')] {
            vk.feed(key);
        }
        assert_eq!(active_search_pattern(&vk, &buf).as_deref(), Some("g"));
    }

    // `insert_with`, but driving a buffer the caller prepared -- a
    // selection standing, a cursor somewhere particular.
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

    // Home, End and Delete did nothing at all in Insert mode: Normal mode
    // binds all three (vimkeys.rs) and so does the shell prompt
    // (editor.rs), and this one loop had no arm for any of them.
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
        buf.insert_text((0, 0), "abc
def");
        buf.set_cursor(0, 1);
        insert_into(&mut buf, &[Key::Delete]);
        assert_eq!(text_of(&buf), "ac
def", "the character *after* the cursor goes");

        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "abc
def");
        buf.set_cursor(0, 3);
        insert_into(&mut buf, &[Key::Delete]);
        assert_eq!(text_of(&buf), "abcdef", "at end of line the newline goes, joining the next");
        assert_eq!(buf.cursor(), (0, 3), "and the cursor does not move");

        // The very end of the buffer has nothing after it.
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "abc");
        buf.set_cursor(0, 3);
        insert_into(&mut buf, &[Key::Delete]);
        assert_eq!(text_of(&buf), "abc");
    }

    // Backspace and Delete already replaced a standing selection; typing
    // did not, which is the half that makes a mouse selection worth
    // making.
    #[test]
    fn insert_mode_typing_replaces_a_standing_selection() {
        let select = |buf: &mut TextBuffer| {
            buf.selections = vec![crate::bishedit::motion::MotionRange {
                shape: crate::bishedit::motion::MotionShape::Exclusive,
                from: (0, 6),
                to: (0, 10),
            }];
            buf.set_cursor(0, 10);
        };

        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "alpha beta gamma");
        select(&mut buf);
        insert_into(&mut buf, &[Key::Char('X')]);
        assert_eq!(text_of(&buf), "alpha X gamma");
        assert!(buf.selections.is_empty(), "and the selection is gone, not left standing over new text");

        // The two that already worked, pinned so they stay working.
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

    // An `InsertServices` that offers exactly what it was handed --
    // enough to drive Ctrl-N and the accept that follows without a
    // language server anywhere in sight.
    struct FixedCompletions(Vec<crate::bishedit::completion::EditorCompletion>);

    impl InsertServices for FixedCompletions {
        fn idle(&mut self, _buf: &mut TextBuffer) -> Option<IdleRedraw> {
            None
        }

        fn complete(&mut self, _buf: &TextBuffer, _row: usize, _col: usize) -> Vec<crate::bishedit::completion::EditorCompletion> {
            self.0.clone()
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

    // A server's snippet completion becomes exactly the same live
    // snippet an `abbr` does: same keys, same marking, same accept.
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

        // Driven to the end it reads as if typed by hand, `$0` included.
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

    // A server may flag a snippet that has no tabstops in it. That is
    // plain text -- but still written in snippet notation, so `\$` is
    // a literal dollar rather than two characters.
    #[test]
    fn a_snippet_completion_with_no_tabstops_is_plain_text() {
        let mut keys = chars("co");
        keys.push(Key::CtrlN);
        keys.push(Key::Enter);
        let buf = insert_completing(&keys, vec![completion("cost", "cost \\$5", true)]);
        assert_eq!(text_of(&buf), "cost $5");
        assert!(buf.snippet_holes.is_empty());
        // Unflagged, the same text is exactly what it says.
        let buf = insert_completing(&keys, vec![completion("cost", "cost \\$5", false)]);
        assert_eq!(text_of(&buf), "cost \\$5");
    }

    fn insert_with(path: Option<&str>, keys: &[Key], abbrs: &[Abbr]) -> TextBuffer {
        // `TextBuffer::open` on a nonexistent path prepares to create it
        // (see its own doc comment) -- nothing is ever written here, and
        // the path is only present so `language_of` has an extension to
        // read.
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
        // The shell prompt gates expansion to command position; a file
        // has no command positions, so this deliberately does not.
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
        // The regression this guards: the snippet moves the real cursor
        // through its own model, so the multi-cursor list this function
        // inserts at has to be resynced from the buffer -- left stale, a
        // keystroke right after an accept lands back where the
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
        // `R`'s whole contract is that the line's length doesn't change.
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
        // No accept/cancel: Insert mode ends at EOF with the snippet
        // still live, so the marks are whatever was last written.
        let buf = insert_with(Some("main.rs"), &chars("f "), &abbrs);
        assert_eq!(buf.snippet_holes.len(), 2);
        assert!(buf.snippet_holes[0].active && !buf.snippet_holes[1].active);
        assert_eq!((buf.snippet_holes[0].start, buf.snippet_holes[0].end), (3, 5));
    }

    // A file buffer has as many lines as it likes, so a multi-line
    // snippet stays multi-line here -- the one thing the prompt cannot
    // do.
    #[test]
    fn a_multi_line_snippet_spans_real_lines_in_a_file() {
        let abbrs = vec![Abbr { lang: "rust".into(), ..Abbr::new("f", "fn ${1:name}() {\n    $0\n}") }];
        let mut keys = chars("f ");
        keys.extend(chars("main"));
        keys.push(Key::CtrlY);
        keys.extend(chars("todo!()"));
        let buf = insert_with(Some("main.rs"), &keys, &abbrs);
        // `$0` put the caret on the indented middle line, so what was
        // typed next landed there rather than after the closing brace.
        assert_eq!(text_of(&buf), "fn main() {\n    todo!()\n}");
    }

    // Before it is accepted, the holes are marked on the lines they are
    // actually on -- the renderer draws per line.
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
        // Gaps at columns 0 and 6 of one row -- typing "X" at both must
        // land right where each original gap was, not have the first
        // insertion's own rightward shift throw off the second's.
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
        // `gi` resumes appending from exactly where insert mode left off,
        // which the Normal-mode clamp above must not disturb.
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

// Ctrl-W in Insert mode: delete the word before the cursor (`:help
// i_CTRL-W`) -- previously a silent no-op (fell through run_insert_
// mode's own catch-all `_ => {}`, since Key::CtrlW was never matched
// there at all; the Normal-mode `<C-w>` window-command prefix it maps
// to elsewhere in this codebase is a different dispatch this function
// never reaches).
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

// Alt-Left/Alt-Right in Insert mode: real vim's own `b`/`w` word
// motions, previously unhandled (silently fell through the same
// catch-all `_ => {}` Ctrl-W did -- see this module's own commit
// history) -- moves the cursor only, never deletes anything.
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
    fn alt_left_moves_the_cursor_back_a_word_without_deleting() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "hello world");
        buf.set_cursor(0, buf.line_len(0));
        let mut vk = VimKeys::new();
        let mut registers = Registers::new_for_test();
        scripted(&mut vk, &[Key::AltLeft, Key::Escape]);

        run_insert_mode(&mut buf, &mut vk, rect(), &mut registers, &mut NoInsertServices, false, 24, 80, None, &[], &[]).unwrap();

        assert_eq!(buf.line_chars(0).into_iter().collect::<String>(), "hello world", "nothing should be deleted");
        assert_eq!(buf.cursor(), (0, 6), "cursor should land at the start of 'world', matching vim's own `b`");
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
        assert_eq!(buf.cursor(), (0, 6), "cursor should land at the start of 'world', matching vim's own `w`");
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

    // `diagnose_buffer`'s language gate keys off the path's own
    // extension (`language_of`) -- `new_unnamed`/`insert_text` cannot produce
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
        let diags = vec![lint::Diagnostic { start: 5, end: 9, severity: lint::Severity::Warning, code: Cow::Borrowed("unquoted-expansion"), source: None, message: String::new(), fix: None }];
        let b = TextBuffer::new_unnamed(10);
        let spans = diagnostic_spans_for_line(&b, &diags, 0, 20);
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start, spans[0].end), (5, 9));
        assert!(diagnostic_spans_for_line(&b, &diags, 10, 20).is_empty());
    }

    // The gutter has room for one mark per line; a line can hold
    // several findings. The rule is "show the worst," which is the only
    // reason `Severity` derives `Ord` at all.
    #[test]
    fn the_gutter_mark_takes_the_worst_severity_on_the_line() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "one two\nthree");
        let diag = |start, end, severity| lint::Diagnostic { start, end, severity, code: Cow::Borrowed("x"), source: None, message: String::new(), fix: None };
        buf.diagnostics = vec![diag(0, 3, lint::Severity::Hint), diag(4, 7, lint::Severity::Error), diag(4, 7, lint::Severity::Warning)];
        let starts = line_starts(&buf);
        assert_eq!(line_severity(&buf, &starts, 0), Some(lint::Severity::Error));
        // Nothing intersects the second line, so no mark at all.
        assert_eq!(line_severity(&buf, &starts, 1), None);
        // ...and the mark is drawn in that severity's own colour, not
        // the hardcoded yellow this used to always be.
        let cell = render_diagnostic_cell(&buf, &starts, 0, 2).unwrap();
        let (fg, _) = crate::theme::resolve(crate::theme::Ui::Error, None);
        assert!(cell.starts_with(&vt100::sgr_codes(fg, vt100::Color::Default, vt100::CellAttrs::default())), "{cell:?}");
        assert!(render_diagnostic_cell(&buf, &starts, 1, 2).is_none());
    }

    #[test]
    fn a_real_edit_clears_previously_computed_diagnostics() {
        let mut buf = temp_bash_buffer("clears", "echo $foo\n");
        buf.diagnostics = diagnose_buffer(&buf);
        assert!(!buf.diagnostics.is_empty());
        buf.insert_text((0, 0), "x");
        assert!(buf.diagnostics.is_empty());
    }

    // The rule the whole batch turns on: applied last-first, because an
    // earlier edit that shifted the text would invalidate every later
    // range.
    #[test]
    fn a_batch_of_edits_applies_without_invalidating_its_own_ranges() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "one two three");
        // Given in *ascending* order, as a server sends them.
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
        // Replace from mid-line 0 through mid-line 2 with two lines --
        // the shape a formatter rewriting a block sends.
        let edits = vec![lsp::TextEdit { start: (0, 1), end: (2, 1), text: "X\nY".to_string() }];
        assert_eq!(apply_text_edits(&mut buf, &edits, lsp::PositionEncoding::Utf32), 1);
        assert_eq!(buffer_text(&buf), "aX\nY");
    }

    #[test]
    fn a_pure_insertion_and_a_pure_deletion_both_work() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "ac");
        // Empty range, non-empty text: an insertion.
        assert_eq!(apply_text_edits(&mut buf, &[lsp::TextEdit { start: (0, 1), end: (0, 1), text: "b".to_string() }], lsp::PositionEncoding::Utf32), 1);
        assert_eq!(buffer_text(&buf), "abc");
        // Non-empty range, empty text: a deletion.
        assert_eq!(apply_text_edits(&mut buf, &[lsp::TextEdit { start: (0, 1), end: (0, 2), text: String::new() }], lsp::PositionEncoding::Utf32), 1);
        assert_eq!(buffer_text(&buf), "ac");
        // Empty range, empty text: nothing, and counted as nothing, so
        // a formatter that changed nothing can say so.
        assert_eq!(apply_text_edits(&mut buf, &[lsp::TextEdit { start: (0, 1), end: (0, 1), text: String::new() }], lsp::PositionEncoding::Utf32), 0);
        assert_eq!(buffer_text(&buf), "ac");
    }

    #[test]
    fn a_server_position_becomes_a_flat_offset_and_back_again() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "abc\ndefgh\nij");
        let starts = line_starts(&buf);
        let utf32 = lsp::PositionEncoding::Utf32;
        // The inverse of diagnostic_position, which is tested just
        // below -- so the two are checked against each other.
        for offset in [0usize, 3, 4, 8, 9, 11] {
            let (line, col) = diagnostic_position(&buf, offset);
            assert_eq!(diagnostic_offset(&buf, &starts, line, col, utf32), offset, "offset {offset}");
        }
        // Past the end clamps to the end of the buffer rather than
        // discarding a finding that is probably still pointing at the
        // right place.
        assert_eq!(diagnostic_offset(&buf, &starts, 99, 0, utf32), 12);
        assert_eq!(diagnostic_offset(&buf, &starts, 1, 99, utf32), starts[1] + 5);
    }

    // The whole reason positionEncoding gets negotiated: a server still
    // counting UTF-16 code units names a column bish would otherwise
    // read as a different character.
    #[test]
    fn a_utf16_column_lands_on_the_right_character() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "a\u{1f30d}bc");
        let starts = line_starts(&buf);
        // The emoji is one char to bish and two UTF-16 code units, so
        // `b` is at char 2 but at UTF-16 column 3.
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
        // Always sourced, even when the server named none: that is what
        // distinguishes a relayed finding from one of bish's own, which
        // is what lets a new publication replace exactly the right ones.
        assert_eq!(diagnostics[1].source.as_deref(), Some("lsp"));
        // A server's diagnostic carries no edit of its own -- a fix is a
        // separate code-action request.
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
        // "$foo" sits at flat offsets 5..9.
        let diagnostic = lint::Diagnostic {
            start: 5,
            end: 9,
            severity: lint::Severity::Warning,
            code: Cow::Borrowed("unquoted-expansion"), source: None,
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
        let diagnostic = lint::Diagnostic { start: 5, end: 9, severity: lint::Severity::Warning, code: Cow::Borrowed("unquoted-expansion"), source: None, message: String::new(), fix: None };
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
        // 40 columns of content in a 10-column pane: 30 is as far as
        // there is anything left to see.
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

    // The whole difference from scroll_to_show_cursor: the cursor comes
    // to the view, not the view back to the cursor.
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

    // Nothing to the side means nothing to do.
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

    // Carets sit between characters and a range covers characters, so
    // the later end steps back one: from before `a` to before `c` is
    // `ab`, not `abc`.
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

    // A caret with nothing swept is a cursor, not an empty selection --
    // otherwise a plain click would leave one standing for Backspace to
    // find.
    #[test]
    fn a_caret_that_has_not_moved_is_not_a_selection() {
        let b = buf("abcdef\n");
        assert_eq!(selection_between(&b, (0, 3), (0, 3)), None);
    }

    // Dragging up to the very start of a line ends the selection at the
    // end of the line before it.
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

    // To the next *stop*, not a fixed width -- which is the whole
    // difference between a tab and a run of spaces.
    #[test]
    fn a_tab_fills_only_what_is_left_of_its_stop() {
        let mut b = buf("ab\tc\n");
        b.tabstop = 4;
        assert_eq!(drawn(&b, 0), "ab  c", "two columns left of the stop");
        let mut b = buf("abc\tx\n");
        b.tabstop = 4;
        assert_eq!(drawn(&b, 0), "abc x", "one");
    }

    // Every space of a tab points back at the one character that is
    // really there, so a click anywhere in it lands somewhere real.
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

    // vim's own rule: one tab is a whole indent, so outdenting a
    // tab-indented line takes the tab and not a shiftwidth of them.
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
        // The cursor sits three rows above the bottom, not on it.
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

    // Nothing to scroll past the ends, so the margin gives way there --
    // otherwise the first line could never sit at the top.
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

    // A margin wider than half the pane has no middle left to keep the
    // cursor in, so it is capped rather than fighting itself.
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

    // The column can't shrink to fit the offsets: a gutter that changed
    // width as the cursor moved would shift the whole pane sideways on
    // every `j`.
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

    // The dispatch, not the highlighters themselves (each has its own
    // tests in bishedit::highlight) -- what this pins is that the editor
    // asks the right one and, for a language with none, quietly renders
    // plain text instead of falling back to bash's highlighter and
    // painting nonsense.
    #[test]
    fn buffer_highlighting_follows_the_buffers_own_language() {
        let json = buf_with_ext(r#"{"a": 1}"#, "json");
        assert!(!buffer_highlight_spans(&json, None).is_empty(), "a .json buffer is highlighted");

        let bash = buf_with_ext("if true; then echo hi; fi", "bash");
        assert!(!buffer_highlight_spans(&bash, None).is_empty(), "a .bash buffer still is");

        let markdown = buf_with_ext("# Title\n\n*emphasis*\n", "md");
        assert!(!buffer_highlight_spans(&markdown, None).is_empty(), "and a .md buffer");

        // A man page section number is an extension like any other.
        let roff = buf_with_ext(".SH NAME\nls \\- list directory contents\n", "1");
        assert!(!buffer_highlight_spans(&roff, None).is_empty(), "and a .1 buffer");
        assert_eq!(language_of(&roff), "roff");

        let toml = buf_with_ext("[package]\nname = \"bish\"\n", "toml");
        assert!(!buffer_highlight_spans(&toml, None).is_empty(), "and a .toml buffer");

        // Valid bash, and deliberately not highlighted as any: nothing
        // claims to know what a .zig file is. (This used to be the .toml
        // case, until TOML got a highlighter of its own -- the point of
        // the assertion is the fallback, so it moved to a language
        // nothing here has an opinion about rather than being dropped.)
        let unclaimed = buf_with_ext("if true; then echo hi; fi", "zig");
        assert!(buffer_highlight_spans(&unclaimed, None).is_empty(), "a language with no highlighter renders plain");
    }

    // Opening compressed content: a member inside a zip and a gzip'd
    // file both arrive as ordinary-looking read-only buffers, named by
    // the path that was asked for.
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

        // ...while an ordinary file in the same directory is untouched
        // by any of this.
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

        // An ordinary nonexistent path opens as a new file (vim's own
        // `:e newfile`); a nonexistent *member* can't, since there's no
        // way to create one.
        let opened = EditSession::open(Some(&crate::archive::join(&zip, "nope.txt")), 10);
        let err = match opened {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a nonexistent member must not open as a blank buffer"),
        };
        assert!(err.contains("no such member"), "{err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // `.gz` says how the bytes are stored, not what they are.
    // The wrap options ride on the buffer (see TextBuffer::wrap), so a
    // test can set them directly and check the layout the frame builder
    // produces without a terminal.
    fn wrapped_buf(text: &str, wrap: crate::bishedit::wrap::Options) -> TextBuffer {
        wrapped_buf_h(text, wrap, 10)
    }

    // The viewport height is fixed at construction, so a test that cares
    // about scrolling states it up front.
    fn wrapped_buf_h(text: &str, wrap: crate::bishedit::wrap::Options, height: usize) -> TextBuffer {
        let mut buf = TextBuffer::new_unnamed(height);
        buf.insert_text((0, 0), text);
        buf.set_cursor(0, 0);
        buf.wrap = wrap;
        buf
    }

    fn wrap_on() -> crate::bishedit::wrap::Options {
        crate::bishedit::wrap::Options {
            wrap: true,
            showbreak: String::new(),
            breakindent: false,
            linebreak: false,
            ..Default::default()
        }
    }

    #[test]
    fn visible_rows_break_a_long_line_across_screen_rows() {
        let buf = wrapped_buf("abcdefghij\nok", wrap_on());
        let rows = visible_rows(&buf, 4, 10);
        let shape: Vec<(usize, bool, usize, usize)> =
            rows.iter().map(|r| (r.line, r.first, r.seg.start, r.seg.end)).collect();
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

    // The frame and the click-to-position inverse have to agree, or a
    // click lands on a different character than the one under the
    // pointer.
    #[test]
    fn a_click_on_a_wrapped_row_finds_the_character_under_it() {
        let buf = wrapped_buf("abcdefghij", wrap_on());
        // A pane 4 columns wide *after* the gutter.
        let gutter = total_gutter_width(&buf);
        let rect = Rect { row: 0, col: 0, rows: 12, cols: gutter + 4 };
        // Row 0 column 0 is 'a'; row 1 column 0 is 'e' (the second
        // segment starts at char 4); row 2 column 1 is 'j'.
        assert_eq!(position_at_screen(&buf, rect, 0, gutter), Some((0, 0)));
        assert_eq!(position_at_screen(&buf, rect, 1, gutter), Some((0, 4)));
        assert_eq!(position_at_screen(&buf, rect, 2, gutter + 1), Some((0, 9)));
    }

    // A line taller than the pane has to be scrollable *within* itself,
    // or a minified file can scroll to the right line and then never
    // reach the cursor inside it.
    #[test]
    fn scrolling_can_move_within_a_line_taller_than_the_pane() {
        let mut buf = wrapped_buf_h(&"x".repeat(40), wrap_on(), 3);
        // 4 columns wide -> 10 rows for this one line, in a 3-row pane.
        buf.set_cursor(0, 39);
        scroll_to_show_cursor(&mut buf, 4);
        assert_eq!(buf.viewport_top(), 0, "still the only line there is");
        assert_eq!(buf.viewport_sub(), 7, "but scrolled seven rows into it");
        let rows = visible_rows(&buf, 4, 3);
        assert!(rows.iter().any(|r| r.seg.contains(39)), "the cursor's own row is on screen");

        // Back to the top of the line.
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

    // sidescrolloff only applies when *not* wrapping -- it is about the
    // other way of handling a long line.
    #[test]
    fn sidescrolloff_keeps_columns_visible_either_side_of_the_cursor() {
        let mut buf = wrapped_buf(&"x".repeat(80), crate::bishedit::wrap::Options {
            sidescrolloff: 5,
            ..Default::default()
        });
        buf.set_cursor(0, 40);
        scroll_to_show_cursor(&mut buf, 20);
        // The cursor sits 5 columns short of the right edge, not against
        // it: viewport_left + 20 - 1 - 5 == 40.
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
        // Char 9 is on the third segment (8..10), second column of it.
        assert!(
            frame.contains(&format!("\x1b[3;{}H", gutter + 2)),
            "expected the cursor on row 3 column {}, got:\n{frame:?}",
            gutter + 2
        );
    }

    // The language server's "these are the same symbol" marks compose
    // over the colour already there rather than replacing it -- the one
    // thing that distinguishes this layer from the search and selection
    // ones, and the thing a regression would silently undo.
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
        // The read is underlined; the write is underlined *and* bold,
        // which is what distinguishes "assigned here" from "used here".
        assert!(marked.contains("\x1b[0;4malpha"), "expected an underlined read:\n{marked:?}");
        assert!(marked.contains("\x1b[0;1;4malpha"), "expected a bold underlined write:\n{marked:?}");
        // The word between them keeps no marking at all.
        assert!(marked.contains("\x1b[0m beta "), "{marked:?}");
        // And the marks are gone the moment the buffer's are.
        buf.document_highlights.clear();
        let cleared = build_editor_frame(&buf, &VimKeys::new(), EditorMode::Normal, rect, 0, 0, None);
        assert_eq!(cleared, plain, "clearing the marks restores exactly the unmarked frame");
    }

    fn csv_buf(text: &str) -> TextBuffer {
        let mut buf = TextBuffer::open(std::path::Path::new("/tmp/bish-tabular-test.csv"), 10).unwrap();
        buf.insert_text((0, 0), text);
        buf.set_cursor(0, 0);
        buf.tabular = crate::bishedit::tabular::style("csv");
        buf
    }

    // What the pane actually draws, with the styling stripped, so a test
    // reads as the screen does.
    fn drawn(buf: &TextBuffer, cols: usize) -> Vec<String> {
        let rect = Rect { row: 0, col: 0, rows: 12, cols };
        let frame = build_editor_frame(buf, &VimKeys::new(), EditorMode::Normal, rect, 0, 0, None);
        let gutter = total_gutter_width(buf);
        // A cursor-position escape starts a row; every other escape is
        // styling and contributes no characters.
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
        rows.into_iter()
            .map(|row| row.chars().skip(gutter).collect::<String>().trim_end().to_string())
            .filter(|row| !row.is_empty())
            .collect()
    }

    #[test]
    fn a_csv_buffer_draws_its_columns_lined_up() {
        let buf = csv_buf("name,age,city\nalice,30,NYC\nbo,7,LA");
        assert_eq!(drawn(&buf, 60), vec!["name,  age, city", "alice, 30,  NYC", "bo,    7,   LA"]);
    }

    // The alignment is drawn, never stored: the buffer still holds
    // exactly what was typed, delimiters and all.
    #[test]
    fn the_alignment_never_touches_the_text() {
        let mut buf = csv_buf("name,age\nalice,30");
        assert_eq!(buffer_text(&buf), "name,age\nalice,30");
        // ...and a delimiter is still an ordinary character: adding one
        // splits the row into another column, which is the whole point
        // of the alignment being cosmetic.
        buf.insert_text((0, 4), ",");
        assert_eq!(buffer_text(&buf), "name,,age\nalice,30");
        assert_eq!(drawn(&buf, 60).len(), 2);
    }

    // A click on padding belongs to the character it was inserted after
    // -- there is nothing else it could mean, since padding is not in
    // the file.
    #[test]
    fn a_click_on_padding_lands_on_the_character_before_it() {
        let buf = csv_buf("alice,30\nbo,7");
        let gutter = total_gutter_width(&buf);
        let rect = Rect { row: 0, col: 0, rows: 12, cols: gutter + 40 };
        // Row 1 draws "bo,    7": columns 0-1 are `bo`, 2 is the comma,
        // 3..6 are padding, 7 is `7`.
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

    // Wrapping and alignment are two answers to the same question, so
    // only one of them applies.
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
        // No table entry needed: an unknown extension is its own name,
        // which is what makes `abbr --lang=toml` work.
        assert_eq!(language_of(&buf_with_ext("x", "toml")), "toml");
        assert_eq!(language_of(&buf_with_ext("x", "TOML")), "toml", "case-folded, so `--lang=toml` matches either spelling");
        assert_eq!(language_of(&buf_with_ext("x", "txt")), "txt");
        assert_eq!(language_of(&TextBuffer::new_unnamed(10)), "text");
    }

    fn buf_named(name: &str) -> TextBuffer {
        TextBuffer::open(std::path::Path::new(&format!("/tmp/bish-fileeditor-name-test/{name}")), 10).unwrap()
    }

    // The INI family is spread across extensions that share nothing.
    #[test]
    fn the_ini_family_is_recognized_by_extension() {
        for name in ["settings.ini", "setup.cfg", "sshd.conf", "firefox.desktop", "nginx.service", "boot.timer", "wg0.netdev"] {
            assert_eq!(language_of(&buf_named(name)), "ini", "{name}");
        }
        assert_eq!(language_of(&buf_named("Desktop.INI")), "ini", "case-folded like every other extension");
    }

    // ...and the most-used ones have no extension at all, which is why
    // there is a name table too.
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
        // ...and a plain `.json` stays strict.
        assert_eq!(language_of(&buf_named("package.json")), "json");
    }

    // The URL pass runs whatever the language is, because being a URL
    // isn't a property of the language around it.
    #[test]
    fn a_bare_url_becomes_a_link_in_any_buffer() {
        let buf = buf_with_ext("ticket https://bugs.example.com/42 here", "txt");
        let (_, links) = buffer_spans(&buf, None);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "https://bugs.example.com/42");
    }

    // ...and the sentence's punctuation is not the URL's.
    #[test]
    fn a_url_at_the_end_of_a_sentence_stops_before_the_full_stop() {
        let buf = buf_with_ext("see https://example.com/x.", "txt");
        let (_, links) = buffer_spans(&buf, None);
        assert_eq!(links[0].url, "https://example.com/x");
    }

    // A markdown link to a sibling document is a relative path, which no
    // terminal can open -- resolving it against the buffer's own
    // directory is what makes clicking it go somewhere.
    #[test]
    fn a_relative_link_target_is_resolved_against_the_buffer_directory() {
        let buf = buf_with_ext("[the plan](plan.md)", "md");
        let (_, links) = buffer_spans(&buf, None);
        let plan = links.iter().find(|l| l.url.ends_with("plan.md")).expect("the relative link");
        assert!(plan.url.starts_with("file:///"), "got {}", plan.url);
    }

    // An unnamed buffer has no directory to resolve against, so a
    // relative target stays underlined and unclickable rather than
    // being guessed at.
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

    // direnv's file is a shell script, not a list of assignments, and
    // the `.env` prefix rule must not take it.
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

    // `config` on its own is claimed by everything; inside `.git` it
    // isn't ambiguous at all.
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

// `:git blame`'s own tests -- the no-path check runs unconditionally (no
// subprocess involved), but everything else here needs an actual git
// repository and a real `git` on $PATH, so those skip themselves (rather
// than failing) when crate::git::available() says there isn't one --
// matching this whole feature's own "quietly unavailable, not a hard
// dependency" contract (see git.rs's own module doc comment).
#[cfg(test)]
mod git_blame_tests {
    use super::*;

    // A repo with two commits: "one\ntwo\n", then "one\nTWO\nthree\n".
    fn repo_with_history(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-git-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(&dir).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test User"]);
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
        let err = toggle_git_blame(&mut buf, None).unwrap_err();
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
        assert!(toggle_git_blame(&mut buf, None).unwrap());
        let blame = buf.blame.as_ref().unwrap();
        assert_eq!(blame.len(), 3);
        let first = blame[0].as_ref().unwrap();
        assert_eq!(first.author, "Test User");
        assert_eq!(first.short_commit.len(), 8);
        assert!(!toggle_git_blame(&mut buf, None).unwrap());
        assert!(buf.blame.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // The whole point of aligning blame to the buffer rather than
    // refusing a dirty one: a line typed just now has no blame (it's in
    // no revision at all), and -- crucially -- every line *after* it
    // still gets the right one despite having shifted down.
    #[test]
    fn toggle_git_blame_lines_up_with_a_dirty_buffer() {
        if !crate::git::available() {
            return;
        }
        let dir = repo_with_history("blame-dirty-test");
        let mut buf = TextBuffer::open(&dir.join("f.txt"), 10).unwrap();
        buf.insert_text((0, 0), "TYPED JUST NOW\n");
        assert!(buf.is_dirty());

        assert!(toggle_git_blame(&mut buf, None).unwrap());
        let blame = buf.blame.as_ref().unwrap();
        assert_eq!(blame.len(), 4);
        assert!(blame[0].is_none(), "the typed line has no blame");
        for (i, entry) in blame.iter().enumerate().skip(1) {
            assert_eq!(entry.as_ref().unwrap().author, "Test User", "line {i}");
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // Blame at an older revision, against a buffer holding the *newer*
    // content: "three" doesn't exist back there, so it gets no blame,
    // while the two lines that do exist still line up.
    #[test]
    fn toggle_git_blame_at_a_revision_marks_lines_absent_from_it() {
        if !crate::git::available() {
            return;
        }
        let dir = repo_with_history("blame-rev-test");
        let mut buf = TextBuffer::open(&dir.join("f.txt"), 10).unwrap();

        assert!(toggle_git_blame(&mut buf, Some("HEAD~1")).unwrap());
        let blame = buf.blame.as_ref().unwrap();
        assert_eq!(blame.len(), 3);
        assert!(blame[0].is_some(), "`one` is unchanged since HEAD~1");
        assert!(blame[1].is_none(), "`TWO` only exists after HEAD~1");
        assert!(blame[2].is_none(), "`three` only exists after HEAD~1");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggle_git_blame_reports_an_unknown_revision() {
        if !crate::git::available() {
            return;
        }
        let dir = repo_with_history("blame-badrev-test");
        let mut buf = TextBuffer::open(&dir.join("f.txt"), 10).unwrap();
        let err = toggle_git_blame(&mut buf, Some("no-such-rev")).unwrap_err();
        assert!(err.contains("no-such-rev"), "{err}");
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
    fn toggle_git_diff_refuses_a_buffer_with_no_path() {
        let mut buf = TextBuffer::new_unnamed(10);
        let err = toggle_git_diff(&mut buf, None).unwrap_err();
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
        // Changed on disk and reloaded, so the committed content and the
        // buffer really do differ.
        std::fs::write(&path, "one\nCHANGED\nthree\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert!(!buf.is_dirty());
        assert!(toggle_git_diff(&mut buf, None).unwrap());
        assert_eq!(buf.diff.as_ref().unwrap().get(&1), Some(&DiffMark::Changed));
        assert_eq!(buf.diff.as_ref().unwrap().len(), 1);
        assert!(!toggle_git_diff(&mut buf, None).unwrap());
        assert!(buf.diff.is_none());

        // ...and an *unsaved* edit counts too, which is the reason this
        // diffs the buffer itself rather than the file on disk.
        buf.insert_text((0, 0), "TYPED\n");
        assert!(toggle_git_diff(&mut buf, None).unwrap());
        let diff = buf.diff.as_ref().unwrap();
        assert_eq!(diff.get(&0), Some(&DiffMark::Added));
        assert_eq!(diff.get(&2), Some(&DiffMark::Changed));
        assert_eq!(diff.len(), 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // `:git diff HEAD~1` against a buffer that matches HEAD exactly: the
    // markers describe the older revision, not the index.
    #[test]
    fn toggle_git_diff_at_a_revision_marks_what_changed_since_it() {
        if !crate::git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-fileeditor-git-diff-rev-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git_run(&dir, &["init", "-q"]);
        git_run(&dir, &["config", "user.email", "test@example.com"]);
        git_run(&dir, &["config", "user.name", "Test User"]);
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        git_run(&dir, &["add", "f.txt"]);
        git_run(&dir, &["commit", "-q", "-m", "initial"]);
        std::fs::write(&path, "one\nTWO\nthree\n").unwrap();
        git_run(&dir, &["add", "f.txt"]);
        git_run(&dir, &["commit", "-q", "-m", "second"]);

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        // Against the index (HEAD) there is nothing to show at all...
        assert!(toggle_git_diff(&mut buf, None).unwrap());
        assert!(buf.diff.as_ref().unwrap().is_empty());
        assert!(!toggle_git_diff(&mut buf, None).unwrap());
        // ...but against HEAD~1, the second line onwards is a hunk that
        // replaced `two` with two lines -- one changed hunk, exactly as
        // real `git diff -U0` reports it (`@@ -2 +2,2 @@`), not a change
        // plus a separate addition.
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
        git_run(&dir, &["init", "-q"]);
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
