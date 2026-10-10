// `e --hex FILE`: a hex viewer and editor on bishedit's core. This
// module owns the buffer, layout, keys and rendering, all testable
// without a terminal; repl.rs owns the frame (`Frame::Hex`), its rect and
// the keystroke loop (`run_hex_frame`), so a hex view stacks and resumes
// like any editor frame.
//
// A hex editor is a buffer with unusual rendering: `HexBuffer`
// implements `bishedit::Buffer`, rows of `bytes_per_row` bytes as lines
// and each byte a Latin-1 char, which brings all of `bishedit::motion`
// (where `w`/`b`/`e` jump between printable runs, handy for strings in a
// binary). Keys go through `VimKeys`, so counts, operators, registers,
// Visual mode and `.` work as in the file editor; undo is
// `UndoTree<Vec<u8>>`; yank and put use the shared `Registers`.
//
// Where bytes differ from text:
//
//  1. Extraction is in byte space, never `motion::extract_text`, which
//     inserts `\n` at row boundaries and would corrupt data where 0x0A
//     is an ordinary byte. `Buffer::line_wraps` stays false, so `$`, `0`
//     and `yy` mean this row; `byte_range` turns a `MotionRange` into a
//     flat span.
//
//  2. Every register written is `RegisterShape::Char`, even from `dd` or
//     `V`: a byte stream has no lines, and a linewise `p` would invent a
//     row boundary.
//
//  3. Search is over the flat byte vector, since a pattern can straddle
//     rows. See `parse_pattern` for how `/` chooses hex or text.
//
// The file is read wholly into memory; multi-gigabyte files are another
// program.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::bishedit::Buffer;
use crate::bishedit::motion::{self, CaseKind, Motion, MotionRange, MotionShape};
use crate::bishedit::registers::{RegisterShape, RegisterValue, Registers};
use crate::bishedit::undo::UndoTree;
use crate::bishedit::vimkeys::{InsertCmd, KeyOutcome, Op, VimKeys, WindowCmd};
use crate::editor::{self, Key};
use crate::history::History;
use crate::repl::render_global_status_row;
use crate::vt100;
use crate::window::Rect;

// Bytes per group, an extra space between groups, as `hexdump -C` and
// `xxd` do.
const GROUP: usize = 8;
// Columns the data inspector needs to be worth showing.
const INSPECTOR_WIDTH: usize = 26;

