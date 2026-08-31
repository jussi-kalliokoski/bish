// `e --hex FILE` -- a hex viewer/editor built on bishedit's existing
// headless core rather than a parallel implementation of it. A top-level
// module, the same tier as fileeditor.rs/browser.rs: a concrete
// interactive view, not reusable headless logic.
//
// Structured as the same split browser.rs's own module doc comment
// describes: this module owns the buffer, the layout arithmetic, the key
// handling and the rendering (all testable without a terminal), and
// repl.rs owns the frame it lives in (`Frame::Hex`), that frame's pane
// rect, and the loop reading real keystrokes into it (`run_hex_frame`).
// So a hex view is a first-class editor frame -- stacked on the same
// pane's frame stack alongside ordinary `Frame::Edit`s, left running and
// resumed intact whenever focus moves to another pane or window --
// rather than a separate program that owns the whole terminal.
//
// The whole design bet here is that a hex editor is a *buffer* with an
// unusual rendering, not a different kind of program. So `HexBuffer`
// implements `bishedit::Buffer` -- rows of `bytes_per_row` bytes are its
// "lines", a byte is its "char" (Latin-1: `b as char`) -- and that one
// impl buys the entire motion vocabulary unchanged, straight out of
// `bishedit::motion`: `hjkl`, `w`/`b`/`e`/`W`/`B`/`E` (which, over
// Latin-1 bytes, genuinely mean "jump between printable runs" -- exactly
// what you want when hunting strings in a binary), `0`/`^`/`$`, `gg`/`G`,
// `f`/`F`/`t`/`T`/`;`/`,`, `H`/`M`/`L`, `Ctrl-D`/`U`/`F`/`B`, `zz`/`zt`/
// `zb`, `{`/`}`, marks and the jump list. Key decoding is
// `bishedit::vimkeys::VimKeys` verbatim, so counts, operator+motion
// composition, registers, Visual mode and `.`-repeat all behave exactly
// as they do in the file editor. Undo is `bishedit::undo::UndoTree`,
// which is already generic over its snapshot type -- `UndoTree<Vec<u8>>`
// needed no changes at all. Yank/put go through the real `Registers`, so
// `"ay` here and `"ap` in the file editor are the same register.
//
// Three places where the byte model genuinely differs from text, each a
// deliberate decision rather than an oversight:
//
//  1. **Extraction is done in byte space, never via
//     `motion::extract_text`.** That function inserts a `\n` at every
//     line boundary it crosses -- correct for text, corrupting for
//     bytes, and unrecoverable here since 0x0A is itself a perfectly
//     ordinary data byte. `Buffer::line_wraps` stays `false` (so `$`,
//     `0` and `yy` mean "this row", which is what a hex editor's rows
//     are for), and `byte_range` below converts a `MotionRange` straight
//     into a flat `[start, end)` byte span instead.
//
//  2. **Every register this editor writes is `RegisterShape::Char`**,
//     even from a linewise motion (`dd`, `dj`, `V`). A flat byte stream
//     has no notion of a line to re-insert as a whole, so a "linewise"
//     range here just means "these bytes"; keeping the shape would make
//     `p` invent a row boundary that doesn't exist in the data.
//
//  3. **Search is over the flat byte vector**, not
//     `motion::find_matches_in_line`. A row is a display artifact, so a
//     pattern straddling one -- overwhelmingly likely, at 16 bytes a row
//     -- must still match. See `parse_pattern` for how `/` decides
//     between a hex-byte pattern and a literal string.
//
// Scope: the file is read wholly into memory (`Vec<u8>`), which is the
// right trade for the sizes a terminal hex editor is actually used on
// and keeps every edit an ordinary `Vec` splice; a windowed/mmap'd model
// for multi-gigabyte files is a different program, not a flag on this
// one.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::bishedit::motion::{self, CaseKind, Motion, MotionRange, MotionShape};
use crate::bishedit::registers::{RegisterShape, RegisterValue, Registers};
use crate::bishedit::undo::UndoTree;
use crate::bishedit::vimkeys::{InsertCmd, KeyOutcome, Op, VimKeys, WindowCmd};
use crate::bishedit::Buffer;
use crate::editor::{self, Key};
use crate::history::History;
use crate::repl::{render_global_status_row, Rect};
use crate::vt100;

// Bytes per group, separated by an extra space in the hex column --
// `hexdump -C`/`xxd`'s own convention, and the reason a 16-byte row is
// readable at a glance instead of being one undifferentiated run.
const GROUP: usize = 8;
// Columns the data inspector needs before it's worth showing at all.
const INSPECTOR_WIDTH: usize = 26;

// ---------------------------------------------------------------------
// The buffer
// ---------------------------------------------------------------------

pub struct HexBuffer {
    bytes: Vec<u8>,
    path: Option<PathBuf>,
    // Absolute byte offset, not a (row, col) pair: rows are a function of
    // `bytes_per_row`, which `:set width` can change at any moment, so
    // storing the offset keeps the cursor meaning the same byte across a
    // reflow instead of sliding to a different one.
    offset: usize,
    bytes_per_row: usize,
    vtop: usize,
    vheight: usize,
    marks: HashMap<char, (usize, usize)>,
    dirty: bool,
    readonly: bool,
    // The named file doesn't exist yet -- opening one is legitimate
    // ("build a small binary from scratch"), but a typo'd path silently
    // presenting an empty buffer is a much worse failure here than in a
    // text editor, so it's marked in the status line until a `:w`
    // actually creates it. Same `[New]` marker vim uses.
    is_new: bool,
}

impl HexBuffer {
    pub fn open(path: Option<&Path>, bytes_per_row: usize, vheight: usize) -> io::Result<HexBuffer> {
        let bytes = match path {
            Some(p) if p.exists() => std::fs::read(p)?,
            _ => Vec::new(),
        };
        let mut buf = HexBuffer::from_bytes(bytes, bytes_per_row, vheight);
        buf.is_new = path.is_some_and(|p| !p.exists());
        buf.path = path.map(|p| p.to_path_buf());
        Ok(buf)
    }

    pub fn from_bytes(bytes: Vec<u8>, bytes_per_row: usize, vheight: usize) -> HexBuffer {
        HexBuffer { bytes, path: None, offset: 0, bytes_per_row, vtop: 0, vheight, marks: HashMap::new(), dirty: false, readonly: false, is_new: false }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    // Clamped to the last real byte -- the one-past-the-end position is
    // reachable only while inserting (see `set_offset_insert`), matching
    // vim's own rule that Normal mode never sits past the last character.
    pub fn set_offset(&mut self, off: usize) {
        self.offset = off.min(self.bytes.len().saturating_sub(1));
    }

    pub fn set_offset_insert(&mut self, off: usize) {
        self.offset = off.min(self.bytes.len());
    }

    pub fn byte_at(&self, off: usize) -> Option<u8> {
        self.bytes.get(off).copied()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn set_bytes_per_row(&mut self, bpr: usize) {
        self.bytes_per_row = bpr.max(1);
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn set_vheight(&mut self, rows: usize) {
        self.vheight = rows.max(1);
    }

    // Every row that has at least one byte in it. Deliberately *not*
    // widened by one for the append position at EOF: motions should stay
    // inside real data (a `G` that lands on a phantom empty row would be
    // a surprise), and the renderer draws that trailing caret itself when
    // Insert mode puts the cursor there.
    pub fn row_count(&self) -> usize {
        self.bytes.len().div_ceil(self.bytes_per_row).max(1)
    }

    // --- mutation, all in byte space -------------------------------

    pub fn splice(&mut self, start: usize, end: usize, with: &[u8]) -> Vec<u8> {
        let start = start.min(self.bytes.len());
        let end = end.clamp(start, self.bytes.len());
        let removed: Vec<u8> = self.bytes.splice(start..end, with.iter().copied()).collect();
        if !removed.is_empty() || !with.is_empty() {
            self.dirty = true;
        }
        removed
    }

    pub fn insert_at(&mut self, at: usize, data: &[u8]) {
        self.splice(at, at, data);
    }

    pub fn delete_range(&mut self, start: usize, end: usize) -> Vec<u8> {
        self.splice(start, end, &[])
    }

    // Overwrite in place, growing the file only if the write runs past
    // the current end -- `R` (and the hex pane's own `r`) shouldn't
    // shift everything after the cursor the way an insert does, since
    // preserving offsets is the entire point of overwrite mode in a
    // binary.
    pub fn overwrite_at(&mut self, at: usize, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            let pos = at + i;
            if pos < self.bytes.len() {
                self.bytes[pos] = b;
            } else {
                self.bytes.push(b);
            }
        }
        if !data.is_empty() {
            self.dirty = true;
        }
    }

    pub fn save(&mut self, to: Option<&Path>) -> io::Result<()> {
        let target = to.map(|p| p.to_path_buf()).or_else(|| self.path.clone());
        let Some(target) = target else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no file name"));
        };
        // Same rule (and same reasoning) as TextBuffer::save: read-only
        // stops this file being overwritten, not the bytes being written
        // somewhere else.
        if self.readonly && Some(&target) == self.path.as_ref() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "buffer is read-only"));
        }
        std::fs::write(&target, &self.bytes)?;
        if to.is_none() || self.path.is_none() {
            self.path = Some(target);
        }
        self.dirty = false;
        self.is_new = false;
        Ok(())
    }

    // A `MotionRange` (rows/cols, from the shared motion engine) turned
    // into the flat `[start, end)` byte span it actually covers. See this
    // module's own doc comment (point 1) for why this exists instead of
    // `motion::extract_text`.
    pub fn byte_range(&self, range: &MotionRange) -> (usize, usize) {
        let bpr = self.bytes_per_row;
        let off = |(r, c): (usize, usize)| r * bpr + c;
        let len = self.bytes.len();
        match range.shape {
            MotionShape::Linewise => ((range.from.0 * bpr).min(len), ((range.to.0 + 1) * bpr).min(len)),
            // The hex view has no blockwise selection of its own (nothing
            // emits one here), and a rectangle over a fixed-width dump is
            // the inclusive byte span anyway.
            MotionShape::Inclusive | MotionShape::Blockwise => (off(range.from).min(len), (off(range.to) + 1).min(len)),
            MotionShape::Exclusive => (off(range.from).min(len), off(range.to).min(len)),
        }
    }
}

impl Buffer for HexBuffer {
    fn line_count(&self) -> usize {
        self.row_count()
    }

    fn line_len(&self, line: usize) -> usize {
        let start = line * self.bytes_per_row;
        if start >= self.bytes.len() {
            0
        } else {
            (self.bytes.len() - start).min(self.bytes_per_row)
        }
    }

    // Latin-1, deliberately: it's the one mapping where every byte has a
    // distinct char and the round trip back is exact, which is what makes
    // the shared word/find/text-object motions operate on real byte
    // values rather than on a lossy decoding of them.
    fn char_at(&self, line: usize, col: usize) -> Option<char> {
        self.bytes.get(line * self.bytes_per_row + col).map(|&b| b as char)
    }

    fn cursor(&self) -> (usize, usize) {
        (self.offset / self.bytes_per_row, self.offset % self.bytes_per_row)
    }

    fn set_cursor(&mut self, line: usize, col: usize) {
        self.offset = (line * self.bytes_per_row + col).min(self.bytes.len().saturating_sub(1));
    }

    fn viewport_top(&self) -> usize {
        self.vtop
    }

    fn set_viewport_top(&mut self, line: usize) {
        self.vtop = line;
    }

    fn viewport_height(&self) -> usize {
        self.vheight
    }

    fn set_mark(&mut self, name: char, pos: (usize, usize)) {
        self.marks.insert(name, pos);
    }

