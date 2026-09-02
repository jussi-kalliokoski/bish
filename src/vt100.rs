// A scoped VT100/ANSI terminal emulator: byte-stream parser + virtual
// screen grid, fed uniformly by a session's OutputSink bytes (exec.rs)
// and a job's pty master bytes (pty.rs). Not a full xterm clone -- covers
// enough of the control-sequence surface for vim/htop/less to render
// correctly (cursor movement, erase, SGR colors/attrs, line/char editing,
// scrolling regions, the alternate screen buffer, cursor visibility,
// line wrap, G0 charset switching for curses box-drawing, and safely
// skipping OSC/DCS bodies without corrupting the grid). Explicitly out of
// scope: Sixel/Kitty/iTerm2 images, double-width/height lines, CJK wide
// characters, real mouse-input forwarding, full G1-G3 charset switching,
// byte-for-byte xterm terminfo compatibility.
//
// Not yet wired into exec.rs/repl.rs: real usage (rendering a window's
// hidden session/job into its own grid) lands with the M9 compositor,
// same "land the seam, wire it in later" pattern as pty.rs.
#![allow(dead_code)]

use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct CellAttrs {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strikethrough: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: CellAttrs,
}

impl Default for Cell {
    fn default() -> Cell {
        Cell { ch: ' ', fg: Color::Default, bg: Color::Default, attrs: CellAttrs::default() }
    }
}

// The inverse of this module's own SGR parsing: turns a cell's resolved
// color/attrs back into the ANSI codes that reproduce them, so the real
// terminal ends up showing the same thing the grid recorded. Shared by
// repl.rs's render_row (a Screen's own cells) and the syntax-highlighting
// renderer's render_styled (synthesized StyledSpan-derived cells) -- both
// need the same run-coalescing "cell style -> SGR escape" step.
pub fn sgr_codes(fg: Color, bg: Color, attrs: CellAttrs) -> String {
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
        Color::Default => {}
        Color::Indexed(i) if i < 8 => codes.push(format!("{}", 30 + i)),
        Color::Indexed(i) if i < 16 => codes.push(format!("{}", 90 + (i - 8))),
        Color::Indexed(i) => codes.push(format!("38;5;{}", i)),
        Color::Rgb(r, g, b) => codes.push(format!("38;2;{};{};{}", r, g, b)),
    }
    match bg {
        Color::Default => {}
        Color::Indexed(i) if i < 8 => codes.push(format!("{}", 40 + i)),
        Color::Indexed(i) if i < 16 => codes.push(format!("{}", 100 + (i - 8))),
        Color::Indexed(i) => codes.push(format!("48;5;{}", i)),
        Color::Rgb(r, g, b) => codes.push(format!("48;2;{};{};{}", r, g, b)),
    }
    format!("\x1b[{}m", codes.join(";"))
}

