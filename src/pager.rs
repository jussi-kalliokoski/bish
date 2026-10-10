// A scrollable read-only view over styled lines, for `:help` and
// `:preview`: state, layout and rendering, testable without a terminal,
// with repl.rs driving it, as for browser.rs and the diagnostics pane.
//
// Styled lines rather than a read-only `TextBuffer` in a `Frame::Edit`:
// a `TextBuffer` holds plain characters coloured by language, so a
// rendered document's colours, box drawing and weight would show as
// literal escapes. The price is a small navigation vocabulary of its
// own.

use crate::bishedit::unicode_width::char_width;
use crate::window::Rect;

pub struct Pager {
    // What the title bar says: `:help`, or a previewed file's name.
    title: String,
    lines: Vec<String>,
    // First visible line.
    top: usize,
    // The first visible column, for a document wider than the pane, as a
    // table is with `table_wrap` off. Prose is wrapped to fit, so this is
    // usually 0.
    left: usize,
    // The widest line's display width, measured once, bounding `left`
    // without scanning the document per key.
    widest: usize,
    rows: usize,
    cols: usize,
    // The last search and its matches, so `n`/`N` step without rescanning.
    query: String,
    matches: Vec<usize>,
    current_match: Option<usize>,
    searching: bool,
    // Shown in the status bar until the next keystroke.
    message: Option<String>,
}

// What one keystroke did, from the driving loop's point of view.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Continue,
    Quit,
    /// A left click this view has no use for, reported so the host can
    /// hit-test it against the tab bar and other panes.
    Click(crate::editor::MouseEvent),
}

// How far one sideways press moves: a column is too slow across a
// table, a screen loses the columns being compared; eight is about a
// cell.
const SIDE_STEP: usize = 8;

impl Pager {
    pub fn new(title: &str, lines: Vec<String>, rows: usize, cols: usize) -> Pager {
        let widest = lines.iter().map(|l| strip_sgr(l).chars().map(char_width).sum::<usize>()).max().unwrap_or(0);
        Pager {
            title: title.to_string(),
            lines,
            top: 0,
            left: 0,
            widest,
            // One row for the title, one for the status bar.
            rows: rows.saturating_sub(2).max(1),
            cols,
            query: String::new(),
            matches: Vec::new(),
            current_match: None,
            searching: false,
            message: None,
        }
    }

    // The top line, so a re-render at a new width can come back near it.
    pub fn top_line(&self) -> usize {
        self.top
    }

    // Clamped: the remembered line may be past the end of a re-wrapped
    // document.
    pub fn scroll_to(&mut self, line: usize) {
        self.top = line;
        self.clamp();
    }

    // The left column, so a re-render at a new width can come back to it.
    pub fn left_column(&self) -> usize {
        self.left
    }

    pub fn scroll_columns_to(&mut self, column: usize) {
        self.left = column.min(self.max_left());
    }