    fn get_mark(&self, name: char) -> Option<(usize, usize)> {
        self.marks.get(&name).copied()
    }
}

// ---------------------------------------------------------------------
// Register interchange
// ---------------------------------------------------------------------

// Bytes -> the `String` a `RegisterValue` holds, one char per byte
// (Latin-1). Exact and reversible for every possible byte, and it means
// a yank taken here is a real register the file editor can put, rather
// than a second, parallel clipboard.
pub fn bytes_to_register_text(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

// The reverse, plus a rule for text that didn't come from here: a char
// that fits in a byte *is* that byte (so our own round trip is exact),
// and anything above U+00FF -- only reachable from a yank taken in the
// file editor -- contributes its UTF-8 encoding, which is what actually
// putting real text into a binary should mean.
pub fn register_text_to_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        if (c as u32) <= 0xFF {
            out.push(c as u32 as u8);
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

// ---------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum Pattern {
    Bytes(Vec<u8>),
    Text(Vec<u8>),
}

impl Pattern {
    pub fn needle(&self) -> &[u8] {
        match self {
            Pattern::Bytes(b) | Pattern::Text(b) => b,
        }
    }
}

// `/` in a hex editor is genuinely ambiguous -- `dead` is both a
// four-letter word and two bytes -- so rather than guess silently, the
// rule is explicit and the status line reports which reading was used:
//
//   /"dead"   literal text (quotes force it)
//   /dead     bytes DE AD  (parses cleanly as an even run of hex digits)
//   /hello    literal text (doesn't parse as hex)
//   /de ad be literal text (odd digit count -- not a whole number of bytes)
//
// Spaces are allowed inside a hex pattern (`/de ad be ef`) so a long one
// stays readable.
pub fn parse_pattern(input: &str) -> Option<Pattern> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        return Some(Pattern::Text(trimmed.as_bytes()[1..trimmed.len() - 1].to_vec()));
    }
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if !compact.is_empty() && compact.len().is_multiple_of(2) && compact.chars().all(|c| c.is_ascii_hexdigit()) {
        let bytes = compact
            .as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        return Some(Pattern::Bytes(bytes));
    }
    Some(Pattern::Text(trimmed.as_bytes().to_vec()))
}

// Plain forward/backward substring search over the flat byte vector,
// starting strictly after/before `from` so pressing `n` on a match moves
// to the next one instead of finding the same one again. Wraps around
// once, matching vim's own default `wrapscan`.
pub fn find_bytes(haystack: &[u8], needle: &[u8], from: usize, forward: bool) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let last_start = haystack.len() - needle.len();
    if forward {
        let begin = from.saturating_add(1).min(last_start + 1);
        (begin..=last_start).find(|&i| &haystack[i..i + needle.len()] == needle).or_else(|| (0..begin.min(last_start + 1)).find(|&i| &haystack[i..i + needle.len()] == needle))
    } else {
        let begin = from.min(last_start + 1);
        (0..begin).rev().find(|&i| &haystack[i..i + needle.len()] == needle).or_else(|| (begin..=last_start).rev().find(|&i| &haystack[i..i + needle.len()] == needle))
    }
}

// ---------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub bytes_per_row: usize,
    pub offset_width: usize,
    // Screen column (0-indexed, relative to the dump area) where the hex
    // column starts, and where the ASCII column's first character does.
    pub hex_col: usize,
    pub ascii_col: usize,
    pub dump_width: usize,
    pub rows: usize,
    // `Some(col)` when the terminal is wide enough to be worth showing
    // the data inspector beside the dump.
    pub inspector_col: Option<usize>,
}

// How many hex digits the largest offset in this file needs, rounded up
// to an even count and never below 8 -- so a small file still gets the
// familiar 8-digit `xxd` gutter instead of a cramped 2-digit one.
pub fn offset_width_for(len: usize) -> usize {
    let mut digits: usize = 1;
    let mut v = len;
    while v >= 16 {
        v /= 16;
        digits += 1;
    }
    digits.next_multiple_of(2).max(8)
}

// The screen column a byte's own two hex digits start at, within a row.
pub fn hex_cell_col(index: usize) -> usize {
    index * 3 + index / GROUP
}

// The width of the whole hex column: the last cell's own start plus its
// two digits and the trailing space. Derived from `hex_cell_col` rather
// than computed separately, so the column positions and the total width
// can never drift apart.
pub fn hex_span(bytes_per_row: usize) -> usize {
    if bytes_per_row == 0 {
        return 0;
    }
    hex_cell_col(bytes_per_row - 1) + 3
}

fn dump_width_for(bytes_per_row: usize, offset_width: usize) -> usize {
    offset_width + 2 + hex_span(bytes_per_row) + 1 + bytes_per_row + 1
}

// `bytes_per_row: None` means auto: the widest multiple of GROUP whose
// dump still fits the pane, which is how a wide pane gets 24 or 32 bytes
// a row instead of always 16.
//
// `rows`/`cols` are this frame's own *pane* rect, not the terminal --
// the status line lives on the terminal's shared global status row
// (`repl::render_global_status_row`), outside the rect entirely, so
// unlike a full-screen tool there's no row to reserve out of it here.
pub fn compute_layout(rows: usize, cols: usize, len: usize, bytes_per_row: Option<usize>, want_inspector: bool) -> Layout {
    let offset_width = offset_width_for(len);
    let widest_fitting = |budget: usize| -> usize {
        let mut best = GROUP;
        let mut candidate = GROUP;
        while dump_width_for(candidate, offset_width) <= budget {
            best = candidate;
            candidate += GROUP;
        }
        best
    };
    let bpr = match bytes_per_row {
        Some(n) => n.max(1),
        // Auto-width is deliberately *not* purely greedy: found via the
        // real terminal, a 110-column window fits exactly 24 bytes a row
        // and so silently squeezed the inspector out entirely -- losing a
        // headline feature to win 8 more bytes of dump. When the
        // inspector is wanted, the row is sized against the width left
        // over after reserving room for it, and only falls back to
        // filling the whole pane if even one group wouldn't fit
        // alongside it.
        None if want_inspector && dump_width_for(GROUP, offset_width) + INSPECTOR_WIDTH <= cols => {
            widest_fitting(cols - INSPECTOR_WIDTH)
        }
        None => widest_fitting(cols),
    };
    let dump_width = dump_width_for(bpr, offset_width);
    let inspector_col = (want_inspector && cols >= dump_width + INSPECTOR_WIDTH).then_some(dump_width + 2);
    Layout {
        bytes_per_row: bpr,
        offset_width,
        hex_col: offset_width + 2,
        ascii_col: offset_width + 2 + hex_span(bpr) + 1,
        dump_width,
        rows: rows.max(1),
        inspector_col,
    }
}

