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
            cursor_row: 0,
            cursor_col: 0,
            pending_wrap: false,
            saved_cursor: (0, 0),
            scroll_top: 0,
            scroll_bottom: rows - 1,
        }
    }

    fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let mut new_cells = vec![Cell::default(); rows * cols];
        for r in 0..self.rows.min(rows) {
            for c in 0..self.cols.min(cols) {
                new_cells[r * cols + c] = self.cells[r * self.cols + c];
            }
        }
        self.cells = new_cells;
        self.rows = rows;
        self.cols = cols;
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.pending_wrap = false;
    }

    pub fn cell(&self, row: usize, col: usize) -> Cell {
        self.cells[row * self.cols + col]
    }

    fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        &mut self.cells[row * self.cols + col]
    }

    fn clear_all(&mut self) {
        self.cells.iter_mut().for_each(|c| *c = Cell::default());
    }

    fn clear_row_range(&mut self, row: usize, from: usize, to_inclusive: usize) {
        for c in from..=to_inclusive.min(self.cols - 1) {
            *self.cell_mut(row, c) = Cell::default();
        }
    }

    // Scrolls the region [scroll_top, scroll_bottom] up by `n` lines,
    // dropping lines off the top; blank lines fill in at the bottom.
    // Returns the dropped lines (for scrollback capture), only meaningful
    // to the caller when the region spans the whole grid.
    fn scroll_up(&mut self, n: usize) -> Vec<Vec<Cell>> {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let region_h = bottom - top + 1;
        let n = n.min(region_h);
        let mut dropped = Vec::new();
        for _ in 0..n {
            let row: Vec<Cell> = (0..self.cols).map(|c| self.cell(top, c)).collect();
            dropped.push(row);
            for r in top..bottom {
                for c in 0..self.cols {
                    let below = self.cell(r + 1, c);
                    *self.cell_mut(r, c) = below;
                }
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
    scrollback_limit: usize,
    pub cursor_visible: bool,
    pub autowrap: bool,
    pub mouse_reporting: bool,
    pub bracketed_paste: bool,

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
            scrollback_limit: 5000,
            cursor_visible: true,
            autowrap: true,
            mouse_reporting: false,
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
        self.primary.resize(rows, cols);
        self.alternate.resize(rows, cols);
    }

    pub fn size(&self) -> (usize, usize) {
        (self.grid().rows, self.grid().cols)
    }

    fn grid(&self) -> &Grid {
        if self.using_alternate {
            &self.alternate
        } else {
            &self.primary
        }
    }

    fn grid_mut(&mut self) -> &mut Grid {
        if self.using_alternate {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    pub fn cell(&self, row: usize, col: usize) -> Cell {
        self.grid().cell(row, col)
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
            0x0E => self.shifted_out = true, // SO
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
                for line in dropped {
                    self.scrollback.push_back(line);
                    if self.scrollback.len() > self.scrollback_limit {
                        self.scrollback.pop_front();
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

    fn parse_params(&self) -> (bool, Vec<i64>) {
        let private = self.csi_raw.starts_with('?');
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
                    for line in dropped {
                        self.scrollback.push_back(line);
                        if self.scrollback.len() > self.scrollback_limit {
                            self.scrollback.pop_front();
                        }
                    }
                }
            }
            b'T' => self.grid_mut().scroll_down(param_at(0, 1) as usize),
            b'r' => {
                let rows = self.grid().rows;
                let top = (param_at(0, 1) - 1).max(0) as usize;
                let bottom = if params.len() > 1 && params[1] != 0 {
                    (params[1] - 1).max(0) as usize
                } else {
                    rows - 1
                };
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
}