    // The largest `top` that still shows content: scrolling stops with the
    // last line on screen.
    fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.rows)
    }

    // Never past the last column with anything on it, or the reader would
    // face a blank pane with no clue which way is back.
    fn max_left(&self) -> usize {
        self.widest.saturating_sub(self.cols)
    }

    fn clamp(&mut self) {
        self.top = self.top.min(self.max_top());
        self.left = self.left.min(self.max_left());
    }

    fn scroll_by(&mut self, delta: isize) {
        let next = self.top as isize + delta;
        self.top = next.clamp(0, self.max_top() as isize) as usize;
    }

    fn scroll_sideways_by(&mut self, delta: isize) {
        let next = self.left as isize + delta;
        self.left = next.clamp(0, self.max_left() as isize) as usize;
    }

    // Puts `line` on screen, about a third from the top if not already
    // visible: context above a search hit.
    fn reveal(&mut self, line: usize) {
        if line >= self.top && line < self.top + self.rows {
            return;
        }
        self.top = line.saturating_sub(self.rows / 3).min(self.max_top());
    }

    pub fn handle_key(&mut self, key: crate::editor::Key) -> Outcome {
        use crate::editor::Key;
        self.message = None;
        if self.searching {
            match key {
                Key::Char(c) => {
                    self.query.push(c);
                    return Outcome::Continue;
                }
                Key::Backspace => {
                    if self.query.pop().is_none() {
                        self.searching = false;
                    }
                    return Outcome::Continue;
                }
                Key::Escape | Key::CtrlC => {
                    self.searching = false;
                    self.query.clear();
                    return Outcome::Continue;
                }
                Key::Enter => {
                    self.searching = false;
                    self.run_search();
                    return Outcome::Continue;
                }
                _ => return Outcome::Continue,
            }
        }
        match key {
            Key::Char('q') | Key::Escape | Key::CtrlC => Outcome::Quit,
            Key::Char('j') | Key::Down | Key::CtrlN => {
                self.scroll_by(1);
                Outcome::Continue
            }
            Key::Char('k') | Key::Up | Key::CtrlP => {
                self.scroll_by(-1);
                Outcome::Continue
            }
            // Half a screen, a whole screen: less(1)'s pair, and vim's.
            Key::CtrlD => {
                self.scroll_by(self.rows as isize / 2);
                Outcome::Continue
            }
            Key::CtrlU => {
                self.scroll_by(-(self.rows as isize / 2));
                Outcome::Continue
            }
            Key::Char(' ') | Key::CtrlF | Key::PageDown => {
                self.scroll_by(self.rows as isize);
                Outcome::Continue
            }
            Key::CtrlB | Key::PageUp => {
                self.scroll_by(-(self.rows as isize));
                Outcome::Continue
            }
            Key::Char('l') | Key::Right => {
                self.scroll_sideways_by(SIDE_STEP as isize);
                Outcome::Continue
            }
            Key::Char('h') | Key::Left => {
                self.scroll_sideways_by(-(SIDE_STEP as isize));
                Outcome::Continue
            }
            // Back to the left edge, vim's `0`. No `$`: the widest line's
            // right edge isn't where any particular line ends.
            Key::Char('0') => {
                self.left = 0;
                Outcome::Continue
            }
            Key::Char('g') | Key::Home => {
                self.top = 0;
                Outcome::Continue
            }
            Key::Char('G') | Key::End => {
                self.top = self.max_top();
                Outcome::Continue
            }
            Key::Char('/') => {
                self.searching = true;
                self.query.clear();
                Outcome::Continue
            }
            Key::Char('n') => {
                self.step_match(1);
                Outcome::Continue
            }
            Key::Char('N') => {
                self.step_match(-1);
                Outcome::Continue
            }
            Key::Mouse(ev) if ev.is_left_click() => Outcome::Click(ev),
            Key::Mouse(ev) => {
                // The editor's and Normal mode's wheel step, from the one
                // constant.
                let step = crate::fileeditor::MOUSE_WHEEL_LINES as isize;
                if ev.is_scroll_down() {
                    self.scroll_by(step);
                } else if ev.is_scroll_up() {
                    self.scroll_by(-step);
                }
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    // Search is over the visible text: lines carry SGR sequences, and a
    // search for "table" means the word.
    fn run_search(&mut self) {
        self.matches.clear();
        self.current_match = None;
        if self.query.is_empty() {
            return;
        }
        let needle = self.query.to_lowercase();
        for (i, line) in self.lines.iter().enumerate() {
            if strip_sgr(line).to_lowercase().contains(&needle) {
                self.matches.push(i);
            }
        }
        if self.matches.is_empty() {
            self.message = Some(format!("no match for {:?}", self.query));
            return;
        }
        // From the first match at or after the top, so searching doesn't jump
        // backwards.
        let from = self.matches.iter().position(|&l| l >= self.top).unwrap_or(0);
        self.current_match = Some(from);
        let line = self.matches[from];
        self.reveal(line);
    }

    fn step_match(&mut self, delta: isize) {
        if self.matches.is_empty() {
            self.message =
                Some(if self.query.is_empty() { "no search yet -- press / to search".to_string() } else { format!("no match for {:?}", self.query) });
            return;
        }
        let len = self.matches.len() as isize;
        let next = match self.current_match {
            Some(i) => (i as isize + delta).rem_euclid(len),
            None => 0,
        };
        self.current_match = Some(next as usize);
        let line = self.matches[next as usize];
        self.reveal(line);
    }

    /// Renders into `rect`, one pane's rectangle, like browser.rs and the
    /// hex view, so other panes and the tab bar stay visible and a click can
    /// be hit-tested against them.
    pub fn render(&self, rect: Rect) -> String {
        let mut out = String::new();
        out.push_str("\x1b[?25l");
        let at = |row: usize| format!("\x1b[{};{}H", rect.row + row + 1, rect.col + 1);
        out.push_str(&at(0));
        out.push_str(&format!("\x1b[1;7m{}\x1b[0m", pad_to_width(&self.title, self.cols)));
        for row in 0..self.rows {
            out.push_str(&at(row + 1));
            let line = self.lines.get(self.top + row).map(String::as_str).unwrap_or("");
            out.push_str(&window_to_width(line, self.left, self.cols));
        }
        // The status row is this pane's last row, not the terminal's, which
        // differ with a split above or below.
        out.push_str(&at(rect.rows.saturating_sub(1)));
        out.push_str(&format!("\x1b[7m{}\x1b[0m", pad_to_width(&self.status(), self.cols)));
        out
    }

    fn status(&self) -> String {
        if self.searching {
            return format!("/{}", self.query);
        }
        if let Some(message) = &self.message {
            return message.clone();
        }
        let last = (self.top + self.rows).min(self.lines.len());
        let position = if self.lines.len() <= self.rows {
            "all".to_string()
        } else if self.top == 0 {
            "top".to_string()
        } else if self.top >= self.max_top() {
            "end".to_string()
        } else {
            format!("{}%", self.top * 100 / self.max_top().max(1))
        };
        let found = match (self.current_match, self.matches.len()) {
            (Some(i), n) if n > 0 => format!("  match {}/{}", i + 1, n),
            _ => String::new(),
        };
        // The sideways keys are mentioned only when there's something off to
        // the side, which prose never has.
        let keys = if self.max_left() > 0 { "j/k h/l scroll" } else { "j/k \u{2191}\u{2193} scroll" };
        format!("{}-{} of {}  {}{}   {keys}  / search  q quit", self.top + 1, last, self.lines.len(), position, found)
    }
}

// Pads or truncates to exactly `cols` columns from the left edge, for
// the title and status rows, which don't scroll.
fn pad_to_width(line: &str, cols: usize) -> String {
    window_to_width(line, 0, cols)
}

// The `cols` columns of `line` from column `left`, padded to that width,
// by drawn width, keeping every SGR sequence passed, including those of
// characters scrolled off: the style at the left edge is whatever they
// last set.
fn window_to_width(line: &str, left: usize, cols: usize) -> String {
    let mut out = String::new();
    // Where we are in the line and how much of the window is filled, equal
    // only when `left` is 0.
    let mut col = 0;
    let mut width = 0;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            out.push(c);
            out.push(chars.next().expect("just peeked"));
            for c2 in chars.by_ref() {
                out.push(c2);
                if c2 == 'm' {
                    break;
                }
            }
            continue;
        }
        let start = col;
        col += char_width(c);
        if col <= left {
            continue;
        }
        // A wide glyph straddling the left edge has no half to draw, so its
        // visible columns are blank.
        let (text, drawn) = if start < left { (" ".repeat(col - left), col - left) } else { (c.to_string(), col - start) };
        if width + drawn > cols {
            break;
        }
        width += drawn;
        out.push_str(&text);
    }
    // The reset: a line ending mid-style would paint the padding and the
    // row below in that style.
    out.push_str("\x1b[0m");
    if width < cols {
        out.push_str(&" ".repeat(cols - width));
    }
    out
}