// ---------------------------------------------------------------------
// The interactive session
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pane {
    Hex,
    Ascii,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Normal,
    Insert,
    Replace,
    Visual,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Endian {
    Little,
    Big,
}

// What repl.rs's `hex_frames` side table actually holds, exactly as
// `fileeditor::EditSession` is what `edit_frames` holds -- the whole
// live view, so a mid-typed nibble, an in-progress Visual selection or
// an undo history all survive a Ctrl+Space detach and come back intact.
pub struct HexSession {
    buf: HexBuffer,
    vk: VimKeys,
    // The shell's own one shared register table, moved in and back out
    // at each focus boundary (`attach_registers`/`detach_registers`)
    // rather than borrowed: only the focused frame is ever driven, so
    // ownership transfer is exactly equivalent to sharing and keeps
    // every method below free of a `&mut Registers` parameter. It is
    // what makes this module's own claim true in practice -- a `"ay`
    // here really is the same register a `"ap` in the file editor reads.
    registers: Registers,
    undo: UndoTree<Vec<u8>>,
    mode: Mode,
    pane: Pane,
    // The high nibble typed so far, while entering a byte in the hex
    // pane. Shown live in the status line so a half-typed byte is never
    // invisible state.
    pending_nibble: Option<u8>,
    // `r` in the hex pane needs *two* digits, so it can't go through
    // `KeyOutcome::ReplaceChar` (which resolves after one) -- see
    // `handle_normal_key`'s own comment.
    pending_replace: Option<Option<u8>>,
    width_override: Option<usize>,
    inspector: bool,
    endian: Endian,
    last_search: Option<(Vec<u8>, bool)>,
    // The undo-tree node whose content is what's actually on disk --
    // exactly `TextBuffer::saved_node`'s own trick, so undoing all the
    // way back to the saved state clears the `[+]` marker instead of
    // leaving it stuck on forever.
    saved_node: usize,
    status: Option<String>,
}

impl HexSession {
    // `rect` only seeds the initial geometry -- every later frame
    // recomputes it (see `prepare`), so a resize or a split between two
    // keystrokes is honored on the very next one.
    // A hex session over bytes that aren't simply what's at `path` --
    // a member inside an archive, or a gzip'd file (see repl.rs's own
    // open_one_edit_target). Always read-only: those bytes came out of a
    // decompressor, and nothing here can put them back.
    pub fn from_bytes(bytes: Vec<u8>, path: &Path, rect: Rect) -> io::Result<HexSession> {
        let layout = compute_layout(rect.rows, rect.cols, 0, None, true);
        let mut buf = HexBuffer::from_bytes(bytes, layout.bytes_per_row, layout.rows);
        buf.path = Some(path.to_path_buf());
        buf.readonly = true;
        Ok(HexSession::from_buffer(buf))
    }

    pub fn open(path: Option<&Path>, rect: Rect, readonly: bool) -> io::Result<HexSession> {
        let layout = compute_layout(rect.rows, rect.cols, 0, None, true);
        let mut buf = HexBuffer::open(path, layout.bytes_per_row, layout.rows)?;
        buf.readonly = readonly;
        Ok(HexSession::from_buffer(buf))
    }

    // The shared tail of both constructors above.
    fn from_buffer(buf: HexBuffer) -> HexSession {
        let undo = UndoTree::new(buf.bytes.clone(), (0, 0));
        HexSession {
            buf,
            vk: VimKeys::new(),
            // A placeholder: `attach_registers` replaces this with the
            // shell's own shared table the moment this frame is first
            // driven, and it is never read before then.
            registers: Registers::default(),
            undo,
            mode: Mode::Normal,
            pane: Pane::Hex,
            pending_nibble: None,
            pending_replace: None,
            width_override: None,
            inspector: true,
            endian: Endian::Little,
            last_search: None,
            saved_node: 0,
            status: None,
        }
    }

    // See the `registers` field's own doc comment: the shared table moves
    // in when this frame takes focus and back out when it loses it.
    pub fn attach_registers(&mut self, registers: &mut Registers) {
        self.registers = std::mem::take(registers);
    }

    pub fn detach_registers(&mut self, registers: &mut Registers) {
        *registers = std::mem::take(&mut self.registers);
    }

    fn layout(&self, rect: Rect) -> Layout {
        compute_layout(rect.rows, rect.cols, self.buf.len(), self.width_override, self.inspector)
    }

    // Deliberately *not* called before each individual edit: like the
    // file editor (`render_nav_frame`'s own `checkpoint_undo` call), this
    // runs once per frame, right before rendering, so the tree's current
    // node always holds exactly what's on screen and a whole Insert-mode
    // session collapses into one undo step instead of one per keystroke.
    // `UndoTree::checkpoint` de-dupes identical content, so a frame where
    // nothing changed costs nothing.
    fn checkpoint(&mut self) {
        let cursor = self.buf.cursor();
        self.undo.checkpoint(&self.buf.bytes, cursor);
        self.buf.dirty = self.undo.current_id() != self.saved_node;
    }

    fn restore(&mut self, snapshot: Option<(Vec<u8>, (usize, usize))>) -> bool {
        let Some((content, cursor)) = snapshot else { return false };
        self.buf.bytes = content;
        self.buf.set_cursor(cursor.0, cursor.1);
        true
    }

    fn scroll_to_cursor(&mut self, rows: usize) {
        let (row, _) = self.buf.cursor();
        self.buf.set_vheight(rows);
        if row < self.buf.vtop {
            self.buf.vtop = row;
        } else if row >= self.buf.vtop + rows {
            self.buf.vtop = row + 1 - rows;
        }
        let max_top = self.buf.row_count().saturating_sub(rows);
        self.buf.vtop = self.buf.vtop.min(max_top);
    }

    // The byte span Visual mode currently covers, or `None` outside it.
    fn visual_range(&self) -> Option<(usize, usize)> {
        let (shape, anchor) = self.vk.visual_anchor()?;
        let bpr = self.buf.bytes_per_row;
        let anchor_off = anchor.0 * bpr + anchor.1;
        let cursor_off = self.buf.offset();
        let (lo, hi) = if anchor_off <= cursor_off { (anchor_off, cursor_off) } else { (cursor_off, anchor_off) };
        Some(match shape {
            // Blockwise never arises here (`Ctrl-V` is not bound in this
            // view), and a rectangle over a fixed-width dump is the same
            // flat span charwise selection already gives.
            RegisterShape::Char | RegisterShape::Block => (lo, (hi + 1).min(self.buf.len())),
            // `V` selects whole rows -- still handed back as a flat byte
            // span (see this module's own doc comment, point 2).
            RegisterShape::Line => ((lo / bpr) * bpr, ((hi / bpr + 1) * bpr).min(self.buf.len())),
        })
    }

    fn write_register(&mut self, register: Option<char>, bytes: &[u8], deleted: bool) {
        let value = RegisterValue { text: bytes_to_register_text(bytes), shape: RegisterShape::Char };
        if deleted {
            self.registers.record_delete(register, value);
        } else {
            self.registers.record_yank(register, value);
        }
    }

    fn apply_op(&mut self, op: Op, start: usize, end: usize, register: Option<char>) {
        if end <= start {
            return;
        }
        match op {
            Op::Yank => {
                let bytes = self.buf.bytes[start..end].to_vec();
                self.write_register(register, &bytes, false);
                self.buf.set_offset(start);
                self.status = Some(format!("{} bytes yanked", bytes.len()));
            }
            Op::Delete | Op::Change => {
                let removed = self.buf.delete_range(start, end);
                self.write_register(register, &removed, true);
                if op == Op::Change {
                    self.buf.set_offset_insert(start);
                    self.mode = Mode::Insert;
                } else {
                    self.buf.set_offset(start);
                }
            }
            // Case operators are meaningful on bytes for exactly the same
            // reason word motions are: the ASCII range inside a binary is
            // real text. Bytes with no case (and anything whose transform
            // would escape a single byte) are left untouched.
            Op::Lowercase | Op::Uppercase | Op::CaseToggle => {
                let kind = match op {
                    Op::Lowercase => CaseKind::Lower,
                    Op::Uppercase => CaseKind::Upper,
                    _ => CaseKind::Toggle,
                };
                for b in &mut self.buf.bytes[start..end] {
                    let transformed = motion::case_transform(*b as char, kind);
                    if (transformed as u32) <= 0xFF {
                        *b = transformed as u32 as u8;
                    }
                }
                self.buf.dirty = true;
                self.buf.set_offset(start);
            }
            Op::Indent | Op::Outdent => {
                self.status = Some("indent has no meaning in a byte buffer".to_string());
            }
        }
    }

    fn put(&mut self, before: bool, count: Option<usize>, register: Option<char>) {
        let value = self.registers.read(register);
        let bytes = register_text_to_bytes(&value.text);
        if bytes.is_empty() {
            self.status = Some("register is empty".to_string());
            return;
        }
        let at = if before { self.buf.offset() } else { (self.buf.offset() + 1).min(self.buf.len()) };
        let mut block = Vec::with_capacity(bytes.len() * count.unwrap_or(1).max(1));
        for _ in 0..count.unwrap_or(1).max(1) {
            block.extend_from_slice(&bytes);
        }
        self.buf.insert_at(at, &block);
        self.buf.set_offset(at + block.len() - 1);
        self.status = Some(format!("{} bytes put", block.len()));
    }

    fn search(&mut self, needle: Vec<u8>, forward: bool, describe: Option<&Pattern>) {
        if needle.is_empty() {
            return;
        }
        match find_bytes(self.buf.bytes(), &needle, self.buf.offset(), forward) {
            Some(at) => {
                self.vk.push_jump(self.buf.cursor());
                self.buf.set_offset(at);
                let what = match describe {
                    Some(Pattern::Bytes(b)) => format!("{} byte{} {}", b.len(), if b.len() == 1 { "" } else { "s" }, hex_list(b)),
                    Some(Pattern::Text(t)) => format!("text {:?}", String::from_utf8_lossy(t)),
                    None => format!("{} bytes", needle.len()),
                };
                self.status = Some(format!("found {what} at 0x{at:X}"));
            }
            None => self.status = Some("pattern not found".to_string()),
        }
        self.last_search = Some((needle, forward));
    }
}

fn hex_list(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------

fn printable(b: u8) -> Option<char> {
    (0x20..0x7F).contains(&b).then_some(b as char)
}

// A byte's own colour in the dump, before any cursor/selection styling is
// layered on top: printable ASCII stands out (that's what makes embedded
// strings findable by eye), a zero byte recedes (padding/alignment is
// usually noise), everything else is plain.
fn byte_color(b: u8) -> &'static str {
    if b == 0 {
        "\x1b[2m"
    } else if printable(b).is_some() {
        "\x1b[36m"
    } else {
        "\x1b[39m"
    }
}

// The dump itself, addressed against `row_origin`/`col_origin` rather
// than the terminal's own top-left -- exactly `fileeditor::
// build_editor_frame`'s own convention, and for the same two reasons:
// the live render passes this pane's absolute position, while `freeze`
// passes `0`/`0` because a session's own vt100 grid is addressed
// pane-relative (see render_compositor_frame's per-pane loop).
//
// Every row is padded out to `rect.cols` instead of ending with an
// `\x1b[K`: erasing to the real end of the line would reach straight
// across a vertical split into whoever owns the columns to the right.
// That padding is also what makes rows past the end of the file blank
// themselves out, with no full-screen clear anywhere in here.
fn build_frame(session: &HexSession, layout: Layout, rect: Rect, row_origin: usize, col_origin: usize) -> String {
    let buf = &session.buf;
    let selection = session.visual_range();
    let cursor = buf.offset();
    let mut out = String::new();

    for screen_row in 0..layout.rows.min(rect.rows) {
        let row = buf.vtop + screen_row;
        out.push_str(&format!("\x1b[{};{}H", row_origin + screen_row + 1, col_origin + 1));
        let row_start = row * layout.bytes_per_row;
        if row_start > buf.len() || (row_start == buf.len() && row_start != cursor) {
            out.push_str(&" ".repeat(rect.cols));
            continue;
        }
        out.push_str(&format!("\x1b[2m{:0width$x}\x1b[0m  ", row_start, width = layout.offset_width));

        // Hex column.
        let mut hex = String::new();
        for i in 0..layout.bytes_per_row {
            let off = row_start + i;
            if i > 0 {
                hex.push(' ');
                if i.is_multiple_of(GROUP) {
                    hex.push(' ');
                }
            }
            match buf.byte_at(off) {
                Some(b) => hex.push_str(&styled(&format!("{b:02x}"), byte_color(b), off, cursor, selection, session.pane == Pane::Hex, session)),
                // Past the end: the append caret when Insert mode is
                // sitting there, blanks otherwise.
                None if off == cursor && session.mode != Mode::Normal => hex.push_str("\x1b[7m  \x1b[0m"),
                None => hex.push_str("  "),
            }
        }
        out.push_str(&hex);

        // ASCII column.
        out.push_str(" \x1b[2m|\x1b[0m");
        for i in 0..layout.bytes_per_row {
            let off = row_start + i;
            match buf.byte_at(off) {
                Some(b) => {
                    let (glyph, color) = match printable(b) {
                        Some(c) => (c.to_string(), byte_color(b)),
                        None => (".".to_string(), "\x1b[2m"),
                    };
                    out.push_str(&styled(&glyph, color, off, cursor, selection, session.pane == Pane::Ascii, session));
                }
                None if off == cursor && session.mode != Mode::Normal => out.push_str("\x1b[7m \x1b[0m"),
                None => out.push(' '),
            }
        }
        out.push_str("\x1b[2m|\x1b[0m");
        // Every drawn row is exactly `dump_width` columns wide (that's
        // what dump_width_for adds up), so what's left to blank out is
        // the rest of the pane.
        out.push_str(&" ".repeat(rect.cols.saturating_sub(layout.dump_width)));
    }

    if let Some(col) = layout.inspector_col {
        out.push_str(&render_inspector(session, layout, rect, row_origin, col_origin + col));
    }
    // The block cursor is drawn by this frame's own reverse-video cell,
    // so the terminal's real cursor would only ever be a second, wrong
    // one somewhere else.
    out.push_str("\x1b[?25l");
    out
}

// Layers cursor/selection styling over a cell's own base colour.
// `active` distinguishes the pane the keyboard is actually editing (a
// full reverse-video block) from its mirror in the other pane (an
// underline), so it's always obvious which of the two a keystroke will
// land in -- the one piece of state a two-pane hex editor absolutely
// must show.
fn styled(text: &str, color: &str, off: usize, cursor: usize, selection: Option<(usize, usize)>, active: bool, session: &HexSession) -> String {
    let selected = selection.is_some_and(|(s, e)| off >= s && off < e);
    let is_cursor = off == cursor;
    let mut out = String::new();
    if is_cursor {
        out.push_str(if active { "\x1b[7m\x1b[1m" } else { "\x1b[4m" });
    } else if selected {
        out.push_str("\x1b[7m");
    }
    if !is_cursor || !active {
        out.push_str(color);
    }
    // A half-typed byte in the hex pane shows its high nibble in place,
    // so the cell being built is never invisible.
    if is_cursor && active && session.pane == Pane::Hex && let Some(n) = session.pending_nibble {
        out.push_str(&format!("{n:x}_"));
    } else {
        out.push_str(text);
    }
    out.push_str("\x1b[0m");
    out
}

// `col` is already absolute (the caller adds `col_origin`); `row_origin`
// positions it against the pane the same way build_frame's own rows are.
fn render_inspector(session: &HexSession, layout: Layout, rect: Rect, row_origin: usize, col: usize) -> String {
    let bytes = session.buf.bytes();
    let off = session.buf.offset();
    let le = session.endian == Endian::Little;
    let read = |n: usize| -> Option<u64> {
        if off + n > bytes.len() {
            return None;
        }
        let slice = &bytes[off..off + n];
        let mut v: u64 = 0;
        if le {
            for (i, &b) in slice.iter().enumerate() {
                v |= (b as u64) << (8 * i);
            }
        } else {
            for &b in slice {
                v = (v << 8) | b as u64;
            }
        }
        Some(v)
    };
    let signed = |v: u64, bits: u32| -> i64 {
        let shift = 64 - bits;
        ((v << shift) as i64) >> shift
    };
    let mut lines: Vec<(String, String)> = Vec::new();
    lines.push(("endian".to_string(), if le { "little".to_string() } else { "big".to_string() }));
    if let Some(v) = read(1) {
        lines.push(("u8".to_string(), format!("{v}")));
        lines.push(("i8".to_string(), format!("{}", signed(v, 8))));
        lines.push(("bin".to_string(), format!("{:08b}", v as u8)));
        lines.push(("oct".to_string(), format!("{:03o}", v as u8)));
    }
    if let Some(v) = read(2) {
        lines.push(("u16".to_string(), format!("{v}")));
        lines.push(("i16".to_string(), format!("{}", signed(v, 16))));
    }
    if let Some(v) = read(4) {
        lines.push(("u32".to_string(), format!("{v}")));
        lines.push(("i32".to_string(), format!("{}", signed(v, 32))));
        lines.push(("f32".to_string(), format_float(f32::from_bits(v as u32) as f64)));
    }
    if let Some(v) = read(8) {
        lines.push(("u64".to_string(), format!("{v}")));
        lines.push(("i64".to_string(), format!("{}", signed(v, 64))));
        lines.push(("f64".to_string(), format_float(f64::from_bits(v))));
    }

    let width = rect.cols.saturating_sub(layout.dump_width + 2);
    let mut out = String::new();
    out.push_str(&format!("\x1b[{};{}H\x1b[1m{}\x1b[0m", row_origin + 1, col + 1, fit(&format!("inspector @ 0x{off:x}"), width)));
    for (i, (name, value)) in lines.iter().enumerate() {
        let row = i + 2;
        if row > layout.rows.min(rect.rows) {
            break;
        }
        out.push_str(&format!("\x1b[{};{}H", row_origin + row, col + 1));
        let text = format!("{name:<7}{value}");
        out.push_str(&format!("\x1b[2m{:<7}\x1b[0m{}", name, fit(&text[7.min(text.len())..], width.saturating_sub(7))));
    }
    out
}

// Random bytes reinterpreted as a float are usually a denormal or an
// astronomically large value, and `{:.6}` renders either as an
// unreadable run of digits (an f64 came out 57 characters wide in the
// real inspector). Scientific notation outside a sane magnitude window
// keeps every row the same shape.
fn format_float(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    if !v.is_finite() {
        return if v.is_nan() { "NaN".to_string() } else { format!("{v}") };
    }
    let magnitude = v.abs();
    if (1e-4..1e12).contains(&magnitude) {
        format!("{v:.6}")
    } else {
        format!("{v:.6e}")
    }
}

// The text for the terminal's shared global status row (see
// `repl::render_global_status_row`, which does the positioning and the
// reverse-video styling) -- `term_cols`, not the pane's width, because
// that row spans the whole terminal regardless of how the panes below it
// are split.
fn status_text(session: &HexSession, layout: Layout, term_cols: usize) -> String {
    let buf = &session.buf;
    // The file's own name, not its whole path -- same as vim's own
    // status line, and found necessary in practice: a deep absolute path
    // is easily 80+ columns on its own, which pushed every actually
    // useful field (offset, byte value, mode) straight off the row.
    let name = buf
        .path()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "[no name]".to_string());
    let dirty = if buf.is_dirty() { " [+]" } else { "" };
    let ro = if buf.readonly { " [RO]" } else { "" };
    let new = if buf.is_new { " [new]" } else { "" };
    let detail = match buf.byte_at(buf.offset) {
        Some(b) => {
            let glyph = printable(b).map(|c| format!(" '{c}'")).unwrap_or_default();
            format!("{b:02x}{glyph}  {b}  0{b:o}  0b{b:08b}")
        }
        None => "eof".to_string(),
    };
    let percent = if buf.len() == 0 { 0 } else { buf.offset * 100 / buf.len().max(1) };
    let left = format!("{name}{new}{dirty}{ro}  0x{:0width$x}/0x{:x}  {}%  {detail}", buf.offset, buf.len(), percent, width = layout.offset_width);

    let mode = match session.mode {
        Mode::Normal => "NORMAL",
        Mode::Insert => "INSERT",
        Mode::Replace => "REPLACE",
        Mode::Visual => "VISUAL",
    };
    let pane = match session.pane {
        Pane::Hex => "hex",
        Pane::Ascii => "ascii",
    };
    let pending = session.vk.pending_display();
    let right = format!("{pending}  [{pane}]  -- {mode} --");

    // A transient message (yank counts, search results, errors) takes the
    // whole row for one frame -- it's the same "say what just happened"
    // slot the file editor's own status line uses.
    let text = match &session.status {
        Some(msg) => fit(msg, term_cols),
        None => {
            let used = display_len(&left) + display_len(&right);
            if used + 2 > term_cols {
                fit(&left, term_cols)
            } else {
                format!("{left}{}{right}", " ".repeat(term_cols - used))
            }
        }
    };
    format!("{text}{}", " ".repeat(term_cols.saturating_sub(display_len(&text))))
}

