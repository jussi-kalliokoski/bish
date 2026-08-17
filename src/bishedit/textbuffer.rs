// The mutable multi-line buffer -- the "once real editing lands" half of
// `Buffer`'s own module doc comment. Everything built before this
// (`editor.rs`'s `LineBuffer`) only ever mutated a single flat `Vec<char>`
// line; a real file has many. Navigation goes through the shared `Buffer`
// trait exactly like `ScreenBuffer`/`LineBuffer` already do; mutation is
// deliberately *not* added to that trait (mutation has never gone through
// it, even for `LineBuffer` -- keeping it bespoke per concrete type is the
// established pattern here, not a shortcut).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use super::lint;
use super::motion;
use super::registers::{RegisterShape, RegisterValue, Registers};
use super::Buffer;

pub struct TextBuffer {
    // Always at least one line, matching a real file (an empty file is
    // one empty line, same as vim's own "new, empty buffer" convention --
    // see `new_unnamed`/`delete_range`'s own Linewise branch, which
    // restores this invariant after deleting every line).
    lines: Vec<Vec<char>>,
    cursor: (usize, usize),
    vtop: usize,
    vheight: usize,
    marks: HashMap<char, (usize, usize)>,
    // Visual mode's own committed selections -- same field, same shape,
    // same reasoning as `repl.rs`'s `ScreenBuffer::selections`.
    pub selections: Vec<motion::MotionRange>,
    // `:diag`'s own last result (see fileeditor::diagnose_buffer) -- rides
    // along with the buffer exactly like `selections` does (survives a
    // Ctrl+Space detach/reattach, since both live on the one thing that
    // does), cleared by `:diag clear` or implicitly by any real edit (see
    // insert_text/delete_range/join_lines below): a lint::Diagnostic's
    // start/end are char offsets into this buffer's *current* text, so
    // keeping a stale one around past the edit that invalidated its
    // position would just be showing the user a lie.
    pub diagnostics: Vec<lint::Diagnostic>,
    dirty: bool,
    path: Option<PathBuf>,
}

impl TextBuffer {
    pub fn new_unnamed(vheight: usize) -> TextBuffer {
        TextBuffer {
            lines: vec![Vec::new()],
            cursor: (0, 0),
            vtop: 0,
            vheight: vheight.max(1),
            marks: HashMap::new(),
            selections: Vec::new(),
            diagnostics: Vec::new(),
            dirty: false,
            path: None,
        }
    }