// The standard DEC Special Graphics mapping (ESC ( 0), the subset curses
// actually relies on for box-drawing (ncurses' default `acsc` string).
fn dec_special_graphics(c: char) -> char {
    match c {
        '`' => '\u{25C6}', // diamond
        'a' => '\u{2592}', // checkerboard
        'f' => '\u{00B0}', // degree
        'g' => '\u{00B1}', // plus/minus
        '~' => '\u{00B7}', // bullet
        ',' => '\u{2190}', // left arrow
        '+' => '\u{2192}', // right arrow
        '-' => '\u{2191}', // up arrow
        '.' => '\u{2193}', // down arrow
        '0' => '\u{2588}', // solid block
        'j' => '\u{2518}', // lower-right corner
        'k' => '\u{2510}', // upper-right corner
        'l' => '\u{250C}', // upper-left corner
        'm' => '\u{2514}', // lower-left corner
        'n' => '\u{253C}', // cross
        'q' => '\u{2500}', // horizontal line
        't' => '\u{251C}', // left tee
        'u' => '\u{2524}', // right tee
        'v' => '\u{2534}', // bottom tee
        'w' => '\u{252C}', // top tee
        'x' => '\u{2502}', // vertical line
        other => other,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Charset {
    Ascii,
    DecLine,
}

pub struct Grid {
    pub rows: usize,
    pub cols: usize,
    cells: Vec<Cell>,
    // Per-row "this row's content didn't end here on purpose -- it just
    // ran out of columns and autowrap continued it onto the next row"
    // flag, set only by print_char's own pending_wrap-consuming branch
    // (never by an explicit CR/LF, which always clears pending_wrap
    // first). This is what lets a Buffer built over a Screen (ScreenBuffer
    // in repl.rs) tell a real line break apart from a column-width
    // artifact, so yanking/selecting a long wrapped line doesn't garble
    // it with a newline that was never actually in the source bytes.
    wrapped: Vec<bool>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pending_wrap: bool,
    saved_cursor: (usize, usize),
    scroll_top: usize,
    scroll_bottom: usize,
}

impl Grid {
    fn new(rows: usize, cols: usize) -> Grid {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Grid {
            rows,
            cols,
            cells: vec![Cell::default(); rows * cols],
            wrapped: vec![false; rows],
            cursor_row: 0,
            cursor_col: 0,
            pending_wrap: false,
            saved_cursor: (0, 0),
            scroll_top: 0,
            scroll_bottom: rows - 1,
        }
    }

    // Whether every cell of `row` is untouched -- what tells a shrink
    // that a row below the cursor is spare space it can just discard
    // rather than history it has to preserve.
    fn row_is_blank(&self, row: usize) -> bool {
        (0..self.cols).all(|c| self.cells[row * self.cols + c] == Cell::default())
    }

    // When shrinking rows, keeps the *bottom* `rows` of the old grid --
    // the ones most likely to still hold the live cursor/prompt -- not
    // the top ones: a naive top-anchored copy (this function's own
    // previous behavior) silently threw away whatever the cursor was
    // actually sitting on, leaving a resized-down pane showing stale
    // top-of-screen content with a fresh prompt then written on top of
    // it at the wrong row entirely (e.g. splitting a window that's
    // already scrolled full of output). Growing keeps everything,
    // top-anchored (`src_start_row` is 0 whenever `rows >= self.rows`),
    // since nothing needs discarding either way -- `Screen::resize` then
    // hands back a matching number of scrollback rows via
    // `prepend_rows`, which is what makes a shrink and a later re-grow
    // cancel out instead of losing what the shrink pushed away. Returns
    // whatever rows
    // this dropped off the top (each paired with its own wrapped flag,
    // same shape as `scroll_up`'s own return) so `Screen::resize` can
    // push them into scrollback exactly like an ordinary scroll would --
    // this function doesn't know whether it's the primary or alternate
    // grid (only `Screen` does), so it can't decide that itself.
    fn resize(&mut self, rows: usize, cols: usize) -> Vec<(Vec<Cell>, bool)> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let overlap_rows = self.rows.min(rows);
        // Which end a shrink takes its rows from. Blank rows *below* the
        // cursor carry nothing, so they go first and are simply
        // discarded -- only once those run out does anything real get
        // pushed off the top into scrollback. That's what makes a shrink
        // and a later re-grow cancel out for a grid that isn't full yet
        // (a pane holding a couple of prompts and acres of blank space,
        // which `:diag` expanding and collapsing resizes twice in a row):
        // top-anchored dropping would push those prompts into scrollback
        // and clamp the cursor to 0, losing where it really was, and
        // `prepend_rows` on the way back has no way to work that out
        // again. A grid that *is* full (the cursor on its last row --
        // "already scrolled full of output" above) has no spare rows
        // here, so it drops from the top exactly as before.
        let shrink = self.rows.saturating_sub(rows);
        let mut from_bottom = 0;
        while from_bottom < shrink {
            let candidate = self.rows - 1 - from_bottom;
            if candidate <= self.cursor_row || !self.row_is_blank(candidate) {
                break;
            }
            from_bottom += 1;
        }
        let src_start_row = shrink - from_bottom;
        let mut dropped = Vec::with_capacity(src_start_row);
        for r in 0..src_start_row {
            let row: Vec<Cell> = (0..self.cols).map(|c| self.cells[r * self.cols + c]).collect();
            dropped.push((row, self.wrapped[r]));
        }
        let overlap_cols = self.cols.min(cols);
        let mut new_cells = vec![Cell::default(); rows * cols];
        let mut new_wrapped = vec![false; rows];
        for r in 0..overlap_rows {
            let src_row = src_start_row + r;
            for c in 0..overlap_cols {
                new_cells[r * cols + c] = self.cells[src_row * self.cols + c];
            }
            new_wrapped[r] = self.wrapped[src_row];
        }
        self.cells = new_cells;
        self.wrapped = new_wrapped;
        self.rows = rows;
        self.cols = cols;
        self.cursor_row = self.cursor_row.saturating_sub(src_start_row).min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.pending_wrap = false;
        dropped
    }

    // `resize`'s own counterpart for the growing direction: puts
    // `restored` rows back at the top, shifting what's already here down
    // by that many rows and carrying the cursor with it. `restored` is
    // in top-to-bottom order, oldest first.
    //
    // Rows come out of a scrollback that may have been captured at a
    // different width, so each is padded or truncated to this grid's own
    // columns rather than assumed to fit -- a shrink-then-regrow that
    // also changed width puts back as much of each line as there's now
    // room for, instead of indexing past the end of it.
    fn prepend_rows(&mut self, restored: &[(Vec<Cell>, bool)]) {
        let n = restored.len().min(self.rows);
        if n == 0 {
            return;
        }
        let mut cells = vec![Cell::default(); self.rows * self.cols];
        let mut wrapped = vec![false; self.rows];
        for (r, (row, row_wrapped)) in restored[restored.len() - n..].iter().enumerate() {
            for c in 0..self.cols.min(row.len()) {
                cells[r * self.cols + c] = row[c];
            }
            wrapped[r] = *row_wrapped;
        }
        for r in 0..self.rows - n {
            for c in 0..self.cols {
                cells[(r + n) * self.cols + c] = self.cells[r * self.cols + c];
            }
            wrapped[r + n] = self.wrapped[r];
        }
        self.cells = cells;
        self.wrapped = wrapped;
        self.cursor_row = (self.cursor_row + n).min(self.rows - 1);
    }

    // Clamped, not a bounds-checked panic: a caller computing (row, col)
    // from some other snapshot of this grid's own size (a pane rect
    // cached before a sibling resize, say -- see repl.rs's TerminalFrame::
    // capture for a real one that used to crash here) can legitimately
    // race a real resize elsewhere and hand in a value that's gone stale
    // by a row or column. Reading/writing the nearest real cell instead
    // of panicking is what a terminal emulator embedded in an
    // interactive shell needs here: a transient, self-healing one-frame
    // rendering glitch from stale input is categorically better than
    // taking the whole process down over what real callers already try
    // hard to keep in sync (this is a last-resort safety net, not a
    // license to skip that effort).
    fn clamped_index(&self, row: usize, col: usize) -> usize {
        let row = row.min(self.rows.saturating_sub(1));
        let col = col.min(self.cols.saturating_sub(1));
        row * self.cols + col
    }

    pub fn cell(&self, row: usize, col: usize) -> Cell {
        self.cells[self.clamped_index(row, col)]
    }

    fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        let idx = self.clamped_index(row, col);
        &mut self.cells[idx]
    }

    // Clamped for the same reason clamped_index is -- a stale row from a
    // caller racing a resize elsewhere shouldn't be able to crash this.
    fn is_wrapped(&self, row: usize) -> bool {
        self.wrapped[row.min(self.rows.saturating_sub(1))]
    }

    fn set_wrapped(&mut self, row: usize, v: bool) {
        let row = row.min(self.rows.saturating_sub(1));
        self.wrapped[row] = v;
    }

    fn clear_all(&mut self) {
        self.cells.iter_mut().for_each(|c| *c = Cell::default());
        self.wrapped.iter_mut().for_each(|w| *w = false);
    }

    fn clear_row_range(&mut self, row: usize, from: usize, to_inclusive: usize) {
        for c in from..=to_inclusive.min(self.cols - 1) {
            *self.cell_mut(row, c) = Cell::default();
        }
        // Clearing from the start of the row means whatever content used
        // to be there (including any wrap flag it carried) is gone --
        // otherwise a row reused for unrelated content (after a scroll,
        // an erase, ...) could wrongly look "joined" to the row after it.
        if from == 0 {
            self.wrapped[row] = false;
        }
    }

    // Scrolls the region [scroll_top, scroll_bottom] up by `n` lines,
    // dropping lines off the top; blank lines fill in at the bottom.
    // Returns the dropped lines plus each one's own wrapped flag (for
    // scrollback capture), only meaningful to the caller when the region
    // spans the whole grid.
    fn scroll_up(&mut self, n: usize) -> Vec<(Vec<Cell>, bool)> {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let region_h = bottom - top + 1;
        let n = n.min(region_h);
        let mut dropped = Vec::new();
        for _ in 0..n {
            let row: Vec<Cell> = (0..self.cols).map(|c| self.cell(top, c)).collect();
            dropped.push((row, self.wrapped[top]));
            for r in top..bottom {
                for c in 0..self.cols {
                    let below = self.cell(r + 1, c);
                    *self.cell_mut(r, c) = below;
                }
                self.wrapped[r] = self.wrapped[r + 1];
            }
            self.clear_row_range(bottom, 0, self.cols - 1);
        }
        dropped
    }

    fn scroll_down(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let region_h = bottom - top + 1;
        let n = n.min(region_h);
        for _ in 0..n {
            for r in (top + 1..=bottom).rev() {
                for c in 0..self.cols {
                    let above = self.cell(r - 1, c);
                    *self.cell_mut(r, c) = above;
                }
                self.wrapped[r] = self.wrapped[r - 1];
            }
            self.clear_row_range(top, 0, self.cols - 1);
        }
    }

    fn insert_lines(&mut self, n: usize) {
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }
        let saved = (self.scroll_top, self.scroll_bottom);
        self.scroll_top = self.cursor_row;
        self.scroll_down(n);
        self.scroll_top = saved.0;
        self.scroll_bottom = saved.1;
    }

    fn delete_lines(&mut self, n: usize) {
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }
        let saved = (self.scroll_top, self.scroll_bottom);
        self.scroll_top = self.cursor_row;
        self.scroll_up(n);
        self.scroll_top = saved.0;
        self.scroll_bottom = saved.1;
    }

    fn insert_chars(&mut self, n: usize) {
        let row = self.cursor_row;
        let start = self.cursor_col;
        for c in (start..self.cols).rev() {
            if c >= start + n {
                let src = self.cell(row, c - n);
                *self.cell_mut(row, c) = src;
            } else {
                *self.cell_mut(row, c) = Cell::default();
            }
        }
    }

    fn delete_chars(&mut self, n: usize) {
        let row = self.cursor_row;
        let start = self.cursor_col;
        for c in start..self.cols {
            let src_idx = c + n;
            if src_idx < self.cols {
                let src = self.cell(row, src_idx);
                *self.cell_mut(row, c) = src;
            } else {
                *self.cell_mut(row, c) = Cell::default();
            }
        }
    }

    fn erase_chars(&mut self, n: usize) {
        let row = self.cursor_row;
        let end = (self.cursor_col + n).min(self.cols);
        for c in self.cursor_col..end {
            *self.cell_mut(row, c) = Cell::default();
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ParserState {
    Ground,
    Escape,
    CsiParam,
    OscString,
    DcsSkip,
    ChangeG0,
    ChangeG1,
}

pub struct Screen {
    pub primary: Grid,
    pub alternate: Grid,
    pub using_alternate: bool,
    pub scrollback: VecDeque<Vec<Cell>>,
    // Parallel to `scrollback`, index-for-index -- whether that scrollback
    // row was a soft-wrap continuation into the row after it (see Grid's
    // own `wrapped` field doc comment). Kept as its own deque rather than
    // folded into `scrollback`'s element type so every existing `Vec<Cell>`
    // read site (`.len()`, indexing, `.get(col)`, ...) is untouched.
    pub scrollback_wrapped: VecDeque<bool>,
    scrollback_limit: usize,
    pub cursor_visible: bool,
    pub autowrap: bool,
    pub mouse_reporting: bool,
    pub bracketed_paste: bool,
    // DECCKM (CSI ?1h/?1l): whether the program currently expects arrow
    // keys encoded as SS3 (ESC O A/B/C/D, "application" mode) rather
    // than the default CSI form (ESC [ A/B/C/D). A curses program (vim,
    // less, ...) toggles this on startup/exit via smkx/rmkx -- almost
    // every terminfo entry (xterm, screen, ...) maps its own cursor-key
    // *input* the same way, so a real terminal switches its own key
    // encoding to match. bish's own compositor can't just relay a job's
    // raw output straight to the real terminal (see render_compositor_
    // frame's doc comment -- it re-renders from grid cell state, which
    // is what makes multiple simultaneously-visible panes possible at
    // all), so this DECSET request would otherwise never reach the real
    // terminal, leaving it stuck sending the *other* encoding than
    // whatever the job now expects. repl.rs's drive_fg_job reads this
    // to re-encode a plain CSI arrow key it receives into SS3 before
    // forwarding, exactly what a real terminal would have done itself.
    pub app_cursor_keys: bool,

    cur_fg: Color,
    cur_bg: Color,
    cur_attrs: CellAttrs,

    g0: Charset,
    g1: Charset,
    shifted_out: bool, // true => G1 currently invoked (via SO / ^N)

    state: ParserState,
    csi_raw: String,
    osc_prev_was_esc: bool,
    dcs_prev_was_esc: bool,

    utf8_buf: Vec<u8>,
    utf8_need: usize,
}

impl Screen {
    pub fn new(rows: usize, cols: usize) -> Screen {
        Screen {
            primary: Grid::new(rows, cols),
            alternate: Grid::new(rows, cols),
            using_alternate: false,
            scrollback: VecDeque::new(),
            scrollback_wrapped: VecDeque::new(),
            scrollback_limit: 5000,
            cursor_visible: true,
            autowrap: true,
            mouse_reporting: false,
            app_cursor_keys: false,
            bracketed_paste: false,
            cur_fg: Color::Default,
            cur_bg: Color::Default,
            cur_attrs: CellAttrs::default(),
            g0: Charset::Ascii,
            g1: Charset::Ascii,
            shifted_out: false,
            state: ParserState::Ground,
            csi_raw: String::new(),
            osc_prev_was_esc: false,
            dcs_prev_was_esc: false,
            utf8_buf: Vec::new(),
            utf8_need: 0,
        }
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        // The primary grid's own rows, only -- pushed into scrollback
        // unconditionally (not gated on `using_alternate` the way
        // line_feed_no_scroll_check's own scroll-driven push is): this
        // is capturing *primary*'s own history regardless of which grid
        // happens to be on screen right now (a resize while an
        // alternate-screen app like vim is open still shrinks the real
        // shell underneath it). The alternate grid's own dropped rows
        // are simply discarded, matching line_feed_no_scroll_check's
        // "alternate screen buffers don't get a scrollback" rule.
        // How many rows the primary grid is about to gain, read before
        // the resize that grants them. Growing and shrinking are mutually
        // exclusive, so at most one of this and `dropped` below is ever
        // non-zero.
        let grew_by = rows.max(1).saturating_sub(self.primary.rows);
        let dropped = self.primary.resize(rows, cols);
        for (line, wrapped) in dropped {
            self.scrollback.push_back(line);
            self.scrollback_wrapped.push_back(wrapped);
            if self.scrollback.len() > self.scrollback_limit {
                self.scrollback.pop_front();
                self.scrollback_wrapped.pop_front();
            }
        }
        // The other direction: rows the grid just gained are filled from
        // the bottom of the scrollback, so a pane that shrinks and grows
        // back -- `:diag` expanding and collapsing, a split closing, a
        // window made taller again -- shows exactly what it showed
        // before, instead of the shrink's rows staying gone with blank
        // space appearing at the bottom. Real terminals restore on growth
        // the same way. The whole document (scrollback followed by the
        // grid) is unchanged by the move either way, which is what keeps
        // Ctrl+Space's own scrollback view consistent across it.
        let restored_count = grew_by.min(self.scrollback.len());
        if restored_count > 0 {
            let mut restored: Vec<(Vec<Cell>, bool)> = Vec::with_capacity(restored_count);
            for _ in 0..restored_count {
                let line = self.scrollback.pop_back().expect("counted against scrollback's own length");
                let wrapped = self.scrollback_wrapped.pop_back().unwrap_or(false);
                restored.push((line, wrapped));
            }
            restored.reverse();
            self.primary.prepend_rows(&restored);
        }
        self.alternate.resize(rows, cols);
    }

    pub fn size(&self) -> (usize, usize) {
        (self.grid().rows, self.grid().cols)
    }

    fn grid(&self) -> &Grid {
        if self.using_alternate { &self.alternate } else { &self.primary }
    }

    fn grid_mut(&mut self) -> &mut Grid {
        if self.using_alternate { &mut self.alternate } else { &mut self.primary }
    }

    pub fn cell(&self, row: usize, col: usize) -> Cell {
        self.grid().cell(row, col)
    }

    // Whether the currently-showing grid's `row` is a soft-wrap
    // continuation into `row + 1` (see Grid's own `wrapped` field doc
    // comment).
    pub fn row_wraps(&self, row: usize) -> bool {
        self.grid().is_wrapped(row)
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.grid().cursor_row, self.grid().cursor_col)
    }

    fn reset_attrs(&mut self) {
        self.cur_fg = Color::Default;
        self.cur_bg = Color::Default;
        self.cur_attrs = CellAttrs::default();
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.feed_byte(b);
        }
    }

    fn feed_byte(&mut self, b: u8) {
        match self.state {
            ParserState::Ground => self.feed_ground(b),
            ParserState::Escape => self.feed_escape(b),
            ParserState::CsiParam => self.feed_csi(b),
            ParserState::OscString => self.feed_osc(b),
            ParserState::DcsSkip => self.feed_dcs(b),
            ParserState::ChangeG0 => {
                self.g0 = if b == b'0' { Charset::DecLine } else { Charset::Ascii };
                self.state = ParserState::Ground;
            }
            ParserState::ChangeG1 => {
                self.g1 = if b == b'0' { Charset::DecLine } else { Charset::Ascii };
                self.state = ParserState::Ground;
            }
        }
    }

    fn feed_ground(&mut self, b: u8) {
        match b {
            0x1B => {
                self.state = ParserState::Escape;
            }
            0x00..=0x06 | 0x0B | 0x0C | 0x0E..=0x1A | 0x1C..=0x1F => {
                self.control(b);
            }
            0x07 => {} // BEL: no-op (no bell/no window title to flash)
            0x08 => self.backspace(),
            0x09 => self.tab(),
            0x0A => self.line_feed(),
            0x0D => self.carriage_return(),
            _ => self.feed_utf8_byte(b),
        }
    }

    fn control(&mut self, b: u8) {
        match b {
            0x0E => self.shifted_out = true,  // SO
            0x0F => self.shifted_out = false, // SI
            _ => {}
        }
    }

    fn feed_utf8_byte(&mut self, b: u8) {
        if self.utf8_need > 0 {
            if b & 0xC0 == 0x80 {
                self.utf8_buf.push(b);
                self.utf8_need -= 1;
                if self.utf8_need == 0 {
                    if let Ok(s) = std::str::from_utf8(&self.utf8_buf) {
                        if let Some(ch) = s.chars().next() {
                            self.print_char(ch);
                        }
                    }
                    self.utf8_buf.clear();
                }
            } else {
                // Invalid continuation -- abandon and reprocess this byte
                // as a fresh lead byte instead of corrupting the grid.
                self.utf8_buf.clear();
                self.utf8_need = 0;
                self.feed_utf8_byte(b);
            }
            return;
        }
        if b < 0x80 {
            self.print_char(b as char);
        } else if b & 0xE0 == 0xC0 {
            self.utf8_buf = vec![b];
            self.utf8_need = 1;
        } else if b & 0xF0 == 0xE0 {
            self.utf8_buf = vec![b];
            self.utf8_need = 2;
        } else if b & 0xF8 == 0xF0 {
            self.utf8_buf = vec![b];
            self.utf8_need = 3;
        } // else: stray continuation/invalid byte, drop it
    }

    fn print_char(&mut self, ch: char) {
        let active = if self.shifted_out { self.g1 } else { self.g0 };
        let ch = if active == Charset::DecLine { dec_special_graphics(ch) } else { ch };

        let cols = self.grid().cols;
        if self.grid().pending_wrap {
            let wrap_row = self.grid().cursor_row;
            self.grid_mut().set_wrapped(wrap_row, true);
            self.line_feed_no_scroll_check();
            self.carriage_return();
            self.grid_mut().pending_wrap = false;
        }
        let (row, col) = (self.grid().cursor_row, self.grid().cursor_col);
        let fg = self.cur_fg;
        let bg = self.cur_bg;
        let attrs = self.cur_attrs;
        *self.grid_mut().cell_mut(row, col) = Cell { ch, fg, bg, attrs };

        if col + 1 >= cols {
            if self.autowrap {
                self.grid_mut().pending_wrap = true;
            }
        } else {
            self.grid_mut().cursor_col = col + 1;
        }
    }

    fn backspace(&mut self) {
        let g = self.grid_mut();
        if g.cursor_col > 0 {
            g.cursor_col -= 1;
        }
        g.pending_wrap = false;
    }

    fn tab(&mut self) {
        let g = self.grid_mut();
        let next = ((g.cursor_col / 8) + 1) * 8;
        g.cursor_col = next.min(g.cols - 1);
        g.pending_wrap = false;
    }

    fn carriage_return(&mut self) {
        let g = self.grid_mut();
        g.cursor_col = 0;
        g.pending_wrap = false;
    }

    // Line feed that respects the scroll region but does not reset column
    // (matches real LF semantics; carriage_return is separate).
    fn line_feed(&mut self) {
        self.line_feed_no_scroll_check();
    }

    fn line_feed_no_scroll_check(&mut self) {
        let at_bottom = self.grid().cursor_row == self.grid().scroll_bottom;
        if at_bottom {
            let full_screen = self.grid().scroll_top == 0 && self.grid().scroll_bottom == self.grid().rows - 1;
            let dropped = self.grid_mut().scroll_up(1);
            if full_screen && !self.using_alternate {
                for (line, wrapped) in dropped {
                    self.scrollback.push_back(line);
                    self.scrollback_wrapped.push_back(wrapped);
                    if self.scrollback.len() > self.scrollback_limit {
                        self.scrollback.pop_front();
                        self.scrollback_wrapped.pop_front();
                    }
                }
            }
        } else {
            let g = self.grid_mut();
            if g.cursor_row + 1 < g.rows {
                g.cursor_row += 1;
            }
        }
        self.grid_mut().pending_wrap = false;
    }

    fn reverse_index(&mut self) {
        let at_top = self.grid().cursor_row == self.grid().scroll_top;
        if at_top {
            self.grid_mut().scroll_down(1);
        } else {
            let g = self.grid_mut();
            if g.cursor_row > 0 {
                g.cursor_row -= 1;
            }
        }
        self.grid_mut().pending_wrap = false;
    }

    fn feed_escape(&mut self, b: u8) {
        match b {
            b'[' => {
                self.csi_raw.clear();
                self.state = ParserState::CsiParam;
            }
            b']' => {
                self.osc_prev_was_esc = false;
                self.state = ParserState::OscString;
            }
            b'P' | b'X' | b'^' | b'_' => {
                self.dcs_prev_was_esc = false;
                self.state = ParserState::DcsSkip;
            }
            b'(' => self.state = ParserState::ChangeG0,
            b')' => self.state = ParserState::ChangeG1,
            b'7' => {
                let (r, c) = (self.grid().cursor_row, self.grid().cursor_col);
                self.grid_mut().saved_cursor = (r, c);
                self.state = ParserState::Ground;
            }
            b'8' => {
                let (r, c) = self.grid().saved_cursor;
                let g = self.grid_mut();
                g.cursor_row = r.min(g.rows - 1);
                g.cursor_col = c.min(g.cols - 1);
                self.state = ParserState::Ground;
            }
            b'c' => {
                self.reset_full();
                self.state = ParserState::Ground;
            }
            b'M' => {
                self.reverse_index();
                self.state = ParserState::Ground;
            }
            b'D' => {
                self.line_feed();
                self.state = ParserState::Ground;
            }
            b'E' => {
                self.line_feed();
                self.carriage_return();
                self.state = ParserState::Ground;
            }
            _ => self.state = ParserState::Ground,
        }
    }

    fn reset_full(&mut self) {
        let (rows, cols) = (self.primary.rows, self.primary.cols);
        self.primary = Grid::new(rows, cols);
        self.alternate = Grid::new(rows, cols);
        self.using_alternate = false;
        self.reset_attrs();
        self.cursor_visible = true;
        self.autowrap = true;
        self.g0 = Charset::Ascii;
        self.g1 = Charset::Ascii;
        self.shifted_out = false;
    }

    fn feed_csi(&mut self, b: u8) {
        match b {
            0x30..=0x3F => self.csi_raw.push(b as char),
            0x40..=0x7E => {
                self.dispatch_csi(b);
                self.state = ParserState::Ground;
            }
            0x20..=0x2F => {
                // Intermediate bytes (rare in the sequences we support) --
                // keep them so a final byte still terminates the sequence,
                // but they don't affect dispatch here.
            }
            _ => {}
        }
    }

    // ECMA-48 reserves '<', '=', '>' and '?' (0x3C-0x3F) as CSI parameter
    // bytes for private/experimental use -- xterm's own ctlseqs.txt uses
    // all four this way (DEC private modes lead with '?'; SGR mouse
    // reports, among others, lead with '<'). Treating only '?' as
    // "private" used to mean a sequence like an SGR mouse report
    // ("ESC[<0;5;3M", which this terminal itself only ever *emits*, never
    // parses as input -- but a job's own stdout can still contain one,
    // e.g. echoing back raw bytes it read from its stdin) fell through
    // to dispatch_csi's public-sequence match instead, where 'M' happens
    // to mean "delete N lines" (DL) -- corrupting the grid instead of
    // being safely ignored the way an unrecognized private sequence
    // already is (dispatch_private_mode's own final-byte match no-ops
    // anything that isn't 'h'/'l'). Recognizing all four leaders as
    // private closes that off in general, not just for this one case.
    fn parse_params(&self) -> (bool, Vec<i64>) {
        let private = self.csi_raw.starts_with(['<', '=', '>', '?']);
        let rest = if private { &self.csi_raw[1..] } else { &self.csi_raw[..] };
        let params: Vec<i64> = rest.split(';').map(|p| p.parse::<i64>().unwrap_or(0)).collect();
        (private, params)
    }

    fn dispatch_csi(&mut self, final_byte: u8) {
        let (private, params) = self.parse_params();
        let param_at = |idx: usize, default: i64| -> i64 {
            let v = params.get(idx).copied().unwrap_or(0);
            if v == 0 { default } else { v }
        };

        if private {
            self.dispatch_private_mode(final_byte, &params);
            return;
        }

        match final_byte {
            b'A' => self.move_cursor(-param_at(0, 1), 0),
            b'B' => self.move_cursor(param_at(0, 1), 0),
            b'C' => self.move_cursor(0, param_at(0, 1)),
            b'D' => self.move_cursor(0, -param_at(0, 1)),
            b'G' => {
                let col = (param_at(0, 1) - 1).max(0) as usize;
                let g = self.grid_mut();
                g.cursor_col = col.min(g.cols - 1);
                g.pending_wrap = false;
            }
            b'd' => {
                let row = (param_at(0, 1) - 1).max(0) as usize;
                let g = self.grid_mut();
                g.cursor_row = row.min(g.rows - 1);
                g.pending_wrap = false;
            }
            b'H' | b'f' => {
                let row = (param_at(0, 1) - 1).max(0) as usize;
                let col = (param_at(1, 1) - 1).max(0) as usize;
                let g = self.grid_mut();
                g.cursor_row = row.min(g.rows - 1);
                g.cursor_col = col.min(g.cols - 1);
                g.pending_wrap = false;
            }
            b'J' => self.erase_in_display(params.first().copied().unwrap_or(0)),
            b'K' => self.erase_in_line(params.first().copied().unwrap_or(0)),
            b'L' => self.grid_mut().insert_lines(param_at(0, 1) as usize),
            b'M' => self.grid_mut().delete_lines(param_at(0, 1) as usize),
            b'@' => self.grid_mut().insert_chars(param_at(0, 1) as usize),
            b'P' => self.grid_mut().delete_chars(param_at(0, 1) as usize),
            b'X' => self.grid_mut().erase_chars(param_at(0, 1) as usize),
            b'S' => {
                let dropped = self.grid_mut().scroll_up(param_at(0, 1) as usize);
                let full_screen = self.grid().scroll_top == 0 && self.grid().scroll_bottom == self.grid().rows - 1;
                if full_screen && !self.using_alternate {
                    for (line, wrapped) in dropped {
                        self.scrollback.push_back(line);
                        self.scrollback_wrapped.push_back(wrapped);
                        if self.scrollback.len() > self.scrollback_limit {
                            self.scrollback.pop_front();
                            self.scrollback_wrapped.pop_front();
                        }
                    }
                }
            }
            b'T' => self.grid_mut().scroll_down(param_at(0, 1) as usize),
            b'r' => {
                let rows = self.grid().rows;
                let top = (param_at(0, 1) - 1).max(0) as usize;
                let bottom = if params.len() > 1 && params[1] != 0 { (params[1] - 1).max(0) as usize } else { rows - 1 };
                let g = self.grid_mut();
                if top < bottom && bottom < rows {
                    g.scroll_top = top;
                    g.scroll_bottom = bottom;
                } else {
                    g.scroll_top = 0;
                    g.scroll_bottom = rows - 1;
                }
                g.cursor_row = g.scroll_top;
                g.cursor_col = 0;
                g.pending_wrap = false;
            }
            b's' => {
                let (r, c) = (self.grid().cursor_row, self.grid().cursor_col);
                self.grid_mut().saved_cursor = (r, c);
            }
            b'u' => {
                let (r, c) = self.grid().saved_cursor;
                let g = self.grid_mut();
                g.cursor_row = r.min(g.rows - 1);
                g.cursor_col = c.min(g.cols - 1);
            }
            b'm' => self.apply_sgr(&params),
            _ => {} // unsupported final byte: ignore, don't corrupt the grid
        }
    }

    fn move_cursor(&mut self, drow: i64, dcol: i64) {
        let g = self.grid_mut();
        let row = (g.cursor_row as i64 + drow).clamp(0, g.rows as i64 - 1) as usize;
        let col = (g.cursor_col as i64 + dcol).clamp(0, g.cols as i64 - 1) as usize;
        g.cursor_row = row;
        g.cursor_col = col;
        g.pending_wrap = false;
    }

    fn erase_in_display(&mut self, mode: i64) {
        let (row, col, rows, cols) = {
            let g = self.grid();
            (g.cursor_row, g.cursor_col, g.rows, g.cols)
        };
        match mode {
            0 => {
                self.grid_mut().clear_row_range(row, col, cols - 1);
                for r in (row + 1)..rows {
                    self.grid_mut().clear_row_range(r, 0, cols - 1);
                }
            }
            1 => {
                for r in 0..row {
                    self.grid_mut().clear_row_range(r, 0, cols - 1);
                }
                self.grid_mut().clear_row_range(row, 0, col);
            }
            2 | 3 => {
                self.grid_mut().clear_all();
                if mode == 3 {
                    self.scrollback.clear();
                    self.scrollback_wrapped.clear();
                }
            }
            _ => {}
        }
    }

    fn erase_in_line(&mut self, mode: i64) {
        let (row, col, cols) = {
            let g = self.grid();
            (g.cursor_row, g.cursor_col, g.cols)
        };
        match mode {
            0 => self.grid_mut().clear_row_range(row, col, cols - 1),
            1 => self.grid_mut().clear_row_range(row, 0, col),
            2 => self.grid_mut().clear_row_range(row, 0, cols - 1),
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &[i64]) {
        if params.is_empty() {
            self.reset_attrs();
            return;
        }
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => self.reset_attrs(),
                1 => self.cur_attrs.bold = true,
                2 => self.cur_attrs.dim = true,
                3 => self.cur_attrs.italic = true,
                4 => self.cur_attrs.underline = true,
                7 => self.cur_attrs.reverse = true,
                9 => self.cur_attrs.strikethrough = true,
                21 | 22 => {
                    self.cur_attrs.bold = false;
                    self.cur_attrs.dim = false;
                }
                23 => self.cur_attrs.italic = false,
                24 => self.cur_attrs.underline = false,
                27 => self.cur_attrs.reverse = false,
                29 => self.cur_attrs.strikethrough = false,
                30..=37 => self.cur_fg = Color::Indexed((params[i] - 30) as u8),
                39 => self.cur_fg = Color::Default,
                40..=47 => self.cur_bg = Color::Indexed((params[i] - 40) as u8),
                49 => self.cur_bg = Color::Default,
                90..=97 => self.cur_fg = Color::Indexed((params[i] - 90 + 8) as u8),
                100..=107 => self.cur_bg = Color::Indexed((params[i] - 100 + 8) as u8),
                38 | 48 => {
                    let target_fg = params[i] == 38;
                    match params.get(i + 1) {
                        Some(5) => {
                            let idx = params.get(i + 2).copied().unwrap_or(0) as u8;
                            if target_fg {
                                self.cur_fg = Color::Indexed(idx);
                            } else {
                                self.cur_bg = Color::Indexed(idx);
                            }
                            i += 2;
                        }
                        Some(2) => {
                            let r = params.get(i + 2).copied().unwrap_or(0) as u8;
                            let g = params.get(i + 3).copied().unwrap_or(0) as u8;
                            let b = params.get(i + 4).copied().unwrap_or(0) as u8;
                            if target_fg {
                                self.cur_fg = Color::Rgb(r, g, b);
                            } else {
                                self.cur_bg = Color::Rgb(r, g, b);
                            }
                            i += 4;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn dispatch_private_mode(&mut self, final_byte: u8, params: &[i64]) {
        let set = match final_byte {
            b'h' => true,
            b'l' => false,
            _ => return,
        };
        for &mode in params {
            match mode {
                1 => self.app_cursor_keys = set,
                7 => self.autowrap = set,
                25 => self.cursor_visible = set,
                1000 | 1002 | 1003 | 1006 => self.mouse_reporting = set,
                2004 => self.bracketed_paste = set,
                47 | 1047 => self.switch_alt_screen(set, false),
                1049 => self.switch_alt_screen(set, true),
                _ => {} // unsupported private mode: tracked nowhere, ignored
            }
        }
    }

    fn switch_alt_screen(&mut self, enable: bool, save_cursor: bool) {
        if enable == self.using_alternate {
            return;
        }
        if enable {
            if save_cursor {
                let (r, c) = (self.primary.cursor_row, self.primary.cursor_col);
                self.primary.saved_cursor = (r, c);
            }
            self.using_alternate = true;
            self.alternate.clear_all();
            self.alternate.cursor_row = 0;
            self.alternate.cursor_col = 0;
            self.alternate.pending_wrap = false;
        } else {
            self.using_alternate = false;
            if save_cursor {
                let (r, c) = self.primary.saved_cursor;
                self.primary.cursor_row = r.min(self.primary.rows - 1);
                self.primary.cursor_col = c.min(self.primary.cols - 1);
            }
        }
    }

    fn feed_osc(&mut self, b: u8) {
        if b == 0x07 {
            self.state = ParserState::Ground;
            return;
        }
        if self.osc_prev_was_esc && b == b'\\' {
            self.state = ParserState::Ground;
            return;
        }
        self.osc_prev_was_esc = b == 0x1B;
    }

    fn feed_dcs(&mut self, b: u8) {
        if b == 0x07 {
            self.state = ParserState::Ground;
            return;
        }
        if self.dcs_prev_was_esc && b == b'\\' {
            self.state = ParserState::Ground;
            return;
        }
        self.dcs_prev_was_esc = b == 0x1B;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_row(s: &Screen, row: usize) -> String {
        (0..s.grid().cols).map(|c| s.cell(row, c).ch).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn plain_text_and_wrap() {
        let mut s = Screen::new(3, 5);
        s.feed(b"hello");
        assert_eq!(text_row(&s, 0), "hello");
        assert_eq!(s.cursor(), (0, 4));
        s.feed(b"X"); // triggers pending wrap, prints on next line
        assert_eq!(text_row(&s, 0), "hello");
        assert_eq!(text_row(&s, 1), "X");
    }

    #[test]
    fn row_wraps_marks_autowrap_but_not_an_explicit_newline() {
        let mut s = Screen::new(3, 5);
        // Fills row 0 exactly, then a further printable byte forces autowrap.
        s.feed(b"helloX");
        assert!(s.row_wraps(0));
        assert!(!s.row_wraps(1));

        // A row filled exactly and then terminated with a real newline
        // never actually wrapped -- CR clears pending_wrap before it can
        // fire.
        let mut s2 = Screen::new(3, 5);
        s2.feed(b"world\r\nY");
        assert!(!s2.row_wraps(0));
        assert_eq!(text_row(&s2, 1), "Y");
    }

    #[test]
    fn row_wraps_survives_scrolling_into_scrollback() {
        let mut s = Screen::new(2, 5);
        // Row 0: "aaaaa" autowraps into row 1 ("bb"), then an explicit
        // newline pushes row 0 off into scrollback.
        s.feed(b"aaaaabb\r\nccc");
        assert_eq!(s.scrollback.len(), 1);
        assert!(s.scrollback_wrapped[0]);
        let sb: String = s.scrollback[0].iter().map(|c| c.ch).collect::<String>().trim_end().to_string();
        assert_eq!(sb, "aaaaa");
    }

    #[test]
    fn mouse_reporting_tracks_the_1000_1002_1003_1006_decset_group() {
        let mut s = Screen::new(5, 10);
        s.feed(b"\x1b[?1002h\x1b[?1006h");
        assert!(s.mouse_reporting);
        s.feed(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l");
        assert!(!s.mouse_reporting);
    }

    #[test]
    fn cursor_positioning_csi_h() {
        let mut s = Screen::new(5, 10);
        s.feed(b"\x1b[3;4Hy");
        assert_eq!(s.cell(2, 3).ch, 'y');
        assert_eq!(s.cursor(), (2, 4)); // cursor advances past the printed char
    }

    #[test]
    fn erase_in_line_and_display() {
        let mut s = Screen::new(2, 5);
        s.feed(b"abcde\x1b[2;1Hfghij");
        s.feed(b"\x1b[1;3H"); // row1 col3
        s.feed(b"\x1b[K"); // erase to end of line
        assert_eq!(text_row(&s, 0), "ab");
        s.feed(b"\x1b[2J");
        assert_eq!(text_row(&s, 1), "");
    }

    #[test]
    fn sgr_colors_basic_and_truecolor() {
        let mut s = Screen::new(1, 5);
        s.feed(b"\x1b[31;1mR\x1b[0mN\x1b[38;2;10;20;30mT");
        assert_eq!(s.cell(0, 0).fg, Color::Indexed(1));
        assert!(s.cell(0, 0).attrs.bold);
        assert_eq!(s.cell(0, 1).fg, Color::Default);
        assert_eq!(s.cell(0, 2).fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn scroll_region_and_scrollback() {
        let mut s = Screen::new(3, 4);
        s.feed(b"one\r\ntwo\r\nthr");
        assert_eq!(text_row(&s, 0), "one");
        s.feed(b"\r\nfour"); // scrolls the whole (full) screen up by one
        assert_eq!(text_row(&s, 0), "two");
        assert_eq!(text_row(&s, 1), "thr");
        assert_eq!(text_row(&s, 2), "four");
        assert_eq!(s.scrollback.len(), 1);
        let sb: String = s.scrollback[0].iter().map(|c| c.ch).collect::<String>().trim_end().to_string();
        assert_eq!(sb, "one");
    }

    #[test]
    fn shrinking_rows_keeps_the_bottom_of_the_grid_not_the_top() {
        // Fills every row (no scrolling yet -- exactly "a terminal
        // that's already full of content"), then shrinks height by one.
        // A naive top-anchored resize would keep "one"/"two" and drop
        // "three" -- the row the live cursor is actually sitting on.
        let mut s = Screen::new(3, 6);
        s.feed(b"one\r\ntwo\r\nthree");
        s.resize(2, 6);
        assert_eq!(text_row(&s, 0), "two");
        assert_eq!(text_row(&s, 1), "three");
    }

    #[test]
    fn shrinking_rows_pushes_the_dropped_top_rows_into_scrollback() {
        let mut s = Screen::new(3, 6);
        s.feed(b"one\r\ntwo\r\nthree");
        s.resize(2, 6);
        assert_eq!(s.scrollback.len(), 1);
        let sb: String = s.scrollback[0].iter().map(|c| c.ch).collect::<String>().trim_end().to_string();
        assert_eq!(sb, "one");
    }

    #[test]
    fn shrinking_rows_keeps_the_cursor_on_its_own_live_row() {
        let mut s = Screen::new(3, 6);
        s.feed(b"one\r\ntwo\r\nthree");
        assert_eq!(s.cursor(), (2, 5));
        s.resize(2, 6);
        // The cursor's own row (2, "three") shifted down to row 1 along
        // with its content -- not clamped to whatever row 1 used to
        // hold ("two", the old middle row).
        assert_eq!(s.cursor(), (1, 5));
    }

    #[test]
    fn growing_rows_stays_top_anchored_when_there_is_no_history_to_restore() {
        let mut s = Screen::new(2, 6);
        s.feed(b"one\r\ntwo");
        s.resize(4, 6);
        assert_eq!(text_row(&s, 0), "one");
        assert_eq!(text_row(&s, 1), "two");
        assert!(s.scrollback.is_empty());
    }

    #[test]
    fn growing_rows_pulls_content_back_out_of_scrollback() {
        let mut s = Screen::new(3, 6);
        s.feed(b"one\r\ntwo\r\nthree");
        s.resize(2, 6);
        assert_eq!(s.scrollback.len(), 1, "the shrink pushed \"one\" away");
        s.resize(3, 6);
        assert!(s.scrollback.is_empty(), "growing took it back");
        assert_eq!(text_row(&s, 0), "one");
        assert_eq!(text_row(&s, 1), "two");
        assert_eq!(text_row(&s, 2), "three");
        assert_eq!(s.cursor(), (2, 5), "and the cursor rode back down with its own row");
    }

    #[test]
    fn shrinking_spends_spare_blank_rows_below_the_cursor_before_touching_history() {
        // A pane holding a couple of prompts and acres of blank space --
        // what a shell pane looks like most of the time, and what `:diag`
        // resizes twice in a row. Nothing here is history yet, so a
        // shrink has no business pushing any of it away.
        let mut s = Screen::new(6, 6);
        s.feed(b"one\r\ntwo\r\n");
        assert_eq!(s.cursor(), (2, 0));
        s.resize(3, 6);
        assert!(s.scrollback.is_empty(), "the three blank rows below the cursor were the ones to give up");
        assert_eq!(text_row(&s, 0), "one");
        assert_eq!(text_row(&s, 1), "two");
        assert_eq!(s.cursor(), (2, 0), "and the cursor never moved");
    }

    #[test]
    fn a_shrink_and_a_regrow_leave_a_partly_filled_grid_exactly_as_it_was() {
        // The `:diag` round trip, reduced: expanding the pane shrinks
        // this grid, collapsing it grows it back, and the prompt has to
        // still be on the row it was on -- otherwise the next one draws
        // somewhere else and the old one stays behind as a ghost.
        let mut s = Screen::new(8, 6);
        s.feed(b"$ e f\r\n");
        let before: Vec<String> = (0..8).map(|r| text_row(&s, r)).collect();
        let cursor = s.cursor();
        s.resize(4, 6);
        s.resize(8, 6);
        assert_eq!((0..8).map(|r| text_row(&s, r)).collect::<Vec<_>>(), before);
        assert_eq!(s.cursor(), cursor);
    }

    #[test]
    fn a_full_grid_still_round_trips_through_scrollback() {
        // No spare rows below the cursor at all, so the shrink genuinely
        // has to spend history -- and the regrow has to hand it back.
        let mut s = Screen::new(3, 6);
        s.feed(b"one\r\ntwo\r\nthree");
        let before: Vec<String> = (0..3).map(|r| text_row(&s, r)).collect();
        let cursor = s.cursor();
        s.resize(2, 6);
        s.resize(3, 6);
        assert_eq!((0..3).map(|r| text_row(&s, r)).collect::<Vec<_>>(), before);
        assert_eq!(s.cursor(), cursor);
    }

    #[test]
    fn resizing_the_alternate_screen_never_pushes_its_own_content_into_scrollback() {
        let mut s = Screen::new(3, 6);
        s.feed(b"\x1b[?1049h"); // enter the alternate screen
        s.feed(b"aaa\r\nbbb\r\nccc");
        assert!(s.using_alternate);
        s.resize(2, 6);
        // Shrinking still drops (empty) primary rows into scrollback --
        // that's expected and harmless (primary.rows unconditionally
        // shrinks, see Screen::resize's own doc comment) -- what matters
        // is that none of the *alternate* screen's own "aaa"/"bbb"/"ccc"
        // content ever ends up there.
        for line in &s.scrollback {
            let text: String = line.iter().map(|c| c.ch).collect();
            assert!(text.trim().is_empty(), "alternate screen content leaked into scrollback: {text:?}");
        }
    }

    #[test]
    fn an_echoed_sgr_mouse_report_does_not_delete_a_line() {
        // A job's own stdout can legitimately contain an SGR mouse report
        // (e.g. echoing back raw bytes it read from its stdin) even
        // though this terminal only ever *emits* one, never receives one
        // as real input -- 'M' is also the public (non-private) final
        // byte for DL (delete N lines), so this would previously wipe
        // the second row instead of being safely ignored the way any
        // other unrecognized private-marked sequence already is.
        let mut s = Screen::new(3, 20);
        s.feed(b"row0\r\nrow1\r\nrow2");
        s.feed(b"\x1b[<0;5;3M");
        assert_eq!(text_row(&s, 0), "row0");
        assert_eq!(text_row(&s, 1), "row1");
        assert_eq!(text_row(&s, 2), "row2");
    }

    #[test]
    fn alt_screen_switch_and_restore() {
        let mut s = Screen::new(2, 10);
        s.feed(b"primary");
        s.feed(b"\x1b[?1049h");
        assert!(s.using_alternate);
        assert_eq!(text_row(&s, 0), "");
        s.feed(b"altscr");
        s.feed(b"\x1b[?1049l");
        assert!(!s.using_alternate);
        assert_eq!(text_row(&s, 0), "primary");
    }

    #[test]
    fn dec_line_drawing_charset() {
        let mut s = Screen::new(1, 3);
        s.feed(b"\x1b(0qqq\x1b(B");
        assert_eq!(s.cell(0, 0).ch, '\u{2500}');
    }

    #[test]
    fn osc_sequence_is_skipped_without_corrupting_grid() {
        let mut s = Screen::new(1, 10);
        s.feed(b"\x1b]0;window title\x07hi");
        assert_eq!(text_row(&s, 0), "hi");
    }

    #[test]
    fn insert_and_delete_chars() {
        let mut s = Screen::new(1, 6);
        s.feed(b"abcdef\x1b[1;2H\x1b[2P"); // delete 2 chars at col 2 (0-idx 1)
        assert_eq!(text_row(&s, 0), "adef");
        s.feed(b"\x1b[1;2H\x1b[2@"); // insert 2 blanks at col 2
        assert_eq!(s.cell(0, 1).ch, ' ');
        assert_eq!(s.cell(0, 2).ch, ' ');
        assert_eq!(s.cell(0, 3).ch, 'd');
    }

    #[test]
    fn cursor_visibility_mode() {
        let mut s = Screen::new(1, 5);
        assert!(s.cursor_visible);
        s.feed(b"\x1b[?25l");
        assert!(!s.cursor_visible);
        s.feed(b"\x1b[?25h");
        assert!(s.cursor_visible);
    }

    #[test]
    fn utf8_multibyte_char() {
        let mut s = Screen::new(1, 5);
        s.feed("héllo".as_bytes());
        assert_eq!(s.cell(0, 1).ch, 'é');
    }

    // Regression: cell/cell_mut used to index self.cells[row * cols +
    // col] with no bounds check at all, which real callers could (and,
    // in production, did -- see repl.rs's TerminalFrame::capture) hit
    // with a stale row/col from a size snapshot that raced a real
    // resize. Reading/writing out of bounds must clamp to the nearest
    // real cell instead of panicking.
    #[test]
    fn cell_out_of_bounds_clamps_instead_of_panicking() {
        let s = Screen::new(2, 3);
        // Same cell as the last real one (1, 2) -- proves this is a
        // real clamp, not a fluke default value.
        assert_eq!(s.cell(50, 50), s.cell(1, 2));
        assert_eq!(s.cell(0, 50).ch, s.cell(0, 2).ch);
        assert_eq!(s.cell(50, 0).ch, s.cell(1, 0).ch);
    }

    #[test]
    fn row_wraps_out_of_bounds_clamps_instead_of_panicking() {
        let s = Screen::new(2, 3);
        assert_eq!(s.row_wraps(50), s.row_wraps(1));
    }
}