pub struct HexBuffer {
    bytes: Vec<u8>,
    path: Option<PathBuf>,
    // An offset rather than (row, col), since `:set width` can change the
    // rows; the cursor stays on the same byte across a reflow.
    offset: usize,
    bytes_per_row: usize,
    vtop: usize,
    vheight: usize,
    marks: HashMap<char, (usize, usize)>,
    dirty: bool,
    readonly: bool,
    // The named file doesn't exist yet. Opening one is fine (building a
    // binary from scratch), but a typo'd path shouldn't look like an empty
    // file, so the status line says `[New]` until `:w` creates it, as vim
    // does.
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
        HexBuffer {
            bytes,
            path: None,
            offset: 0,
            bytes_per_row,
            vtop: 0,
            vheight,
            marks: HashMap::new(),
            dirty: false,
            readonly: false,
            is_new: false,
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    // Clamped to the last byte: one past the end is only for Insert mode
    // (`set_offset_insert`), as Normal mode in vim never sits past the end.
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

    // Rows holding at least one byte, not counting the append position at
    // EOF: motions stay in real data, and the renderer draws that caret
    // itself.
    pub fn row_count(&self) -> usize {
        self.bytes.len().div_ceil(self.bytes_per_row).max(1)
    }

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

    // Overwrites in place, growing the file only past its end: `R` and the
    // hex pane's `r` keep the offsets after the cursor, which is the point
    // of overwrite in a binary.
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
        // As in `TextBuffer::save`: read-only protects this file, not the
        // bytes from being written elsewhere.
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

    // A `MotionRange` as the flat `[start, end)` byte span it covers (see
    // the module doc, point 1).
    pub fn byte_range(&self, range: &MotionRange) -> (usize, usize) {
        let bpr = self.bytes_per_row;
        let off = |(r, c): (usize, usize)| r * bpr + c;
        let len = self.bytes.len();
        match range.shape {
            MotionShape::Linewise => ((range.from.0 * bpr).min(len), ((range.to.0 + 1) * bpr).min(len)),
            // Nothing emits a blockwise selection here, and a rectangle
            // over the dump is the inclusive span anyway.
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
        if start >= self.bytes.len() { 0 } else { (self.bytes.len() - start).min(self.bytes_per_row) }
    }

    // Latin-1: every byte a distinct char with an exact round trip, so the
    // shared motions work on real byte values.
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

// Bytes as a register's `String`, one Latin-1 char per byte: exact,
// and a real register the file editor can put.
pub fn bytes_to_register_text(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

// The reverse. A char up to U+00FF is that byte; anything above, only
// from a yank in the file editor, contributes its UTF-8 encoding.
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

// `/` is ambiguous (`dead` is a word and two bytes), so the rule is
// explicit and the status line reports which reading was used:
//
//   /"dead"   literal text (quotes force it)
//   /dead     bytes DE AD (an even run of hex digits)
//   /hello    literal text (not hex)
//   /de ad be literal text (odd digit count)
//
// Spaces may separate hex digits (`/de ad be ef`).
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
        let bytes = compact.as_bytes().chunks(2).map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()).collect();
        return Some(Pattern::Bytes(bytes));
    }
    Some(Pattern::Text(trimmed.as_bytes().to_vec()))
}

// Substring search over the bytes, starting strictly after or before
// `from` so `n` moves to the next match. Wraps once, like vim's
// `wrapscan`.
pub fn find_bytes(haystack: &[u8], needle: &[u8], from: usize, forward: bool) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let last_start = haystack.len() - needle.len();
    if forward {
        let begin = from.saturating_add(1).min(last_start + 1);
        (begin..=last_start)
            .find(|&i| &haystack[i..i + needle.len()] == needle)
            .or_else(|| (0..begin.min(last_start + 1)).find(|&i| &haystack[i..i + needle.len()] == needle))
    } else {
        let begin = from.min(last_start + 1);
        (0..begin)
            .rev()
            .find(|&i| &haystack[i..i + needle.len()] == needle)
            .or_else(|| (begin..=last_start).rev().find(|&i| &haystack[i..i + needle.len()] == needle))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub bytes_per_row: usize,
    pub offset_width: usize,
    // Screen columns, relative to the dump, where the hex and ASCII
    // columns start.
    pub hex_col: usize,
    pub ascii_col: usize,
    pub dump_width: usize,
    pub rows: usize,
    // `Some(col)` when there's room for the data inspector.
    pub inspector_col: Option<usize>,
}

// Hex digits the largest offset needs, rounded to even and at least 8,
// the familiar `xxd` gutter.
pub fn offset_width_for(len: usize) -> usize {
    let mut digits: usize = 1;
    let mut v = len;
    while v >= 16 {
        v /= 16;
        digits += 1;
    }
    digits.next_multiple_of(2).max(8)
}

// The screen column a byte's two hex digits start at, within a row.
pub fn hex_cell_col(index: usize) -> usize {
    index * 3 + index / GROUP
}

// The hex column's width: the last cell's start, two digits and a
// space. Derived from `hex_cell_col` so they can't disagree.
pub fn hex_span(bytes_per_row: usize) -> usize {
    if bytes_per_row == 0 {
        return 0;
    }
    hex_cell_col(bytes_per_row - 1) + 3
}

fn dump_width_for(bytes_per_row: usize, offset_width: usize) -> usize {
    offset_width + 2 + hex_span(bytes_per_row) + 1 + bytes_per_row + 1
}

// `bytes_per_row: None` is auto: the widest multiple of `GROUP` that
// fits, so a wide pane gets 24 or 32 bytes a row. `rows`/`cols` are the
// pane; the status line is outside it
// (`repl::render_global_status_row`).
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
        // Not purely greedy: when the inspector is wanted, the row is sized
        // for the width left after it, so a 110-column window doesn't trade
        // the inspector for 8 more bytes. Fills the pane only if not even
        // one group fits beside it.
        None if want_inspector && dump_width_for(GROUP, offset_width) + INSPECTOR_WIDTH <= cols => widest_fitting(cols - INSPECTOR_WIDTH),
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

// What repl.rs's `hex_frames` holds, as `EditSession` is for
// `edit_frames`: the whole view, so a half-typed nibble, a selection and
// the undo history survive a detach.
pub struct HexSession {
    buf: HexBuffer,
    vk: VimKeys,
    // The shell's shared register table, moved in and out at focus changes
    // (`attach_registers`/`detach_registers`) rather than borrowed: only the
    // focused frame is driven, so it's equivalent, and methods need no
    // `&mut Registers`.
    registers: Registers,
    undo: UndoTree<Vec<u8>>,
    mode: Mode,
    pane: Pane,
    // The high nibble typed so far in the hex pane, shown in the status
    // line.
    pending_nibble: Option<u8>,
    // `r` in the hex pane takes two digits, so it can't use
    // `KeyOutcome::ReplaceChar`; see `handle_normal_key`.
    pending_replace: Option<Option<u8>>,
    width_override: Option<usize>,
    inspector: bool,
    endian: Endian,
    last_search: Option<(Vec<u8>, bool)>,
    // The undo node matching what's on disk, as `TextBuffer::saved_node`,
    // so undoing back to it clears `[+]`.
    saved_node: usize,
    status: Option<String>,
}

impl HexSession {
    // Bytes that aren't simply what's at `path`: an archive member or a
    // gzip'd file (see repl.rs's `open_one_edit_target`). Always read-only,
    // since nothing here can recompress them.
    pub fn from_bytes(bytes: Vec<u8>, path: &Path, rect: Rect) -> io::Result<HexSession> {
        if !cfg!(feature = "hexedit") {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "hex editing is not part of this build"));
        }
        let layout = compute_layout(rect.rows, rect.cols, 0, None, true);
        let mut buf = HexBuffer::from_bytes(bytes, layout.bytes_per_row, layout.rows);
        buf.path = Some(path.to_path_buf());
        buf.readonly = true;
        Ok(HexSession::from_buffer(buf))
    }

    // `rect` only seeds the geometry; each frame recomputes it (`prepare`).
    pub fn open(path: Option<&Path>, rect: Rect, readonly: bool) -> io::Result<HexSession> {
        if !cfg!(feature = "hexedit") {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "hex editing is not part of this build"));
        }
        let layout = compute_layout(rect.rows, rect.cols, 0, None, true);
        let mut buf = HexBuffer::open(path, layout.bytes_per_row, layout.rows)?;
        buf.readonly = readonly;
        Ok(HexSession::from_buffer(buf))
    }

    // The shared tail of both constructors.
    fn from_buffer(buf: HexBuffer) -> HexSession {
        let undo = UndoTree::new(buf.bytes.clone(), (0, 0));
        HexSession {
            buf,
            vk: VimKeys::new(),
            // A placeholder until `attach_registers` brings the shared
            // table; never read before then.
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

    // See the `registers` field.
    pub fn attach_registers(&mut self, registers: &mut Registers) {
        self.registers = std::mem::take(registers);
    }

    pub fn detach_registers(&mut self, registers: &mut Registers) {
        *registers = std::mem::take(&mut self.registers);
    }

    fn layout(&self, rect: Rect) -> Layout {
        compute_layout(rect.rows, rect.cols, self.buf.len(), self.width_override, self.inspector)
    }

    // Once per frame before rendering, not per edit, as the file editor's
    // `checkpoint_undo`: the current node is what's on screen, and an
    // Insert session is one undo step. Identical content is de-duplicated.
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

    // The byte span Visual mode covers, or `None` outside it.
    fn visual_range(&self) -> Option<(usize, usize)> {
        let (shape, anchor) = self.vk.visual_anchor()?;
        let bpr = self.buf.bytes_per_row;
        let anchor_off = anchor.0 * bpr + anchor.1;
        let cursor_off = self.buf.offset();
        let (lo, hi) = if anchor_off <= cursor_off { (anchor_off, cursor_off) } else { (cursor_off, anchor_off) };
        Some(match shape {
            // Blockwise doesn't arise (`Ctrl-V` isn't bound), and would be
            // the same flat span.
            RegisterShape::Char | RegisterShape::Block => (lo, (hi + 1).min(self.buf.len())),
            // `V` selects whole rows, still as a flat span (module doc,
            // point 2).
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
            // Case operators work on bytes for the same reason word motions
            // do: ASCII inside a binary is real text. Bytes without case,
            // or whose transform wouldn't fit a byte, are left alone.
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
            Op::Fold => {
                self.status = Some("folds have no meaning in a byte buffer".to_string());
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

fn printable(b: u8) -> Option<char> {
    (0x20..0x7F).contains(&b).then_some(b as char)
}

// A byte's colour in the dump before cursor and selection styling:
// printable ASCII stands out, zero recedes, the rest is plain.
fn byte_color(b: u8) -> &'static str {
    if b == 0 {
        "\x1b[2m"
    } else if printable(b).is_some() {
        "\x1b[36m"
    } else {
        "\x1b[39m"
    }
}

// The dump, positioned from `row_origin`/`col_origin`: the pane's
// position when live, 0/0 for `freeze`, whose grid is pane-relative (as
// `fileeditor::build_editor_frame`). Rows are padded to `rect.cols`
// rather than ended with `\x1b[K`, which would erase across a vertical
// split; the padding also blanks rows past the end of the file.
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
                // Past the end: the append caret in Insert mode, else blank.
                None if off == cursor && session.mode != Mode::Normal => hex.push_str("\x1b[7m  \x1b[0m"),
                None => hex.push_str("  "),
            }
        }
        out.push_str(&hex);

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
        // Every row is `dump_width` columns, so the rest of the pane is
        // blanked.
        out.push_str(&" ".repeat(rect.cols.saturating_sub(layout.dump_width)));
    }

    if let Some(col) = layout.inspector_col {
        out.push_str(&render_inspector(session, layout, rect, row_origin, col_origin + col));
    }
    // The frame draws its own block cursor, so the terminal's is hidden.
    out.push_str("\x1b[?25l");
    out
}

// Cursor and selection styling over a cell's colour. `active` marks the
// pane the keyboard edits (reverse video) apart from its mirror in the
// other pane (underline), so it's clear where a keystroke lands.
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
    // A half-typed byte shows its high nibble in the cell.
    if is_cursor
        && active
        && session.pane == Pane::Hex
        && let Some(n) = session.pending_nibble
    {
        out.push_str(&format!("{n:x}_"));
    } else {
        out.push_str(text);
    }
    out.push_str("\x1b[0m");
    out
}