    // A nonexistent path opens as a fresh unnamed-but-pathed buffer --
    // vim's own ":e newfile" behavior (the file is created on first
    // `:w`, not on open).
    pub fn open(path: &Path, vheight: usize) -> io::Result<TextBuffer> {
        let lines = match std::fs::read_to_string(path) {
            Ok(text) => {
                // A trailing newline is the normal case (the file's own
                // last line ends in "\n", same as `save`'s own output
                // shape below) -- stripped here so it doesn't show up as
                // a phantom trailing empty line; anything else in the
                // file (embedded blank lines, no trailing newline at
                // all) is preserved exactly.
                let text = text.strip_suffix('\n').unwrap_or(&text);
                let lines: Vec<Vec<char>> = text.split('\n').map(|l| l.chars().collect()).collect();
                if lines.is_empty() { vec![Vec::new()] } else { lines }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => vec![Vec::new()],
            Err(e) => return Err(e),
        };
        Ok(TextBuffer {
            lines,
            cursor: (0, 0),
            vtop: 0,
            vheight: vheight.max(1),
            marks: HashMap::new(),
            selections: Vec::new(),
            diagnostics: Vec::new(),
            dirty: false,
            path: Some(path.to_path_buf()),
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    // Writes every line joined by "\n", plus one trailing "\n" (matching
    // a real text file, and `open`'s own inverse -- see its doc comment).
    // `path` overrides `self.path` for this write and, if this buffer had
    // none yet, becomes the buffer's own path afterward -- vim's own
    // ":w newname" behavior on an unnamed buffer.
    pub fn save(&mut self, path: Option<&Path>) -> io::Result<()> {
        let target = path.or(self.path.as_deref()).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "No file name"))?;
        let mut text: String = self.lines.iter().map(|l| l.iter().collect::<String>()).collect::<Vec<_>>().join("\n");
        text.push('\n');
        std::fs::write(target, text)?;
        if self.path.is_none() {
            self.path = Some(target.to_path_buf());
        }
        self.dirty = false;
        Ok(())
    }

    // Splits `text` on '\n' and splices it into the buffer at `at`,
    // joining/splitting lines as needed -- the one real new primitive
    // this whole feature depends on: nothing before this ever inserted
    // text that could itself contain newlines into a multi-line buffer.
    // Returns the cursor position right after the inserted text (vim's
    // own convention -- see `apply_put`'s own doc comment in vimkeys.rs
    // for the single-line precedent this generalizes).
    pub fn insert_text(&mut self, at: (usize, usize), text: &str) -> (usize, usize) {
        if text.is_empty() {
            return at;
        }
        let row = at.0.min(self.lines.len().saturating_sub(1));
        let parts: Vec<&str> = text.split('\n').collect();
        let line = std::mem::take(&mut self.lines[row]);
        let col = at.1.min(line.len());
        let before = line[..col].to_vec();
        let after = line[col..].to_vec();

        let new_pos = if parts.len() == 1 {
            let mut new_line = before;
            new_line.extend(parts[0].chars());
            let new_col = new_line.len();
            new_line.extend(after);
            self.lines[row] = new_line;
            (row, new_col)
        } else {
            let mut new_lines: Vec<Vec<char>> = Vec::with_capacity(parts.len());
            let mut first = before;
            first.extend(parts[0].chars());
            new_lines.push(first);
            for part in &parts[1..parts.len() - 1] {
                new_lines.push(part.chars().collect());
            }
            let mut last: Vec<char> = parts[parts.len() - 1].chars().collect();
            let new_col = last.len();
            last.extend(after);
            new_lines.push(last);
            let new_row = row + parts.len() - 1;
            self.lines.splice(row..=row, new_lines);
            (new_row, new_col)
        };
        self.dirty = true;
        self.diagnostics.clear();
        self.cursor = new_pos;
        // `.` -- vim's own "position of the last change" mark (`` `. ``),
        // set automatically by every mutation here rather than at each of
        // fileeditor.rs's own many call sites -- these three methods are
        // the only places a real edit actually happens (and, per
        // `diagnostics`'s own doc comment, the only places a stale
        // diagnostic's position could actually go wrong).
        self.marks.insert('.', new_pos);
        new_pos
    }

    // Removes a `MotionRange` that may span several real lines (joining
    // the two cut ends into one line), returning the removed text.
    // `motion::extract_text`/`motion::motion_range` already resolve a
    // range correctly across multiple lines (built for `ScreenBuffer`'s
    // own y{motion} originally) -- this is the missing mutating
    // counterpart. Row indices are clamped to the buffer's own *current*
    // bounds up front, so a stale/overlapping range (multiple Visual
    // selections deleted back-to-back -- see `delete_selections` below)
    // degrades gracefully instead of panicking.
    pub fn delete_range(&mut self, range: &motion::MotionRange) -> String {
        let last_row = self.lines.len().saturating_sub(1);
        let range = motion::MotionRange { shape: range.shape, from: (range.from.0.min(last_row), range.from.1), to: (range.to.0.min(last_row), range.to.1) };
        let text = motion::extract_text(&*self, &range);
        match range.shape {
            motion::MotionShape::Linewise => {
                self.lines.drain(range.from.0..=range.to.0);
                if self.lines.is_empty() {
                    self.lines.push(Vec::new());
                }
                let row = range.from.0.min(self.lines.len() - 1);
                self.cursor = (row, 0);
            }
            _ => {
                let end_col = if range.shape == motion::MotionShape::Inclusive { range.to.1 + 1 } else { range.to.1 };
                let last_line = &self.lines[range.to.0];
                let after: Vec<char> = last_line.get(end_col.min(last_line.len())..).map(|s| s.to_vec()).unwrap_or_default();
                let first_line = &self.lines[range.from.0];
                let mut joined: Vec<char> = first_line[..range.from.1.min(first_line.len())].to_vec();
                joined.extend(after);
                self.lines.splice(range.from.0..=range.to.0, std::iter::once(joined));
                let row = range.from.0;
                self.cursor = (row, range.from.1.min(self.lines[row].len()));
            }
        }
        self.dirty = true;
        self.diagnostics.clear();
        self.marks.insert('.', self.cursor);
        text
    }

    // `J`/`gJ`: joins `count` lines (minimum 2, matching vim -- a bare `J`
    // or `1J` both just join the current line with the next one) starting
    // at the cursor's own line. `with_space` selects vim's default
    // whitespace-aware join (strips each joined-in line's own leading
    // whitespace and inserts a single space, unless the current line
    // already ends in whitespace, the joined-in line is empty, or it
    // starts with ')') vs. `gJ`'s raw concatenation. Returns whether
    // anything was actually joined (false at the last line, matching
    // every other buffer command's own "nothing happened" signal).
    // Cursor lands at the last join's own boundary, matching vim.
    pub fn join_lines(&mut self, count: usize, with_space: bool) -> bool {
        let (row, _) = self.cursor;
        let available = self.lines.len().saturating_sub(1).saturating_sub(row);
        let joins = count.max(2).saturating_sub(1).min(available);
        if joins == 0 {
            return false;
        }
        let mut join_col = self.lines[row].len();
        for _ in 0..joins {
            let mut next = self.lines.remove(row + 1);
            if with_space {
                let leading_ws = next.iter().take_while(|c| c.is_whitespace()).count();
                next.drain(0..leading_ws);
                let cur_ends_blank = self.lines[row].last().is_none_or(|c| c.is_whitespace());
                let next_starts_close_paren = next.first() == Some(&')');
                join_col = self.lines[row].len();
                if !cur_ends_blank && !next.is_empty() && !next_starts_close_paren {
                    self.lines[row].push(' ');
                }
            } else {
                join_col = self.lines[row].len();
            }
            self.lines[row].extend(next);
        }
        self.cursor = (row, join_col.min(self.lines[row].len().saturating_sub(1)));
        self.dirty = true;
        self.diagnostics.clear();
        self.marks.insert('.', self.cursor);
        true
    }

    // Visual mode's own `y`: every selection, concatenated with no
    // separator -- same rule `editor.rs`'s own `yank_selections_line`/
    // repl.rs's `yank_selections` already establish (a `Linewise` part
    // already ends in "\n", so it naturally lands on its own line).
    pub fn yank_selections(&self, registers: &mut Registers, register: Option<char>) {
        if self.selections.is_empty() {
            return;
        }
        let mut text = String::new();
        let mut shape = RegisterShape::Char;
        for range in &self.selections {
            text.push_str(&motion::extract_text(self, range));
            if range.shape == motion::MotionShape::Linewise {
                shape = RegisterShape::Line;
            }
        }
        registers.record_yank(register, RegisterValue { text, shape });
    }

    // Visual mode's own `d`: removes every selection, writing the
    // concatenated deleted text to a register first (vim's own "delete
    // always yanks" rule). Selections are removed highest-position
    // first (`(line, col)` ordered, `(usize, usize)`'s own `Ord` is
    // already exactly that lexicographic order) so removing a later one
    // never shifts a still-pending earlier one's own coordinates --
    // same reasoning `editor.rs`'s own `delete_selections` already
    // established for a single line, just ordered by position instead
    // of a bare column. `delete_range`'s own defensive row-clamping
    // (see its doc comment) covers the rest against any pathological
    // overlap between selections.
    pub fn delete_selections(&mut self, registers: &mut Registers, register: Option<char>) -> bool {
        if self.selections.is_empty() {
            return false;
        }
        let mut text = String::new();
        let mut shape = RegisterShape::Char;
        for range in &self.selections {
            text.push_str(&motion::extract_text(self, range));
            if range.shape == motion::MotionShape::Linewise {
                shape = RegisterShape::Line;
            }
        }
        registers.record_delete(register, RegisterValue { text, shape });

        let leftmost = self.selections.iter().map(|r| r.from).min().unwrap();
        let mut ranges = self.selections.clone();
        ranges.sort_by_key(|r| std::cmp::Reverse(r.from));
        for range in &ranges {
            self.delete_range(range);
        }
        let row = leftmost.0.min(self.lines.len() - 1);
        self.cursor = (row, leftmost.1.min(self.lines[row].len()));
        true
    }

    // Visual mode's own `p`/`P`: replaces every selection with the
    // register's content, broadcasting the same replacement to each
    // (see `editor.rs`'s own `put_over_selections` for why: no vim
    // precedent for multi-selection paste, and "replace every one of
    // these with that" is the more useful behavior). Unlike that
    // single-line version, this uses the register's *raw* text, embedded
    // newlines and all -- `insert_text` already understands them, so a
    // linewise yank pasted over a charwise selection correctly splits
    // the surrounding line around the inserted lines, matching real
    // vim's own visual-`p` shape for that case, with no special-casing
    // needed here.
    pub fn put_over_selections(&mut self, registers: &mut Registers, register: Option<char>) -> bool {
        if self.selections.is_empty() {
            return false;
        }
        let text = registers.read(register).text;
        if text.is_empty() {
            return false;
        }

        let leftmost = self.selections.iter().map(|r| r.from).min().unwrap();
        let mut ranges = self.selections.clone();
        ranges.sort_by_key(|r| std::cmp::Reverse(r.from));
        let mut cursor_at = self.cursor;
        for range in &ranges {
            self.delete_range(range);
            let last_row = self.lines.len().saturating_sub(1);
            let at = (range.from.0.min(last_row), range.from.1.min(self.lines[range.from.0.min(last_row)].len()));
            let new_cursor = self.insert_text(at, &text);
            if range.from == leftmost {
                cursor_at = new_cursor;
            }
        }
        self.cursor = cursor_at;
        true
    }
}

impl Buffer for TextBuffer {
    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn line_len(&self, line: usize) -> usize {
        self.lines.get(line).map_or(0, |l| l.len())
    }

    fn char_at(&self, line: usize, col: usize) -> Option<char> {
        self.lines.get(line).and_then(|l| l.get(col)).copied()
    }

    fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    fn set_cursor(&mut self, line: usize, col: usize) {
        let row = line.min(self.lines.len().saturating_sub(1));
        let col = col.min(self.lines[row].len());
        self.cursor = (row, col);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make(text: &str) -> TextBuffer {
        let lines: Vec<Vec<char>> = text.split('\n').map(|l| l.chars().collect()).collect();
        TextBuffer { lines, cursor: (0, 0), vtop: 0, vheight: 10, marks: HashMap::new(), selections: Vec::new(), diagnostics: Vec::new(), dirty: false, path: None }
    }

    fn text_of(buf: &TextBuffer) -> String {
        buf.lines.iter().map(|l| l.iter().collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    fn make_registers() -> Registers {
        Registers::new_for_test()
    }

    #[test]
    fn insert_text_single_line_no_newline() {
        let mut buf = make("foo bar");
        let new_cursor = buf.insert_text((0, 3), "XYZ");
        assert_eq!(text_of(&buf), "fooXYZ bar");
        assert_eq!(new_cursor, (0, 6));
        assert!(buf.is_dirty());
    }

    #[test]
    fn insert_text_splits_the_line_on_embedded_newlines() {
        let mut buf = make("foobar");
        let new_cursor = buf.insert_text((0, 3), "1\n2\n3");
        assert_eq!(text_of(&buf), "foo1\n2\n3bar");
        assert_eq!(new_cursor, (2, 1));
    }

    #[test]
    fn insert_text_at_end_of_buffer_appends_a_new_line() {
        let mut buf = make("foo");
        let new_cursor = buf.insert_text((0, 3), "\nbar");
        assert_eq!(text_of(&buf), "foo\nbar");
        assert_eq!(new_cursor, (1, 3));
    }

    #[test]
    fn delete_range_within_one_line() {
        let mut buf = make("foo bar baz");
        let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 4), to: (0, 6) };
        let deleted = buf.delete_range(&range);
        assert_eq!(deleted, "bar");
        assert_eq!(text_of(&buf), "foo  baz");
        assert_eq!(buf.cursor(), (0, 4));
    }

    #[test]
    fn delete_range_spanning_two_lines_joins_them() {
        let mut buf = make("foo bar\nbaz qux");
        // From the space before "bar" through the space before "qux" --
        // removes "bar\nbaz " and joins "foo " with "qux".
        let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (0, 4), to: (1, 4) };
        let deleted = buf.delete_range(&range);
        assert_eq!(deleted, "bar\nbaz ");
        assert_eq!(text_of(&buf), "foo qux");
        assert_eq!(buf.cursor(), (0, 4));
    }

    #[test]
    fn delete_range_spanning_three_lines_removes_the_middle_ones_entirely() {
        let mut buf = make("one\ntwo\nthree\nfour");
        let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 1), to: (2, 1) };
        buf.delete_range(&range);
        assert_eq!(text_of(&buf), "oree\nfour");
    }