fn display_len(s: &str) -> usize {
    s.chars().count()
}

fn fit(s: &str, width: usize) -> String {
    if display_len(s) <= width {
        return s.to_string();
    }
    s.chars().take(width).collect()
}

// ---------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------

fn hex_digit(c: char) -> Option<u8> {
    c.to_digit(16).map(|d| d as u8)
}

impl HexSession {
    fn handle_insert_key(&mut self, key: Key) {
        match key {
            Key::Escape => {
                self.pending_nibble = None;
                self.mode = Mode::Normal;
                self.buf.set_offset(self.buf.offset.saturating_sub(1));
            }
            Key::Backspace => {
                if self.pending_nibble.take().is_some() {
                    return;
                }
                if self.buf.offset > 0 {
                    self.buf.delete_range(self.buf.offset - 1, self.buf.offset);
                    self.buf.set_offset_insert(self.buf.offset - 1);
                }
            }
            Key::Tab => self.pane = if self.pane == Pane::Hex { Pane::Ascii } else { Pane::Hex },
            Key::Left => self.buf.set_offset_insert(self.buf.offset.saturating_sub(1)),
            Key::Right => self.buf.set_offset_insert(self.buf.offset + 1),
            Key::Up => self.buf.set_offset_insert(self.buf.offset.saturating_sub(self.buf.bytes_per_row)),
            Key::Down => self.buf.set_offset_insert(self.buf.offset + self.buf.bytes_per_row),
            Key::Char(c) => match self.pane {
                // Nibble at a time: the first hex digit is held (and
                // shown in the cell, see `styled`), the second commits
                // the whole byte.
                Pane::Hex => {
                    let Some(nib) = hex_digit(c) else {
                        self.status = Some(format!("'{c}' is not a hex digit"));
                        return;
                    };
                    match self.pending_nibble.take() {
                        None => self.pending_nibble = Some(nib),
                        Some(high) => {
                            let byte = (high << 4) | nib;
                            let at = self.buf.offset;
                            if self.mode == Mode::Replace {
                                self.buf.overwrite_at(at, &[byte]);
                            } else {
                                self.buf.insert_at(at, &[byte]);
                            }
                            self.buf.set_offset_insert(at + 1);
                        }
                    }
                }
                // Typing real text into a binary should insert what the
                // character actually is, so a non-ASCII char contributes
                // its UTF-8 bytes rather than being refused.
                Pane::Ascii => {
                    let mut tmp = [0u8; 4];
                    let bytes = c.encode_utf8(&mut tmp).as_bytes().to_vec();
                    let at = self.buf.offset;
                    if self.mode == Mode::Replace {
                        self.buf.overwrite_at(at, &bytes);
                    } else {
                        self.buf.insert_at(at, &bytes);
                    }
                    self.buf.set_offset_insert(at + bytes.len());
                }
            },
            _ => {}
        }
    }

    fn enter_insert(&mut self, cmd: InsertCmd) {
        if self.readonly_refused() {
            return;
        }
        let bpr = self.buf.bytes_per_row;
        let (row, _) = self.buf.cursor();
        let row_start = row * bpr;
        let row_end = (row_start + self.buf.line_len(row)).min(self.buf.len());
        match cmd {
            InsertCmd::Before | InsertCmd::LastInsertPos => {}
            InsertCmd::After => self.buf.set_offset_insert(self.buf.offset + 1),
            InsertCmd::LineStart => self.buf.set_offset_insert(row_start),
            InsertCmd::LineEnd => self.buf.set_offset_insert(row_end),
            InsertCmd::SubstituteChar => {
                let at = self.buf.offset;
                let removed = self.buf.delete_range(at, (at + 1).min(self.buf.len()));
                self.write_register(None, &removed, true);
                self.buf.set_offset_insert(at);
            }
            InsertCmd::SubstituteLine => {
                let removed = self.buf.delete_range(row_start, row_end);
                self.write_register(None, &removed, true);
                self.buf.set_offset_insert(row_start);
            }
            InsertCmd::ChangeToEnd => {
                let at = self.buf.offset;
                let removed = self.buf.delete_range(at, row_end);
                self.write_register(None, &removed, true);
                self.buf.set_offset_insert(at);
            }
        }
        self.mode = Mode::Insert;
        self.pending_nibble = None;
    }

    fn readonly_refused(&mut self) -> bool {
        if self.buf.readonly {
            self.status = Some("buffer is read-only (opened with `e --readonly`)".to_string());
            return true;
        }
        false
    }