fn strip_sgr(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c2 in chars.by_ref() {
                if c2 == 'm' {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Key;

    fn pager(count: usize, rows: usize) -> Pager {
        let lines: Vec<String> = (0..count).map(|i| format!("line {i}")).collect();
        Pager::new("test", lines, rows, 40)
    }

    #[test]
    fn scrolling_stops_with_the_last_line_on_screen() {
        // 10 lines, 12 terminal rows: 10 content rows.
        let mut p = pager(10, 12);
        assert_eq!(p.rows, 10);
        p.handle_key(Key::Char('G'));
        assert_eq!(p.top, 0, "everything already fits, so there is nowhere to go");

        let mut p = pager(100, 12);
        p.handle_key(Key::Char('G'));
        assert_eq!(p.top, 90, "the last line sits on the last row");
        p.handle_key(Key::Char('j'));
        assert_eq!(p.top, 90, "and stays there");
    }

    #[test]
    fn the_movement_vocabulary_is_the_one_a_pager_has() {
        let mut p = pager(100, 12);
        p.handle_key(Key::Char('j'));
        assert_eq!(p.top, 1);
        p.handle_key(Key::Char('k'));
        assert_eq!(p.top, 0);
        p.handle_key(Key::CtrlD);
        assert_eq!(p.top, 5, "half a screen");
        p.handle_key(Key::Char(' '));
        assert_eq!(p.top, 15, "a whole screen");
        p.handle_key(Key::CtrlB);
        assert_eq!(p.top, 5);
        p.handle_key(Key::Char('g'));
        assert_eq!(p.top, 0);
    }

    #[test]
    fn q_and_escape_quit_and_nothing_else_does() {
        assert_eq!(pager(10, 12).handle_key(Key::Char('q')), Outcome::Quit);
        assert_eq!(pager(10, 12).handle_key(Key::Escape), Outcome::Quit);
        assert_eq!(pager(10, 12).handle_key(Key::Char('j')), Outcome::Continue);
    }

    // Search is over what a line shows: a word split by an escape is still
    // found.
    #[test]
    fn search_ignores_the_styling_in_a_line() {
        let lines = vec!["plain".to_string(), "a \x1b[1mstyled\x1b[0m word".to_string()];
        let mut p = Pager::new("t", lines, 12, 40);
        for c in "styled".chars() {
            p.handle_key(Key::Char(c));
        }
        // Typed outside search mode, so it did nothing.
        assert!(p.matches.is_empty());
        p.handle_key(Key::Char('/'));
        for c in "styled".chars() {
            p.handle_key(Key::Char(c));
        }
        p.handle_key(Key::Enter);
        assert_eq!(p.matches, vec![1]);
    }

    #[test]
    fn n_steps_between_matches_and_wraps() {
        let lines: Vec<String> = (0..30).map(|i| if i % 10 == 0 { "hit".to_string() } else { "x".to_string() }).collect();
        let mut p = Pager::new("t", lines, 7, 40);
        p.handle_key(Key::Char('/'));
        for c in "hit".chars() {
            p.handle_key(Key::Char(c));
        }
        p.handle_key(Key::Enter);
        assert_eq!(p.matches, vec![0, 10, 20]);
        assert_eq!(p.current_match, Some(0));
        p.handle_key(Key::Char('n'));
        assert_eq!(p.current_match, Some(1));
        p.handle_key(Key::Char('n'));
        p.handle_key(Key::Char('n'));
        assert_eq!(p.current_match, Some(0), "wraps around");
        p.handle_key(Key::Char('N'));
        assert_eq!(p.current_match, Some(2), "and backwards too");
    }

    #[test]
    fn escape_leaves_the_search_input_before_it_leaves_the_pager() {
        let mut p = pager(10, 12);
        p.handle_key(Key::Char('/'));
        assert_eq!(p.handle_key(Key::Escape), Outcome::Continue);
        assert!(!p.searching);
        assert_eq!(p.handle_key(Key::Escape), Outcome::Quit);
    }

    // Every row is exactly the terminal's width, or what was there before
    // shows through.
    #[test]
    fn every_rendered_row_is_exactly_the_width() {
        let lines = vec!["short".to_string(), "\x1b[1mstyled\x1b[0m".to_string(), "\u{65e5}\u{672c}\u{8a9e} wide".to_string(), "x".repeat(200)];
        let p = Pager::new("title", lines, 10, 40);
        for row in p.render(Rect { row: 0, col: 0, rows: 10, cols: 80 }).split("\x1b[").filter(|s| s.ends_with('H') || s.contains('H')) {
            let _ = row;
        }
        // Measured directly: every row goes through `pad_to_width`.
        for line in ["short", "\x1b[1mstyled\x1b[0m", "\u{65e5}\u{672c}\u{8a9e} wide", &"x".repeat(200)] {
            assert_eq!(visible_width(&pad_to_width(line, 40)), 40, "{line:?}");
        }
    }

    fn visible_width(s: &str) -> usize {
        strip_sgr(s).chars().map(char_width).sum()
    }

    // With `table_wrap` off a table runs off the side, and the view can go
    // and get the rest.
    #[test]
    fn a_document_wider_than_the_pane_scrolls_sideways() {
        let wide: String = ('a'..='z').chain('A'..='Z').collect();
        let mut p = Pager::new("t", vec!["short".to_string(), wide.clone()], 6, 20);
        assert_eq!(visible_row(&p, 1), "abcdefghijklmnopqrst");

        p.handle_key(Key::Char('l'));
        assert_eq!(visible_row(&p, 1), "ijklmnopqrstuvwxyzAB", "one step is SIDE_STEP columns");
        // The line that ran out is blank, not repeating its last character.
        assert_eq!(visible_row(&p, 0), " ".repeat(20));

        p.handle_key(Key::Char('h'));
        assert_eq!(visible_row(&p, 1), "abcdefghijklmnopqrst");
        // `h` at the left edge stays there.
        p.handle_key(Key::Char('h'));
        assert_eq!(p.left_column(), 0);

        p.handle_key(Key::Char('l'));
        p.handle_key(Key::Char('0'));
        assert_eq!(p.left_column(), 0, "0 comes straight back to the left edge");
    }

    // Never into empty space.
    #[test]
    fn scrolling_sideways_stops_at_the_widest_line() {
        let mut p = Pager::new("t", vec!["x".repeat(30)], 6, 20);
        for _ in 0..20 {
            p.handle_key(Key::Right);
        }
        assert_eq!(p.left_column(), 10, "30 columns of line, 20 of pane");
        assert_eq!(visible_row(&p, 0), "x".repeat(20));
        // A document narrower than the pane has nowhere to go.
        let mut narrow = Pager::new("t", vec!["x".to_string()], 6, 20);
        narrow.handle_key(Key::Right);
        assert_eq!(narrow.left_column(), 0);
    }

    // Scrolled rows are still the pane's width, and the style of characters
    // now off to the left still applies: their escapes are kept.
    #[test]
    fn a_scrolled_row_keeps_its_width_and_its_styling() {
        let line = format!("\x1b[1m{}\x1b[0m", "x".repeat(30));
        assert_eq!(visible_width(&window_to_width(&line, 8, 20)), 20);
        assert!(window_to_width(&line, 8, 20).starts_with("\x1b[1m"), "the style survives the columns it was applied to");
        // A wide glyph halved by the left edge draws as its visible blank
        // columns.
        assert_eq!(strip_sgr(&window_to_width("\u{65e5}\u{672c}", 1, 4)), " \u{672c} ");
    }

    fn visible_row(p: &Pager, row: usize) -> String {
        strip_sgr(&window_to_width(&p.lines[p.top + row], p.left, p.cols))
    }

    // A resize re-renders the document (the wrap width and every table
    // changed), so the loop builds a fresh pager and returns it to where the
    // reader was, even if the document got shorter.
    #[test]
    fn restoring_a_position_into_a_shorter_document_stays_inside_it() {
        let mut p = pager(100, 12);
        p.handle_key(Key::Char('G'));
        assert_eq!(p.top_line(), 90);

        let mut taller = pager(100, 60);
        taller.scroll_to(90);
        assert_eq!(taller.top_line(), 42, "clamped to what the taller screen can still show");

        let mut shorter = pager(10, 12);
        shorter.scroll_to(90);
        assert_eq!(shorter.top_line(), 0, "a document that now fits has nowhere to scroll");
    }

    #[test]
    fn an_empty_document_renders_without_panicking() {
        let p = Pager::new("empty", Vec::new(), 10, 40);
        assert!(!p.render(Rect { row: 0, col: 0, rows: 10, cols: 80 }).is_empty());
    }
}