    #[test]
    fn delete_range_linewise_removes_whole_lines_and_never_leaves_zero_lines() {
        let mut buf = make("one\ntwo\nthree");
        let range = motion::MotionRange { shape: motion::MotionShape::Linewise, from: (0, 0), to: (2, 0) };
        let deleted = buf.delete_range(&range);
        assert_eq!(deleted, "one\ntwo\nthree\n");
        assert_eq!(text_of(&buf), "");
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.cursor(), (0, 0));
    }

    #[test]
    fn mutations_set_the_dot_mark_at_the_cursors_own_landing_spot() {
        let mut buf = make("hello");
        assert_eq!(buf.get_mark('.'), None);
        let pos = buf.insert_text((0, 5), "!");
        assert_eq!(buf.get_mark('.'), Some(pos));

        let mut buf = make("one two");
        let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (0, 0), to: (0, 4) };
        buf.delete_range(&range);
        assert_eq!(buf.get_mark('.'), Some(buf.cursor()));

        let mut buf = make("one\ntwo");
        buf.set_cursor(0, 0);
        buf.join_lines(2, true);
        assert_eq!(buf.get_mark('.'), Some(buf.cursor()));
    }

    #[test]
    fn join_lines_default_inserts_a_space_and_strips_leading_whitespace() {
        let mut buf = make("one\n   two\nthree");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(2, true));
        assert_eq!(text_of(&buf), "one two\nthree");
        assert_eq!(buf.cursor(), (0, 3)); // lands on the inserted space
        assert!(buf.is_dirty());
    }

    #[test]
    fn join_lines_no_space_when_current_line_already_ends_blank() {
        let mut buf = make("one   \ntwo");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(2, true));
        assert_eq!(text_of(&buf), "one   two");
    }

    #[test]
    fn join_lines_no_space_before_a_leading_close_paren() {
        let mut buf = make("foo(a\n)bar");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(2, true));
        assert_eq!(text_of(&buf), "foo(a)bar");
    }

    #[test]
    fn join_lines_empty_joined_line_adds_nothing() {
        let mut buf = make("one\n\ntwo");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(2, true));
        assert_eq!(text_of(&buf), "one\ntwo");
    }

    #[test]
    fn gjoin_is_raw_concatenation_no_space_no_stripping() {
        let mut buf = make("one\n   two");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(2, false));
        assert_eq!(text_of(&buf), "one   two");
    }

    #[test]
    fn join_lines_count_joins_several_lines_at_once() {
        let mut buf = make("one\ntwo\nthree\nfour");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(3, true));
        assert_eq!(text_of(&buf), "one two three\nfour");
    }

    #[test]
    fn join_lines_count_of_one_behaves_like_two() {
        let mut buf = make("one\ntwo\nthree");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(1, true));
        assert_eq!(text_of(&buf), "one two\nthree");
    }

    #[test]
    fn join_lines_at_the_last_line_is_a_no_op() {
        let mut buf = make("only");
        buf.set_cursor(0, 0);
        assert!(!buf.join_lines(2, true));
        assert_eq!(text_of(&buf), "only");
        assert!(!buf.is_dirty());
    }

    #[test]
    fn join_lines_count_past_the_end_clamps_to_the_last_line() {
        let mut buf = make("one\ntwo\nthree");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(100, true));
        assert_eq!(text_of(&buf), "one two three");
    }

    #[test]
    fn open_and_save_round_trip_a_real_file() {
        let dir = std::env::temp_dir().join(format!("bish-textbuffer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert_eq!(text_of(&buf), "alpha\nbeta\ngamma");
        assert!(!buf.is_dirty());

        buf.insert_text((0, 5), "!");
        assert!(buf.is_dirty());
        buf.save(None).unwrap();
        assert!(!buf.is_dirty());

        let saved = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(saved, "alpha!\nbeta\ngamma\n");
    }

    #[test]
    fn open_a_nonexistent_path_yields_a_fresh_buffer_with_that_path_remembered() {
        let path = std::env::temp_dir().join(format!("bish-textbuffer-nonexistent-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert_eq!(text_of(&buf), "");
        assert_eq!(buf.path(), Some(path.as_path()));
        buf.insert_text((0, 0), "hi");
        buf.save(None).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(saved, "hi\n");
    }

    fn range(from: (usize, usize), to: (usize, usize)) -> motion::MotionRange {
        motion::MotionRange { shape: motion::MotionShape::Inclusive, from, to }
    }

    #[test]
    fn yank_selections_concatenates_across_lines_with_no_separator() {
        let mut buf = make("foo bar\nbaz qux");
        buf.selections = vec![range((0, 0), (0, 2)), range((1, 0), (1, 2))]; // "foo", "baz"
        let mut registers = make_registers();
        buf.yank_selections(&mut registers, None);
        assert_eq!(registers.read(None).text, "foobaz");
    }

    #[test]
    fn delete_selections_removes_every_range_leftmost_cursor_concatenated_register() {
        let mut buf = make("foo bar\nbaz qux");
        buf.selections = vec![range((0, 0), (0, 2)), range((1, 0), (1, 2))]; // "foo", "baz"
        let mut registers = make_registers();
        assert!(buf.delete_selections(&mut registers, None));
        assert_eq!(text_of(&buf), " bar\n qux");
        assert_eq!(registers.read(None).text, "foobaz");
        assert_eq!(buf.cursor(), (0, 0));
    }

    #[test]
    fn put_over_selections_broadcasts_multiline_register_content() {
        let mut buf = make("foo bar\nbaz qux");
        buf.selections = vec![range((0, 0), (0, 2)), range((1, 0), (1, 2))]; // "foo", "baz"
        let mut registers = make_registers();
        registers.write(None, RegisterValue { text: "X\nY".to_string(), shape: RegisterShape::Char });
        assert!(buf.put_over_selections(&mut registers, None));
        assert_eq!(text_of(&buf), "X\nY bar\nX\nY qux");
        // Register itself untouched -- same broadcast reasoning as
        // editor.rs's own put_over_selections.
        assert_eq!(registers.read(None).text, "X\nY");
    }

    #[test]
    fn put_over_selections_is_a_no_op_with_an_empty_register() {
        let mut buf = make("foo bar");
        buf.selections = vec![range((0, 0), (0, 2))];
        let mut registers = make_registers();
        assert!(!buf.put_over_selections(&mut registers, None));
        assert_eq!(text_of(&buf), "foo bar");
    }
}