    // Returns the `<C-w>` window command this keystroke resolved to, if
    // any -- handled by repl.rs (which owns focus and the layout), not
    // here. Everything else is applied in place.
    fn handle_outcome(&mut self, outcome: KeyOutcome, rows: usize) -> Option<(WindowCmd, Option<usize>)> {
        if let KeyOutcome::Window(cmd, count) = outcome {
            return Some((cmd, count));
        }
        match outcome {
            KeyOutcome::Pending | KeyOutcome::None => {}
            // `/`/`?` come through as ordinary motions from the shared key
            // decoder, but resolve against the flat byte vector here (see
            // this module's doc comment, point 3) instead of the shared
            // per-line regex search.
            KeyOutcome::Motion(Motion::SearchForward(p) | Motion::SearchBackward(p), _) if p.is_empty() => {
                if let Some((needle, forward)) = self.last_search.clone() {
                    self.search(needle, forward, None);
                }
            }
            KeyOutcome::Motion(Motion::SearchForward(p), _) => {
                if let Some(pattern) = parse_pattern(&p) {
                    self.search(pattern.needle().to_vec(), true, Some(&pattern));
                }
            }
            KeyOutcome::Motion(Motion::SearchBackward(p), _) => {
                if let Some(pattern) = parse_pattern(&p) {
                    self.search(pattern.needle().to_vec(), false, Some(&pattern));
                }
            }
            KeyOutcome::Motion(m, count) => {
                if motion::is_jump(&m) {
                    let here = self.buf.cursor();
                    self.vk.push_jump(here);
                }
                self.buf.set_vheight(rows);
                motion::apply_motion(&mut self.buf, m, count);
            }
            KeyOutcome::Operator(op, m, count, register) => {
                self.buf.set_vheight(rows);
                if let Some(range) = motion::motion_range(&mut self.buf, m, count) {
                    let (start, end) = self.buf.byte_range(&range);
                    if op != Op::Yank && self.readonly_refused() {
                        return None;
                    }
                    self.apply_op(op, start, end, register);
                }
            }
            KeyOutcome::OperatorLines(op, count, register) => {
                let bpr = self.buf.bytes_per_row;
                let (row, _) = self.buf.cursor();
                let start = row * bpr;
                let end = ((row + count.unwrap_or(1).max(1)) * bpr).min(self.buf.len());
                if op != Op::Yank && self.readonly_refused() {
                    return None;
                }
                self.apply_op(op, start, end, register);
            }
            KeyOutcome::Put { before, count, register } => {
                if !self.readonly_refused() {
                    self.put(before, count, register);
                }
            }
            KeyOutcome::DeleteCharForward { count, register } => {
                if self.readonly_refused() {
                    return None;
                }
                let start = self.buf.offset;
                let end = (start + count.unwrap_or(1).max(1)).min(self.buf.len());
                if end > start {
                    let removed = self.buf.delete_range(start, end);
                    self.write_register(register, &removed, true);
                    self.buf.set_offset(start);
                }
            }
            KeyOutcome::EnterInsert(cmd) => self.enter_insert(cmd),
            KeyOutcome::EnterReplace => {
                if !self.readonly_refused() {
                    self.mode = Mode::Replace;
                    self.pending_nibble = None;
                }
            }
            KeyOutcome::ReplaceChar { ch, count } => {
                // Only the ASCII pane resolves `r` this way -- the hex
                // pane intercepts `r` before `feed` ever sees it, since a
                // byte there takes two digits (see `handle_normal_key`).
                if self.readonly_refused() {
                    return None;
                }
                let mut tmp = [0u8; 4];
                let bytes = ch.encode_utf8(&mut tmp).as_bytes().to_vec();
                let n = count.unwrap_or(1).max(1);
                let mut all = Vec::new();
                for _ in 0..n {
                    all.extend_from_slice(&bytes);
                }
                let at = self.buf.offset;
                self.buf.overwrite_at(at, &all);
                self.buf.set_offset(at + all.len() - 1);
            }
            KeyOutcome::ToggleCase { count } => {
                if self.readonly_refused() {
                    return None;
                }
                let start = self.buf.offset;
                let end = (start + count.unwrap_or(1).max(1)).min(self.buf.len());
                for b in &mut self.buf.bytes[start..end] {
                    let t = motion::case_transform(*b as char, CaseKind::Toggle);
                    if (t as u32) <= 0xFF {
                        *b = t as u32 as u8;
                    }
                }
                self.buf.dirty = true;
                self.buf.set_offset(end.saturating_sub(1).max(start));
            }
            // Ctrl-A/Ctrl-X on a byte value -- the natural hex-editor
            // reading of vim's own increment/decrement, wrapping rather
            // than saturating so 0xff+1 is 0x00.
            KeyOutcome::AdjustNumber { delta } => {
                if self.readonly_refused() {
                    return None;
                }
                if let Some(b) = self.buf.byte_at(self.buf.offset) {
                    let at = self.buf.offset;
                    self.buf.bytes[at] = (b as i64).wrapping_add(delta) as u8;
                    self.buf.dirty = true;
                }
            }
            KeyOutcome::Undo(count) => {
                let mut moved = false;
                for _ in 0..count.unwrap_or(1).max(1) {
                    let snap = self.undo.undo().map(|s| (s.content.clone(), s.cursor));
                    moved |= self.restore(snap);
                }
                self.status = Some(if moved { "undo".to_string() } else { "already at oldest change".to_string() });
            }
            KeyOutcome::Redo(count) => {
                let mut moved = false;
                for _ in 0..count.unwrap_or(1).max(1) {
                    let snap = self.undo.redo().map(|s| (s.content.clone(), s.cursor));
                    moved |= self.restore(snap);
                }
                self.status = Some(if moved { "redo".to_string() } else { "already at newest change".to_string() });
            }
            KeyOutcome::UndoSeq { forward, count } => {
                for _ in 0..count.unwrap_or(1).max(1) {
                    let snap = if forward { self.undo.time_travel_forward() } else { self.undo.time_travel_back() };
                    let snap = snap.map(|s| (s.content.clone(), s.cursor));
                    self.restore(snap);
                }
            }
            KeyOutcome::EnterVisual(shape) => {
                let anchor = self.buf.cursor();
                self.vk.begin_visual(shape, anchor);
                self.mode = Mode::Visual;
            }
            // A hex view of a binary file has no identifiers and no
            // language server; `gd`/`gr` simply do nothing, the same
            // way every other outcome this view has no meaning for does.
            KeyOutcome::GotoDefinition(_) | KeyOutcome::GotoReferences | KeyOutcome::DocumentSymbols | KeyOutcome::CodeActions => {}
            KeyOutcome::ReselectVisual => {
                if let Some((shape, anchor, cursor)) = self.vk.last_visual() {
                    self.buf.set_cursor(cursor.0, cursor.1);
                    self.vk.begin_visual(shape, anchor);
                    self.mode = Mode::Visual;
                }
            }
            KeyOutcome::Jump { forward } => {
                let here = self.buf.cursor();
                let target = if forward { self.vk.jump_forward(here) } else { self.vk.jump_back(here) };
                if let Some((r, c)) = target {
                    self.buf.set_cursor(r, c);
                }
            }
            // Everything below is real vim vocabulary with no coherent
            // meaning over a flat byte stream -- said out loud rather
            // than silently swallowed, so a key that does nothing is
            // never mistaken for a key that didn't register.
            KeyOutcome::Join { .. } => self.status = Some("J: rows are a display artifact here, not lines to join".to_string()),
            KeyOutcome::OpenLine { .. } => self.status = Some("o/O: rows are a display artifact here -- use i/a to insert bytes".to_string()),
            KeyOutcome::AddSurround { .. } | KeyOutcome::DeleteSurround { .. } | KeyOutcome::ChangeSurround { .. } => {
                self.status = Some("surround has no meaning in a byte buffer".to_string())
            }
            KeyOutcome::Window(..) => unreachable!("bubbled up before this match"),
        }
        None
    }

    // Visual mode's own operators, which act on the live selection rather
    // than on a motion range.
    fn handle_visual_key(&mut self, key: Key) -> bool {
        let register = self.vk.take_pending_register();
        let (op, deleted) = match key {
            Key::Char('y') => (Op::Yank, false),
            Key::Char('d') | Key::Char('x') => (Op::Delete, true),
            Key::Char('c') | Key::Char('s') => (Op::Change, true),
            Key::Char('u') => (Op::Lowercase, true),
            Key::Char('U') => (Op::Uppercase, true),
            Key::Char('~') => (Op::CaseToggle, true),
            _ => return false,
        };
        let Some((start, end)) = self.visual_range() else { return false };
        if deleted && self.readonly_refused() {
            return true;
        }
        self.apply_op(op, start, end, register);
        let cursor = self.buf.cursor();
        self.vk.end_visual(cursor);
        if self.mode == Mode::Visual {
            self.mode = Mode::Normal;
        }
        true
    }
}

// ---------------------------------------------------------------------
// Colon commands
// ---------------------------------------------------------------------