// `col` is absolute already; `row_origin` places it as `build_frame`'s
// rows are.
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

// Random bytes as a float are usually a denormal or a huge value, which
// `{:.6}` prints as dozens of digits; scientific notation outside a
// sane range keeps rows one shape.
fn format_float(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    if !v.is_finite() {
        return if v.is_nan() { "NaN".to_string() } else { format!("{v}") };
    }
    let magnitude = v.abs();
    if (1e-4..1e12).contains(&magnitude) { format!("{v:.6}") } else { format!("{v:.6e}") }
}

// The text of the global status row (`repl::render_global_status_row`
// positions and styles it), sized by `term_cols`, since the row spans
// the terminal.
fn status_text(session: &HexSession, layout: Layout, term_cols: usize) -> String {
    let buf = &session.buf;
    // The file name, not the path, as vim shows: a deep path would push the
    // offset, value and mode off the row.
    let name = buf.path().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| "[no name]".to_string());
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
    // row for one frame.
    let text = match &session.status {
        Some(msg) => fit(msg, term_cols),
        None => {
            let used = display_len(&left) + display_len(&right);
            if used + 2 > term_cols { fit(&left, term_cols) } else { format!("{left}{}{right}", " ".repeat(term_cols - used)) }
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
                // A nibble at a time: the first digit is held and shown
                // (see `styled`), the second commits the byte.
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
                // Typed text goes in as itself: a non-ASCII char
                // contributes its UTF-8 bytes.
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

    // Returns the `<C-w>` window command this resolved to, if any, for
    // repl.rs, which owns focus and layout. Everything else is applied here.
    fn handle_outcome(&mut self, outcome: KeyOutcome, rows: usize) -> Option<(WindowCmd, Option<usize>)> {
        if let KeyOutcome::Window(cmd, count) = outcome {
            return Some((cmd, count));
        }
        match outcome {
            KeyOutcome::Pending | KeyOutcome::None => {}
            // `/` and `?` arrive as motions but search the flat bytes here
            // (module doc, point 3).
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
                // Only the ASCII pane gets `r` here; the hex pane takes it
                // earlier, as a byte needs two digits
                // (`handle_normal_key`).
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
            // Ctrl-A/Ctrl-X on a byte, wrapping: 0xff+1 is 0x00.
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
            // No identifiers or language server in a binary, and nothing to
            // fold: these do nothing, like every outcome with no meaning
            // here.
            KeyOutcome::GotoDefinition(_)
            | KeyOutcome::GotoReferences
            | KeyOutcome::DocumentSymbols
            | KeyOutcome::CodeActions
            | KeyOutcome::Fold(..) => {}
            KeyOutcome::ReselectVisual => {
                if let Some((shape, anchor, cursor)) = self.vk.last_visual() {
                    self.buf.set_cursor(cursor.0, cursor.1);
                    self.vk.begin_visual(shape, anchor);
                    self.mode = Mode::Visual;
                }
            }
            KeyOutcome::SwapVisualEnds => {
                if let Some((_, anchor)) = self.vk.visual_anchor() {
                    let cursor = self.buf.cursor();
                    self.buf.set_cursor(anchor.0, anchor.1);
                    self.vk.set_visual_anchor(cursor);
                }
            }
            KeyOutcome::Jump { forward } => {
                let here = self.buf.cursor();
                let target = if forward { self.vk.jump_forward(here) } else { self.vk.jump_back(here) };
                if let Some((r, c)) = target {
                    self.buf.set_cursor(r, c);
                }
            }
            // Vim commands with no meaning over a byte stream say so rather
            // than silently doing nothing.
            KeyOutcome::Join { .. } => self.status = Some("J: rows are a display artifact here, not lines to join".to_string()),
            KeyOutcome::OpenLine { .. } => self.status = Some("o/O: rows are a display artifact here -- use i/a to insert bytes".to_string()),
            KeyOutcome::AddSurround { .. } | KeyOutcome::DeleteSurround { .. } | KeyOutcome::ChangeSurround { .. } => {
                self.status = Some("surround has no meaning in a byte buffer".to_string())
            }
            KeyOutcome::Window(..) | KeyOutcome::Opener => unreachable!("bubbled up before this match"),
        }
        None
    }

    // Visual mode's operators, acting on the selection.
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
    // `true` when the command asks to close this frame (`:q`, `:q!`, `:wq`,
    // `:x`); repl.rs owns the frame stack and acts on it.
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
            // `:0x1f00`, `:4096`: an offset, not vim's line number, since
            // offsets are this editor's coordinates.
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

// How a keystroke ended. `Window` goes to repl.rs, which owns focus and
// layout, as `run_normal_mode_navigation`'s `KeyOutcome::Window` does
// for the file editor; it's the only way to leave without closing.
pub enum HexOutcome {
    Continue,
    Quit,
    Window(WindowCmd, Option<usize>),
    /// Ctrl+T: the fuzzy opener over panes and tracked files, reported like
    /// `Window`, since both belong to the window manager.
    Opener,
    /// Ctrl-L on the colon line: repaint everything, neighbours and tab bar
    /// included. Reported because a full repaint needs the compositor; a
    /// plain `\x1b[H\x1b[2J` would wipe the whole frame with nothing to
    /// repaint it (see repl.rs's `ReadOutcome::CtrlL`).
    Redraw,
}

impl HexSession {
    // Everything before drawing a frame, in the order that keeps undo
    // right: reflow to the pane's width, checkpoint (see `checkpoint`), then
    // bring the cursor into view.
    fn prepare(&mut self, rect: Rect) -> Layout {
        let layout = self.layout(rect);
        self.buf.set_bytes_per_row(layout.bytes_per_row);
        self.checkpoint();
        self.scroll_to_cursor(layout.rows.min(rect.rows));
        layout
    }

    // The live render: the dump plus the global status row. Clears
    // `self.status`, so a transient message shows for one frame.
    pub fn render(&mut self, rect: Rect, term_rows: usize, term_cols: usize) -> String {
        let layout = self.prepare(rect);
        let mut out = render_global_status_row(&status_text(self, layout, term_cols), term_rows);
        out.push_str(&build_frame(self, layout, rect, rect.row, rect.col));
        self.status = None;
        out
    }

    // `fileeditor::freeze_editor_frame`'s counterpart: draws into the
    // session's grid, pane-relative, so a redraw of a detached `Frame::Hex`
    // shows the dump. The status row is outside every pane, so not included.
    pub fn freeze(&mut self, screen: &Rc<RefCell<vt100::Screen>>, rect: Rect) {
        let layout = self.prepare(rect);
        let framed = build_frame(self, layout, rect, 0, 0);
        screen.borrow_mut().feed(framed.as_bytes());
    }

    // One keystroke. `on_idle` is the caller's `service_background_jobs`
    // hook, for the two places this blocks for more input (the colon line
    // and help), so jobs keep draining and resizes are noticed.
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

        // `r` in the hex pane takes two digits, which `VimKeys` can't
        // express, so it's read here: the one step outside `VimKeys`, for
        // the pane where a byte takes two keys.
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
        // Reported before anything is applied, like `Window`: the opener is
        // made of panes and repositories, which this view can't see.
        if matches!(outcome, KeyOutcome::Opener) {
            return HexOutcome::Opener;
        }
        let window = self.handle_outcome(outcome, layout.rows.min(rect.rows));
        // A motion in Visual mode keeps the selection; an operator already
        // ended it in `handle_outcome`.
        if self.mode == Mode::Visual && !self.vk.is_visual() {
            self.mode = Mode::Normal;
        }
        match window {
            Some((cmd, count)) => HexOutcome::Window(cmd, count),
            None => HexOutcome::Continue,
        }
    }

    // The colon line through `editor::read_line`, on the global command row
    // `run_command_mode` uses, without completion: these commands are this
    // view's own.
    fn run_colon_line(&mut self, rect: Rect, term_rows: usize, term_cols: usize, cmd_history: &History, on_idle: &mut dyn FnMut()) -> HexOutcome {
        print!("\x1b[{};1H\x1b[K\x1b[?25h", crate::repl::command_mode_row(term_rows) + 1);
        let _ = io::stdout().flush();
        let mut registers = std::mem::take(&mut self.registers);
        let outcome = editor::read_line(
            ":",
            ":",
            cmd_history,
            true,
            // `ctrl_l_reports`: Ctrl-L comes back as an outcome rather than
            // a raw full-screen clear.
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
            // The caller repaints the hex frame on the next key, so no
            // redraw.
            &mut || {
                on_idle();
                false
            },
            // No shell to ask, and nothing to sweep with the terminal's
            // selection.
            true,
            // No shell, so no mapping table; this isn't the command mode
            // `::bish map -m command` means, and nothing can have queued
            // keys.
            Vec::new(),
            Vec::new(),
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

    // One page, dismissed by any key, as the file editor's `:help`. Drawn in
    // the pane, so split siblings and the tab bar stay; a short pane shows
    // what fits.
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

    // `run_hex_frame` renders into a rect, so the tests do too.
    const RECT: Rect = Rect { row: 0, col: 0, rows: 22, cols: 100 };

    fn session_with(bytes: &[u8]) -> HexSession {
        let b = buf(bytes);
        let undo = UndoTree::new(b.bytes.clone(), (0, 0));
        HexSession {
            buf: b,
            vk: VimKeys::new(),
            // Not `Registers::new()`, whose unnamed register is the real OS
            // clipboard, shared with every other test.
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

    // Mirrors `run_hex_frame`'s loop: checkpoint (inside `render`/`prepare`
    // there, committing the previous key's edit), then the next key, so undo
    // behaves as in the editor.
    fn feed(session: &mut HexSession, keys: &str) {
        for c in keys.chars() {
            let key = match c {
                '\n' => Key::Enter,
                '\x1b' => Key::Escape,
                '\t' => Key::Tab,
                other => Key::Char(other),
            };
            session.checkpoint();
            session.handle_key(key, RECT, 24, RECT.cols, &History::load("/dev/null", 0), &mut || {});
        }
    }

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

    // The shared motion engine drives a byte buffer with no hex-specific
    // code.
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

    // Word motions over Latin-1 bytes find embedded strings with `w`/`b`.
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

    #[test]
    fn every_byte_round_trips_through_a_register() {
        let all: Vec<u8> = (0..=255).collect();
        let text = bytes_to_register_text(&all);
        assert_eq!(register_text_to_bytes(&text), all);
    }

    #[test]
    fn text_yanked_elsewhere_becomes_its_utf8_bytes() {
        // Below U+0100 a char is its byte; above, its UTF-8 encoding.
        assert_eq!(register_text_to_bytes("A"), vec![0x41]);
        assert_eq!(register_text_to_bytes("\u{00e9}"), vec![0xE9]);
        assert_eq!(register_text_to_bytes("\u{2192}"), vec![0xE2, 0x86, 0x92]);
    }

    #[test]
    fn patterns_distinguish_hex_bytes_from_literal_text() {
        assert_eq!(parse_pattern("dead"), Some(Pattern::Bytes(vec![0xDE, 0xAD])));
        assert_eq!(parse_pattern("de ad be ef"), Some(Pattern::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF])));
        assert_eq!(parse_pattern("\"dead\""), Some(Pattern::Text(b"dead".to_vec())));
        assert_eq!(parse_pattern("hello"), Some(Pattern::Text(b"hello".to_vec())));
        // An odd digit count isn't whole bytes, so it's text.
        assert_eq!(parse_pattern("dea"), Some(Pattern::Text(b"dea".to_vec())));
        assert_eq!(parse_pattern("   "), None);
    }

    #[test]
    fn search_finds_a_pattern_straddling_a_row_boundary() {
        // At 16 bytes a row, most patterns cross a boundary.
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

    #[test]
    fn layout_matches_the_familiar_xxd_geometry() {
        let l = compute_layout(24, 100, 0x1000, Some(16), false);
        assert_eq!(l.bytes_per_row, 16);
        assert_eq!(l.offset_width, 8);
        assert_eq!(l.hex_col, 10);
        // 8 offset, 2 gap, 49 hex, 1 gap, 1 `|`: ASCII starts at 60.
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

    // Purely greedy auto-width gave a 110-column terminal 24 bytes a row
    // and no room for the inspector.
    #[test]
    fn auto_width_leaves_room_for_the_inspector_instead_of_squeezing_it_out() {
        let l = compute_layout(24, 110, 0x100, None, true);
        assert!(l.inspector_col.is_some(), "{l:?}");
        assert!(l.dump_width + INSPECTOR_WIDTH <= 110, "{l:?}");
        // Without the inspector, the same terminal fills every column.
        assert!(compute_layout(24, 110, 0x100, None, false).bytes_per_row > l.bytes_per_row);
        // Too narrow for both still gets a usable dump.
        let narrow = compute_layout(24, 60, 0x100, None, true);
        assert!(narrow.inspector_col.is_none());
        assert_eq!(narrow.bytes_per_row, GROUP);
    }

    #[test]
    fn floats_stay_readable_whatever_the_bytes_decode_to() {
        assert_eq!(format_float(0.0), "0");
        assert_eq!(format_float(1.5), "1.500000");
        // 57 digits without the fix.
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
        s.handle_key(Key::CtrlR, RECT, 24, RECT.cols, &History::load("/dev/null", 0), &mut || {});
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
        let history = History::load("/dev/null", 0);
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

        // An existing file is never marked new.
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

    // Strips SGR and cursor-positioning escapes, leaving what the frame
    // shows.
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
        // `show_help` drops what doesn't fit, so the page must fit a 24-row
        // terminal's pane.
        let pane_rows = crate::window::content_rows(24);
        let lines: Vec<&str> = HELP_TEXT.lines().collect();
        assert!(lines.len() <= pane_rows, "the help page is {} lines but a 24-row terminal's pane is {pane_rows}", lines.len());
        // 80 columns: `fit` truncates rather than wraps.
        for line in lines {
            assert!(line.chars().count() <= 80, "help line {line:?} is wider than 80 columns");
        }
    }

    #[test]
    fn every_row_is_painted_out_to_the_pane_width() {
        // With no full-screen clear, each row blanks its own width, including
        // rows past the end of a short file.
        let mut s = session_with(b"AB");
        let rect = Rect { row: 0, col: 0, rows: 6, cols: 90 };
        let frame = s.render(rect, 24, rect.cols);
        // Row 0 is the status line; every row after it reaches `cols`.
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
        // The live render uses the pane's real position...
        let live = build_frame(&s, layout, rect, rect.row, rect.col);
        assert!(live.contains("\x1b[8;31H"), "first row should land at the pane's own top-left");
        // ...and `freeze` the session's grid, which is pane-relative.
        let frozen = build_frame(&s, layout, rect, 0, 0);
        assert!(frozen.contains("\x1b[1;1H"), "frozen frame should start at the grid's own origin");
    }

    #[test]
    fn no_rendered_row_is_wider_than_its_pane() {
        let mut s = session_with(&(0u8..=255).collect::<Vec<u8>>());
        for cols in [80usize, 100, 140] {
            let rect = Rect { row: 0, col: 0, rows: 22, cols };
            s.inspector = true;
            // Skipping the status row, which spans the terminal
            // (`status_text`).
            let frame = s.render(rect, 24, cols);
            let dump = frame.split_once("\x1b[2m").map(|(_, rest)| rest.to_string()).unwrap_or(frame);
            for line in plain(&dump).lines() {
                assert!(line.chars().count() <= cols, "row {:?} is {} wide, the pane is {cols}", line, line.chars().count());
            }
        }
    }
}