const HELP_TEXT: &str = "\
hex editor (e --hex) -- quick reference (:help)
  Motions are bishedit's own: h j k l | w b e W B E | 0 ^ $ | gg G | f F
  t T ; , | H M L | Ctrl-D/U/F/B | zz zt zb | m{a-z} `{m} | Ctrl-O/I. A
  row of bytes is a \"line\", a byte is a \"char\", so w/b hop printable runs.

  Tab           switch between the hex and ASCII panes
  i a I A       insert bytes (hex pane: two hex digits per byte)
  R             overwrite instead of inserting (offsets preserved)
  r             replace the byte under the cursor
  x d c y p P   delete / change / yank / put, with counts and \"registers
  v V           visual selection (byte-wise / whole rows), then y d c u U
  ~ Ctrl-A/X    toggle case / increment / decrement the byte
  u Ctrl-R      undo / redo (g- and g+ time travel too)
  / ? n N       search: /dead is bytes DE AD, /\"dead\" is literal text

  :w [FILE]     write (:wq/:x write+quit, :q close, :q! discard+close)
  :goto EXPR    jump to an offset -- 0x1f00, 4096, +16, $ (or bare :0x1f00)
  :set width N  bytes per row (N, or `auto` to fit the pane)
  :set endian   little | big -- which way the inspector reads integers
  :inspect      toggle the data inspector column
  :help         this screen
  <C-w>...      window commands: this frame stays open, like an `e` one";

fn parse_offset(expr: &str, current: usize, len: usize) -> Option<usize> {
    let expr = expr.trim();
    let (sign, rest) = match expr.strip_prefix('+') {
        Some(r) => (1i64, r.trim()),
        None => match expr.strip_prefix('-') {
            Some(r) => (-1i64, r.trim()),
            None => (0i64, expr),
        },
    };
    let value = if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        usize::from_str_radix(hex, 16).ok()?
    } else if rest == "$" {
        len.saturating_sub(1)
    } else {
        rest.parse::<usize>().ok()?
    };
    let target = if sign == 0 { value as i64 } else { current as i64 + sign * value as i64 };
    Some(target.clamp(0, len.saturating_sub(1) as i64) as usize)
}

impl HexSession {
    // Returns `true` when the command asked to close this frame
    // (`:q`/`:q!`/`:wq`/`:x`) -- what to actually *do* about that is
    // repl.rs's business, since it owns the pane's frame stack.
    fn dispatch_colon(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() {
            return false;
        }
        let (cmd, arg) = match line.split_once(' ') {
            Some((c, a)) => (c, Some(a.trim()).filter(|a| !a.is_empty())),
            None => (line, None),
        };
        match cmd {
            "w" | "write" => match self.buf.save(arg.map(Path::new)) {
                Ok(()) => {
                    self.checkpoint();
                    self.saved_node = self.undo.current_id();
                    self.buf.dirty = false;
                    self.status = Some(format!("{} bytes written", self.buf.len()));
                }
                Err(e) => self.status = Some(format!("bish: hex: {e}")),
            },
            "wq" | "x" => match self.buf.save(arg.map(Path::new)) {
                Ok(()) => return true,
                Err(e) => self.status = Some(format!("bish: hex: {e}")),
            },
            "q" if self.buf.is_dirty() => {
                self.status = Some("E37: No write since last change (add ! to override)".to_string());
            }
            "q" | "q!" => return true,
            "goto" | "g" => match arg.and_then(|a| parse_offset(a, self.buf.offset, self.buf.len())) {
                Some(off) => {
                    self.vk.push_jump(self.buf.cursor());
                    self.buf.set_offset(off);
                }
                None => self.status = Some("usage: :goto OFFSET (0x1f00, 4096, +16, -16, $)".to_string()),
            },
            "inspect" => {
                self.inspector = !self.inspector;
                self.status = Some(format!("inspector {}", if self.inspector { "on" } else { "off" }));
            }
            "set" => {
                let (opt, value) = match arg.map(|a| a.split_once(' ').unwrap_or((a, ""))) {
                    Some((o, v)) => (o, v.trim()),
                    None => ("", ""),
                };
                match opt {
                    "width" | "w" => match value {
                        "auto" | "" => {
                            self.width_override = None;
                            self.status = Some("width auto".to_string());
                        }
                        v => match v.parse::<usize>() {
                            Ok(n) if n > 0 => {
                                self.width_override = Some(n);
                                self.buf.set_bytes_per_row(n);
                                self.status = Some(format!("width {n}"));
                            }
                            _ => self.status = Some("usage: :set width N | auto".to_string()),
                        },
                    },
                    "endian" | "e" => match value {
                        "little" | "le" => self.endian = Endian::Little,
                        "big" | "be" => self.endian = Endian::Big,
                        _ => self.status = Some("usage: :set endian little | big".to_string()),
                    },
                    _ => self.status = Some(format!("bish: hex: unknown option '{opt}' (expected: width, endian)")),
                }
            }
            "help" | "h" | "?" => self.status = Some("(help shown -- press any key)".to_string()),
            // `:0x1f00` / `:4096` -- a bare offset. Deliberately *not*
            // vim's `:N` "go to line N": offsets are this editor's own
            // coordinate system, and a row number would be a far less
            // useful thing to be able to type.
            other => match parse_offset(other, self.buf.offset, self.buf.len()) {
                Some(off) => {
                    self.vk.push_jump(self.buf.cursor());
                    self.buf.set_offset(off);
                }
                None => self.status = Some(format!("bish: hex: not a command: {other}")),
            },
        }
        false
    }
}

// ---------------------------------------------------------------------
// The frame API repl.rs drives this through
// ---------------------------------------------------------------------

// How driving one keystroke ended. `Continue`/`Quit` are this view's own
// business; `Window` is handed straight back to repl.rs, which owns
// focus and the window layout -- exactly the split
// `run_normal_mode_navigation`'s own `KeyOutcome::Window` arm already
// establishes for the file editor, and (as there) the only way this view
// is ever left without closing it.
pub enum HexOutcome {
    Continue,
    Quit,
    Window(WindowCmd, Option<usize>),
    /// Ctrl-L on the colon line: repaint everything, this pane's
    /// neighbours and the tab bar included.
    ///
    /// Reported rather than done here because a full repaint needs the
    /// compositor, which this view has no reach into -- the same reason
    /// `Window` is reported. What it replaces is worse: `read_line`'s
    /// own unreported Ctrl-L prints `\x1b[H\x1b[2J` straight at the real
    /// terminal, which wipes the compositor's whole frame -- pane
    /// borders, tab bar, every neighbour -- with nothing repainting any
    /// of it. See repl.rs's own `ReadOutcome::CtrlL` arm, which is where
    /// that was already fixed for the shell prompt.
    Redraw,
}

impl HexSession {
    // Everything that has to happen before a frame can be drawn, in the
    // one order that keeps undo honest: reflow to the pane's current
    // width, *then* checkpoint (see `checkpoint`'s own doc comment on
    // why this runs after the edits rather than before them), then bring
    // the cursor back into view.
    fn prepare(&mut self, rect: Rect) -> Layout {
        let layout = self.layout(rect);
        self.buf.set_bytes_per_row(layout.bytes_per_row);
        self.checkpoint();
        self.scroll_to_cursor(layout.rows.min(rect.rows));
        layout
    }

    // The live render: this pane's own dump plus the terminal's shared
    // global status row. Clears `self.status` on the way out, so a
    // transient message (a yank count, a search result, an error) is
    // shown for exactly one frame and the next keystroke reveals the
    // ordinary status line again.
    pub fn render(&mut self, rect: Rect, term_rows: usize, term_cols: usize) -> String {
        let layout = self.prepare(rect);
        let mut out = render_global_status_row(&status_text(self, layout, term_cols), term_rows);
        out.push_str(&build_frame(self, layout, rect, rect.row, rect.col));
        self.status = None;
        out
    }

    // `fileeditor::freeze_editor_frame`'s own counterpart -- feeds this
    // frame's current content into the owning session's vt100 grid
    // (pane-relative, hence the `0`/`0` origin) so a compositor redraw of
    // a pane holding a *detached* `Frame::Hex` shows the real dump
    // instead of stale or blank rows. The global status row is
    // deliberately not part of it: that row lives outside every pane's
    // rect, so it isn't this grid's to hold.
    pub fn freeze(&mut self, screen: &Rc<RefCell<vt100::Screen>>, rect: Rect) {
        let layout = self.prepare(rect);
        let framed = build_frame(self, layout, rect, 0, 0);
        screen.borrow_mut().feed(framed.as_bytes());
    }

    // One keystroke. `on_idle` is the caller's own
    // `service_background_jobs` hook, threaded in for the two places this
    // can itself block on further input (the `:` colon line and the help
    // screen) so background jobs keep draining and a resize is still
    // noticed within one poll tick while either is up.
    pub fn handle_key(
        &mut self,
        key: Key,
        rect: Rect,
        term_rows: usize,
        term_cols: usize,
        cmd_history: &History,
        on_idle: &mut dyn FnMut(),
    ) -> HexOutcome {
        let layout = self.layout(rect);
        if matches!(self.mode, Mode::Insert | Mode::Replace) {
            self.handle_insert_key(key);
            return HexOutcome::Continue;
        }

        // `r` in the hex pane needs two digits, which the shared key
        // decoder can't express (`ReplaceChar` resolves after one), so
        // it's read here instead -- the one place this editor's key
        // handling steps outside `VimKeys`, and only for the pane where a
        // byte genuinely takes two keystrokes.
        if let Some(pending) = self.pending_replace {
            match key {
                Key::Escape => self.pending_replace = None,
                Key::Char(c) => match hex_digit(c) {
                    Some(nib) => match pending {
                        None => self.pending_replace = Some(Some(nib)),
                        Some(high) => {
                            self.pending_replace = None;
                            if !self.readonly_refused() {
                                let at = self.buf.offset;
                                self.buf.overwrite_at(at, &[(high << 4) | nib]);
                            }
                        }
                    },
                    None => {
                        self.pending_replace = None;
                        self.status = Some(format!("'{c}' is not a hex digit"));
                    }
                },
                _ => self.pending_replace = None,
            }
            return HexOutcome::Continue;
        }

        match key {
            Key::Tab if !self.vk.is_search_pending() => {
                self.pane = if self.pane == Pane::Hex { Pane::Ascii } else { Pane::Hex };
                return HexOutcome::Continue;
            }
            Key::Char('r') if self.pane == Pane::Hex && !self.vk.is_search_pending() && self.vk.pending_display().is_empty() => {
                self.pending_replace = Some(None);
                return HexOutcome::Continue;
            }
            Key::Char(':') if !self.vk.is_search_pending() && self.vk.pending_display().is_empty() => {
                return self.run_colon_line(rect, term_rows, term_cols, cmd_history, on_idle);
            }
            Key::Escape if self.mode == Mode::Visual && !self.vk.is_search_pending() => {
                let cursor = self.buf.cursor();
                self.vk.end_visual(cursor);
                self.mode = Mode::Normal;
                return HexOutcome::Continue;
            }
            _ => {}
        }

        if self.mode == Mode::Visual && self.vk.pending_display().is_empty() && !self.vk.is_search_pending() && self.handle_visual_key(key) {
            return HexOutcome::Continue;
        }

        let outcome = self.vk.feed(key);
        let window = self.handle_outcome(outcome, layout.rows.min(rect.rows));
        // A motion in Visual mode keeps the selection live; anything that
        // committed an operator already ended it inside `handle_outcome`.
        if self.mode == Mode::Visual && !self.vk.is_visual() {
            self.mode = Mode::Normal;
        }
        match window {
            Some((cmd, count)) => HexOutcome::Window(cmd, count),
            None => HexOutcome::Continue,
        }
    }

    // Reuses `editor::read_line` for the colon line itself, on the same
    // shared global command row `run_command_mode`'s own `:` prompt uses,
    // with no completion/suggestion providers -- this view's commands are
    // its own, not the shell's.
    fn run_colon_line(
        &mut self,
        rect: Rect,
        term_rows: usize,
        term_cols: usize,
        cmd_history: &History,
        on_idle: &mut dyn FnMut(),
    ) -> HexOutcome {
        print!("\x1b[{};1H\x1b[K\x1b[?25h", crate::repl::command_mode_row(term_rows) + 1);
        let _ = io::stdout().flush();
        let mut registers = std::mem::take(&mut self.registers);
        let outcome = editor::read_line(
            ":",
            cmd_history,
            true,
            // `ctrl_l_reports`: true, so Ctrl-L comes back here as an
            // outcome instead of being answered inside `read_line` with
            // a raw full-screen clear this pane could never put back.
            true,
            None,
            0,
            term_cols,
            crate::bishedit::highlight::HighlightContext::default(),
            None,
            None,
            false,
            None,
            &mut registers,
            &[],
            None,
            // This colon line is drawn over a hex frame the caller
            // repaints itself on the next key -- nothing here can put it
            // back, so it never asks read_line to redraw.
            &mut || {
                on_idle();
                false
            },
            // No shell here to ask, and a hex dump has nothing you would
            // sweep with the terminal's own selection anyway.
            true,
        );
        self.registers = registers;
        if matches!(outcome, Ok(editor::ReadOutcome::CtrlL)) {
            return HexOutcome::Redraw;
        }
        if let Ok(editor::ReadOutcome::Line(line)) = outcome {
            if matches!(line.trim(), "help" | "h" | "?") {
                self.show_help(rect, term_rows, term_cols, on_idle);
                return HexOutcome::Continue;
            }
            if self.dispatch_colon(&line) {
                return HexOutcome::Quit;
            }
        }
        HexOutcome::Continue
    }

    // One curated page, dismissed by any key -- the same "one screen,
    // not a whole help system" scope the file editor's own `:help`
    // already sets. Drawn into this pane rather than over the whole
    // terminal, so what's around it (a split sibling, the tab bar) stays
    // where it is; a pane too short for the whole page simply shows as
    // much as fits.
    fn show_help(&mut self, rect: Rect, term_rows: usize, term_cols: usize, on_idle: &mut dyn FnMut()) {
        let mut out = String::new();
        let mut help = HELP_TEXT.lines();
        for r in 0..rect.rows {
            out.push_str(&format!("\x1b[{};{}H{}", rect.row + r + 1, rect.col + 1, fit_pad(help.next().unwrap_or(""), rect.cols)));
        }
        out.push_str(&render_global_status_row(&fit_pad("-- press any key --", term_cols), term_rows));
        out.push_str("\x1b[?25l");
        print!("{out}");
        let _ = io::stdout().flush();
        let _ = editor::read_key_idle(on_idle);
    }
}

fn fit_pad(s: &str, width: usize) -> String {
    let s = fit(s, width);
    format!("{s}{}", " ".repeat(width.saturating_sub(display_len(&s))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(bytes: &[u8]) -> HexBuffer {
        HexBuffer::from_bytes(bytes.to_vec(), 16, 10)
    }

    // The pane every test below drives its session in: `run_hex_frame`
    // renders into a rect, not the whole terminal, so the tests use one
    // too rather than a size the real editor never passes.
    const RECT: Rect = Rect { row: 0, col: 0, rows: 22, cols: 100 };

    fn session_with(bytes: &[u8]) -> HexSession {
        let b = buf(bytes);
        let undo = UndoTree::new(b.bytes.clone(), (0, 0));
        HexSession {
            buf: b,
            vk: VimKeys::new(),
            // Not `Registers::new()`: that one's unnamed register is the
            // *real* OS clipboard, which is genuinely global -- a test
            // yanking into it races every other test (and anything else
            // on the machine) that touches it.
            registers: Registers::new_for_test(),
            undo,
            mode: Mode::Normal,
            pane: Pane::Hex,
            pending_nibble: None,
            pending_replace: None,
            width_override: Some(16),
            inspector: false,
            endian: Endian::Little,
            last_search: None,
            saved_node: 0,
            status: None,
        }
    }

    // Mirrors `run_hex_frame`'s own loop exactly: checkpoint (which is
    // what commits the previous key's edit to the undo tree -- it runs
    // inside `render`/`prepare` there), then handle the next key.
    // Getting this order wrong here would make undo look broken in tests
    // while working in the real editor, or vice versa.
    fn feed(session: &mut HexSession, keys: &str) {
        for c in keys.chars() {
            let key = match c {
                '\n' => Key::Enter,
                '\x1b' => Key::Escape,
                '\t' => Key::Tab,
                other => Key::Char(other),
            };
            session.checkpoint();
            session.handle_key(key, RECT, 24, RECT.cols, &History::load("/dev/null"), &mut || {});
        }
    }

    // --- the Buffer impl -------------------------------------------

    #[test]
    fn rows_are_lines_and_bytes_are_chars() {
        let b = buf(&[0x41, 0x42, 0x43]);
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line_len(0), 3);
        assert_eq!(b.char_at(0, 0), Some('A'));
        assert_eq!(b.char_at(0, 2), Some('C'));
        assert_eq!(b.char_at(0, 3), None);

        let wide = HexBuffer::from_bytes((0u8..40).collect(), 16, 10);
        assert_eq!(wide.line_count(), 3);
        assert_eq!(wide.line_len(0), 16);
        assert_eq!(wide.line_len(2), 8);
        assert_eq!(wide.char_at(2, 0), Some(32u8 as char));
    }

    #[test]
    fn an_empty_buffer_still_has_one_row_so_motions_cannot_panic() {
        let mut b = buf(&[]);
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line_len(0), 0);
        motion::apply_motion(&mut b, Motion::Right, None);
        motion::apply_motion(&mut b, Motion::GotoLastLine, None);
        assert_eq!(b.offset(), 0);
    }

    // The whole point of the Buffer impl: the shared motion engine drives
    // a byte buffer with no hex-specific code at all.
    #[test]
    fn shared_motions_navigate_bytes_without_any_hex_specific_code() {
        let mut b = HexBuffer::from_bytes((0u8..48).collect(), 16, 10);
        motion::apply_motion(&mut b, Motion::Right, Some(5));
        assert_eq!(b.offset(), 5);
        motion::apply_motion(&mut b, Motion::Down, None);
        assert_eq!(b.offset(), 21);
        motion::apply_motion(&mut b, Motion::LineStart, None);
        assert_eq!(b.offset(), 16);
        motion::apply_motion(&mut b, Motion::LineEnd, None);
        assert_eq!(b.offset(), 31);
        motion::apply_motion(&mut b, Motion::GotoLastLine, None);
        assert_eq!(b.cursor().0, 2);
        motion::apply_motion(&mut b, Motion::GotoFirstLine, None);
        assert_eq!(b.offset(), 0);
    }

    // Word motions over Latin-1 bytes are what makes hunting embedded
    // strings in a binary work with the ordinary `w`/`b` keys.
    #[test]
    fn word_motions_step_between_printable_runs_in_binary() {
        let mut data = vec![0x00, 0x00, 0x00];
        data.extend_from_slice(b"hello");
        data.extend_from_slice(&[0x00, 0x00]);
        data.extend_from_slice(b"world");
        let mut b = HexBuffer::from_bytes(data, 16, 10);
        motion::apply_motion(&mut b, Motion::WordForward, None);
        assert_eq!(b.offset(), 3, "w should land on the start of \"hello\"");
        motion::apply_motion(&mut b, Motion::WordForward, None);
        assert_eq!(b.offset(), 8, "next w onto the NUL run between the strings");
        motion::apply_motion(&mut b, Motion::WordForward, None);
        assert_eq!(b.offset(), 10, "and then onto \"world\"");
    }

    #[test]
    fn set_cursor_never_lands_past_the_last_byte() {
        let mut b = buf(&[1, 2, 3]);
        b.set_cursor(0, 99);
        assert_eq!(b.offset(), 2);
        b.set_cursor(5, 0);
        assert_eq!(b.offset(), 2);
    }

    // --- byte ranges from shared motion ranges ---------------------

    #[test]
    fn motion_ranges_convert_to_flat_byte_spans() {
        let mut b = HexBuffer::from_bytes((0u8..48).collect(), 16, 10);
        b.set_offset(4);
        let range = motion::motion_range(&mut b, Motion::Right, Some(3)).unwrap();
        assert_eq!(b.byte_range(&range), (4, 7), "exclusive: 3l covers 3 bytes");

        b.set_offset(4);
        let range = motion::motion_range(&mut b, Motion::Down, None).unwrap();
        assert_eq!(b.byte_range(&range), (0, 32), "linewise: two whole rows");

        b.set_offset(40);
        let range = motion::motion_range(&mut b, Motion::GotoLastLine, None).unwrap();
        assert_eq!(b.byte_range(&range), (32, 48), "linewise clamps to the real end");
    }

    // --- register interchange --------------------------------------

    #[test]
    fn every_byte_round_trips_through_a_register() {
        let all: Vec<u8> = (0..=255).collect();
        let text = bytes_to_register_text(&all);
        assert_eq!(register_text_to_bytes(&text), all);
    }

    #[test]
    fn text_yanked_elsewhere_becomes_its_utf8_bytes() {
        // Below U+0100 is the byte itself (our own round trip); above it,
        // the character's real UTF-8 encoding.
        assert_eq!(register_text_to_bytes("A"), vec![0x41]);
        assert_eq!(register_text_to_bytes("\u{00e9}"), vec![0xE9]);
        assert_eq!(register_text_to_bytes("\u{2192}"), vec![0xE2, 0x86, 0x92]);
    }

    // --- search ----------------------------------------------------

    #[test]
    fn patterns_distinguish_hex_bytes_from_literal_text() {
        assert_eq!(parse_pattern("dead"), Some(Pattern::Bytes(vec![0xDE, 0xAD])));
        assert_eq!(parse_pattern("de ad be ef"), Some(Pattern::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])));
        assert_eq!(parse_pattern("\"dead\""), Some(Pattern::Text(b"dead".to_vec())));
        assert_eq!(parse_pattern("hello"), Some(Pattern::Text(b"hello".to_vec())));
        // Odd digit count isn't a whole number of bytes, so it's text.
        assert_eq!(parse_pattern("dea"), Some(Pattern::Text(b"dea".to_vec())));
        assert_eq!(parse_pattern("   "), None);
    }

    #[test]
    fn search_finds_a_pattern_straddling_a_row_boundary() {
        // The reason search can't go through the shared per-line matcher:
        // at 16 bytes a row, most patterns cross a boundary.
        let mut data = vec![0u8; 32];
        data[14] = 0xDE;
        data[15] = 0xAD;
        data[16] = 0xBE;
        data[17] = 0xEF;
        assert_eq!(find_bytes(&data, &[0xDE, 0xAD, 0xBE, 0xEF], 0, true), Some(14));
    }

    #[test]
    fn search_advances_past_the_current_match_and_wraps() {
        let data = b"abcXabcXabc".to_vec();
        assert_eq!(find_bytes(&data, b"X", 0, true), Some(3));
        assert_eq!(find_bytes(&data, b"X", 3, true), Some(7));
        assert_eq!(find_bytes(&data, b"X", 7, true), Some(3), "wraps around");
        assert_eq!(find_bytes(&data, b"X", 7, false), Some(3));
        assert_eq!(find_bytes(&data, b"X", 0, false), Some(7), "wraps backward");
        assert_eq!(find_bytes(&data, b"zz", 0, true), None);
    }

    // --- layout ----------------------------------------------------

    #[test]
    fn layout_matches_the_familiar_xxd_geometry() {
        let l = compute_layout(24, 100, 0x1000, Some(16), false);
        assert_eq!(l.bytes_per_row, 16);
        assert_eq!(l.offset_width, 8);
        assert_eq!(l.hex_col, 10);
        // 8 offset + 2 gap + 49 hex + 1 gap + 1 '|' ... ascii starts here.
        assert_eq!(l.ascii_col, 60);
        assert_eq!(l.dump_width, 77);
        assert_eq!(hex_cell_col(0), 0);
        assert_eq!(hex_cell_col(8), 25, "the group gap shifts the second half by one");
        assert_eq!(hex_cell_col(15), 46);
    }

    #[test]
    fn auto_width_grows_with_the_terminal_but_never_overflows_it() {
        for cols in [60usize, 80, 100, 140, 200] {
            let l = compute_layout(24, cols, 0x100, None, false);
            assert!(l.bytes_per_row.is_multiple_of(GROUP), "{l:?}");
            if cols >= 77 {
                assert!(l.dump_width <= cols, "dump {} wider than terminal {cols}", l.dump_width);
            }
        }
        assert!(compute_layout(24, 200, 0x100, None, false).bytes_per_row > compute_layout(24, 80, 0x100, None, false).bytes_per_row);
    }

    #[test]
    fn the_offset_column_widens_for_a_large_file() {
        assert_eq!(offset_width_for(0), 8);
        assert_eq!(offset_width_for(0xFFFF_FFFF), 8);
        assert_eq!(offset_width_for(0x1_0000_0000), 10);
    }

    // The bug the pty run found: purely greedy auto-width filled a
    // 110-column terminal with 24 bytes a row, leaving exactly zero
    // columns for the inspector.
    #[test]
    fn auto_width_leaves_room_for_the_inspector_instead_of_squeezing_it_out() {
        let l = compute_layout(24, 110, 0x100, None, true);
        assert!(l.inspector_col.is_some(), "{l:?}");
        assert!(l.dump_width + INSPECTOR_WIDTH <= 110, "{l:?}");
        // Without the inspector, the same terminal goes back to filling
        // every column it can.
        assert!(compute_layout(24, 110, 0x100, None, false).bytes_per_row > l.bytes_per_row);
        // A terminal too narrow for both still gets a usable dump.
        let narrow = compute_layout(24, 60, 0x100, None, true);
        assert!(narrow.inspector_col.is_none());
        assert_eq!(narrow.bytes_per_row, GROUP);
    }

    #[test]
    fn floats_stay_readable_whatever_the_bytes_decode_to() {
        assert_eq!(format_float(0.0), "0");
        assert_eq!(format_float(1.5), "1.500000");
        // The real case from the pty run: 57 digits before this fix.
        assert!(format_float(f64::from_bits(0x6862_7369_6f2c_2077)).len() < 20);
        assert!(format_float(f32::from_bits(0x0000_0068) as f64).len() < 20);
        assert_eq!(format_float(f64::NAN), "NaN");
    }

    #[test]
    fn the_inspector_only_appears_when_there_is_room_for_it() {
        assert!(compute_layout(24, 80, 0x100, Some(16), true).inspector_col.is_none());
        assert!(compute_layout(24, 120, 0x100, Some(16), true).inspector_col.is_some());
        assert!(compute_layout(24, 120, 0x100, Some(16), false).inspector_col.is_none());
    }

    // --- editing ---------------------------------------------------

    #[test]
    fn x_deletes_a_byte_and_writes_it_to_the_unnamed_register() {
        let mut s = session_with(b"ABCD");
        feed(&mut s, "x");
        assert_eq!(s.buf.bytes(), b"BCD");
        assert_eq!(register_text_to_bytes(&s.registers.read(None).text), b"A");
    }

    #[test]
    fn a_count_and_an_operator_compose_exactly_as_in_the_file_editor() {
        let mut s = session_with(b"ABCDEFGH");
        feed(&mut s, "3x");
        assert_eq!(s.buf.bytes(), b"DEFGH");
        let mut s = session_with(b"ABCDEFGH");
        feed(&mut s, "d3l");
        assert_eq!(s.buf.bytes(), b"DEFGH");
    }

    #[test]
    fn yank_and_put_move_real_bytes_through_a_named_register() {
        let mut s = session_with(b"\x00\x01\x02\x03");
        feed(&mut s, "\"ay2l");
        feed(&mut s, "$");
        feed(&mut s, "\"ap");
        assert_eq!(s.buf.bytes(), b"\x00\x01\x02\x03\x00\x01");
    }

    #[test]
    fn undo_and_redo_restore_the_byte_vector() {
        let mut s = session_with(b"ABCD");
        feed(&mut s, "xx");
        assert_eq!(s.buf.bytes(), b"CD");
        feed(&mut s, "u");
        assert_eq!(s.buf.bytes(), b"BCD");
        feed(&mut s, "u");
        assert_eq!(s.buf.bytes(), b"ABCD");
        s.checkpoint();
        s.handle_key(Key::CtrlR, RECT, 24, RECT.cols, &History::load("/dev/null"), &mut || {});
        assert_eq!(s.buf.bytes(), b"BCD");
    }

    #[test]
    fn insert_mode_in_the_hex_pane_takes_two_digits_per_byte() {
        let mut s = session_with(b"\xff");
        feed(&mut s, "i");
        assert_eq!(s.mode, Mode::Insert);
        feed(&mut s, "4");
        assert_eq!(s.pending_nibble, Some(4), "a half-typed byte is real, visible state");
        assert_eq!(s.buf.bytes(), b"\xff", "nothing is written until the second digit");
        feed(&mut s, "1");
        assert_eq!(s.buf.bytes(), b"\x41\xff");
        assert_eq!(s.pending_nibble, None);
        feed(&mut s, "\x1b");
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn insert_mode_in_the_ascii_pane_writes_characters_directly() {
        let mut s = session_with(b"\x00");
        s.pane = Pane::Ascii;
        feed(&mut s, "iHi\x1b");
        assert_eq!(s.buf.bytes(), b"Hi\x00");
    }

    #[test]
    fn replace_mode_overwrites_instead_of_shifting_everything_after_it() {
        let mut s = session_with(b"ABCD");
        feed(&mut s, "R4141");
        assert_eq!(s.buf.bytes(), b"AACD", "same length -- offsets preserved");
        assert_eq!(s.buf.len(), 4);
    }

    #[test]
    fn r_in_the_hex_pane_replaces_one_byte_from_two_digits() {
        let mut s = session_with(b"ABCD");
        feed(&mut s, "r");
        assert_eq!(s.pending_replace, Some(None));
        feed(&mut s, "7f");
        assert_eq!(s.buf.bytes(), b"\x7fBCD");
        assert_eq!(s.pending_replace, None);
    }

    #[test]
    fn tab_switches_which_pane_a_keystroke_lands_in() {
        let mut s = session_with(b"AB");
        assert_eq!(s.pane, Pane::Hex);
        feed(&mut s, "\t");
        assert_eq!(s.pane, Pane::Ascii);
        feed(&mut s, "\t");
        assert_eq!(s.pane, Pane::Hex);
    }

    #[test]
    fn ctrl_a_and_ctrl_x_increment_the_byte_under_the_cursor() {
        let mut s = session_with(&[0xFE]);
        let history = History::load("/dev/null");
        s.handle_key(Key::CtrlA, RECT, 24, RECT.cols, &history, &mut || {});
        assert_eq!(s.buf.bytes(), &[0xFF]);
        s.handle_key(Key::CtrlA, RECT, 24, RECT.cols, &history, &mut || {});
        assert_eq!(s.buf.bytes(), &[0x00], "wraps rather than saturating");
        s.handle_key(Key::CtrlX, RECT, 24, RECT.cols, &history, &mut || {});
        assert_eq!(s.buf.bytes(), &[0xFF]);
    }

    #[test]
    fn visual_mode_selects_a_byte_span_and_yanks_it() {
        let mut s = session_with(b"ABCDEF");
        feed(&mut s, "vll");
        assert_eq!(s.visual_range(), Some((0, 3)));
        feed(&mut s, "y");
        assert_eq!(register_text_to_bytes(&s.registers.read(None).text), b"ABC");
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn visual_line_mode_selects_whole_rows() {
        let mut s = session_with(&(0u8..48).collect::<Vec<u8>>());
        feed(&mut s, "jV");
        assert_eq!(s.visual_range(), Some((16, 32)));
        feed(&mut s, "d");
        assert_eq!(s.buf.len(), 32);
        assert_eq!(s.buf.bytes()[16], 32, "the third row moved up into the second");
    }

    #[test]
    fn case_operators_work_on_the_ascii_range_inside_a_binary() {
        let mut s = session_with(b"\x00hello\x00");
        feed(&mut s, "lgU3l");
        assert_eq!(s.buf.bytes(), b"\x00HELlo\x00");
    }

    // --- colon commands --------------------------------------------

    #[test]
    fn goto_accepts_hex_decimal_and_relative_offsets() {
        assert_eq!(parse_offset("0x10", 0, 0x100), Some(16));
        assert_eq!(parse_offset("4096", 0, 0x2000), Some(4096));
        assert_eq!(parse_offset("+16", 32, 0x100), Some(48));
        assert_eq!(parse_offset("-16", 32, 0x100), Some(16));
        assert_eq!(parse_offset("$", 0, 0x100), Some(0xFF));
        assert_eq!(parse_offset("-100", 0, 0x100), Some(0), "clamped, not wrapped");
        assert_eq!(parse_offset("nonsense", 0, 0x100), None);
    }

    #[test]
    fn a_bare_offset_is_a_colon_command_of_its_own() {
        let mut s = session_with(&(0u8..64).collect::<Vec<u8>>());
        s.dispatch_colon("0x20");
        assert_eq!(s.buf.offset(), 32);
        s.dispatch_colon("goto +8");
        assert_eq!(s.buf.offset(), 40);
    }

    #[test]
    fn set_width_reflows_without_moving_the_cursor_off_its_byte() {
        let mut s = session_with(&(0u8..64).collect::<Vec<u8>>());
        s.dispatch_colon("goto 0x21");
        assert_eq!(s.buf.offset(), 33);
        assert_eq!(s.buf.cursor(), (2, 1));
        s.dispatch_colon("set width 8");
        assert_eq!(s.buf.offset(), 33, "still the same byte");
        assert_eq!(s.buf.cursor(), (4, 1), "just at a different row/col");
    }

    #[test]
    fn a_path_that_does_not_exist_yet_is_marked_new_until_it_is_written() {
        let dir = std::env::temp_dir().join(format!("bish-hex-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fresh.bin");

        let mut b = HexBuffer::open(Some(&path), 16, 10).unwrap();
        assert!(b.is_new, "a typo'd path must not look like an ordinary empty file");
        assert_eq!(b.len(), 0);
        b.insert_at(0, b"hi");
        b.save(None).unwrap();
        assert!(!b.is_new);
        assert_eq!(std::fs::read(&path).unwrap(), b"hi");

        // ...and an existing file is never marked new.
        assert!(!HexBuffer::open(Some(&path), 16, 10).unwrap().is_new);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quitting_a_modified_buffer_needs_the_bang() {
        let mut s = session_with(b"AB");
        feed(&mut s, "x");
        assert!(!s.dispatch_colon("q"), "a dirty buffer's own `:q` must not close the frame");
        assert!(s.status.as_deref().unwrap_or_default().contains("E37"));
        assert!(s.dispatch_colon("q!"));
    }

    #[test]
    fn readonly_refuses_every_mutation_but_allows_yanking() {
        let mut s = session_with(b"ABCD");
        s.buf.readonly = true;
        feed(&mut s, "x");
        assert_eq!(s.buf.bytes(), b"ABCD");
        feed(&mut s, "y2l");
        assert_eq!(register_text_to_bytes(&s.registers.read(None).text), b"AB");
    }

    // --- rendering -------------------------------------------------

    // Strips SGR and cursor-positioning escapes so a rendered frame can be
    // checked for what it actually shows.
    fn plain(frame: &str) -> String {
        let mut out = String::new();
        let mut chars = frame.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        if c2 == 'H' {
                            out.push('\n');
                        }
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn a_rendered_row_looks_like_hexdump_c() {
        let mut s = session_with(b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00");
        let frame = plain(&s.render(RECT, 24, RECT.cols));
        let row = frame.lines().find(|l| l.trim_start().starts_with("00000000")).expect("offset gutter");
        assert!(row.contains("7f 45 4c 46 02 01 01 00  00 00 00 00 00 00 00 00"), "{row:?}");
        assert!(row.contains("|.ELF............|"), "{row:?}");
    }

    #[test]
    fn the_status_line_reports_the_byte_under_the_cursor_every_way() {
        let mut s = session_with(&[0x41]);
        let layout = s.layout(RECT);
        let text = plain(&status_text(&s, layout, RECT.cols));
        assert!(text.contains("41 'A'"), "{text:?}");
        assert!(text.contains("65"), "{text:?}");
        assert!(text.contains("0101"), "{text:?}");
        assert!(text.contains("0b01000001"), "{text:?}");
        assert!(text.contains("-- NORMAL --"), "{text:?}");
        assert!(text.contains("[hex]"), "{text:?}");
        s.pane = Pane::Ascii;
        assert!(plain(&status_text(&s, layout, RECT.cols)).contains("[ascii]"));
    }

    #[test]
    fn the_help_page_fits_the_pane_an_ordinary_terminal_gives() {
        // `show_help` shows as much of the page as the pane is tall and
        // silently drops the rest -- which, on the 24-row terminal that
        // is still the common default, used to lose the last four lines
        // (found via pty against the real binary). This is the guard
        // against it happening again the next time a line is added.
        let pane_rows = crate::repl::content_rows(24);
        let lines: Vec<&str> = HELP_TEXT.lines().collect();
        assert!(lines.len() <= pane_rows, "the help page is {} lines but a 24-row terminal's pane is {pane_rows}", lines.len());
        // 80 columns, minus nothing: `fit` truncates rather than wraps,
        // so anything wider is silently cut off mid-word.
        for line in lines {
            assert!(line.chars().count() <= 80, "help line {line:?} is wider than 80 columns");
        }
    }

    #[test]
    fn every_row_is_painted_out_to_the_pane_width() {
        // The pane version has no full-screen clear to lean on, so each
        // row must blank the rest of its own width itself -- including
        // the rows past the end of a short file, which would otherwise
        // keep showing whatever the previous frame left there.
        let mut s = session_with(b"AB");
        let rect = Rect { row: 0, col: 0, rows: 6, cols: 90 };
        let frame = s.render(rect, 24, rect.cols);
        // Row 0 is the status line (rendered first); the rest are the
        // pane's own, and every one of them has to reach `cols`.
        let flat = plain(&frame);
        let rows: Vec<&str> = flat.lines().skip(2).collect();
        assert_eq!(rows.len(), rect.rows, "one line per pane row, {rows:?}");
        for row in rows {
            assert_eq!(row.chars().count(), rect.cols, "row {row:?} does not paint its whole width");
        }
    }

    #[test]
    fn the_frame_is_addressed_against_its_pane_not_the_terminal() {
        let mut s = session_with(b"AB");
        let rect = Rect { row: 7, col: 30, rows: 4, cols: 60 };
        let layout = s.prepare(rect);
        // The live render addresses this pane's real position...
        let live = build_frame(&s, layout, rect, rect.row, rect.col);
        assert!(live.contains("\x1b[8;31H"), "first row should land at the pane's own top-left");
        // ...while `freeze` addresses the session's own grid, which is
        // pane-relative -- feeding absolute positions into it would put
        // the content at completely the wrong cells in a split window.
        let frozen = build_frame(&s, layout, rect, 0, 0);
        assert!(frozen.contains("\x1b[1;1H"), "frozen frame should start at the grid's own origin");
    }

    #[test]
    fn no_rendered_row_is_wider_than_its_pane() {
        let mut s = session_with(&(0u8..=255).collect::<Vec<u8>>());
        for cols in [80usize, 100, 140] {
            let rect = Rect { row: 0, col: 0, rows: 22, cols };
            s.inspector = true;
            // Skips the status row, which is the *terminal's* full width
            // by design, not this pane's -- see status_text.
            let frame = s.render(rect, 24, cols);
            let dump = frame.split_once("\x1b[2m").map(|(_, rest)| rest.to_string()).unwrap_or(frame);
            for line in plain(&dump).lines() {
                assert!(line.chars().count() <= cols, "row {:?} is {} wide, the pane is {cols}", line, line.chars().count());
            }
        }
    }
}
