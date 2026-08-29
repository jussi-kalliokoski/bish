// A scrollable, read-only view over already-styled lines: what `:help`
// and `:preview` show. Its own module because it is a genuine view --
// state, layout and rendering, all testable without a terminal -- with
// repl.rs owning only the small driving loop, the same split browser.rs
// and the diagnostics pane already use.
//
// **Why pre-styled lines rather than a buffer.** The obvious way to make
// a document scrollable here would be a read-only `TextBuffer` in a
// `Frame::Edit`, which would bring vim motions and search along for
// free. It can't work: a `TextBuffer` holds plain characters and the
// editor colours it by *language*, so a rendered markdown document --
// whose whole point is that it already carries colour, box-drawing and
// weight -- would arrive as literal escape sequences on screen. So this
// takes the lines as rendered and scrolls them, and pays for that by
// re-deriving a small navigation vocabulary instead of inheriting one.
//
// It takes over the terminal for its own duration and hands control back
// -- the same "blocks until it exits" shape `:dbg`'s own run already has
// (see repl.rs's `dbg` arm), rather than a pane of its own. A document
// someone is reading wants the whole screen, and wants it to go away
// completely when they're done.

use crate::bishedit::unicode_width::char_width;

pub struct Pager {
    // What the title bar says this is: `:help`, or a previewed file's
    // own name.
    title: String,
    lines: Vec<String>,
    // First visible line.
    top: usize,
    rows: usize,
    cols: usize,
    // The last search and where the matches are, so `n`/`N` can step
    // between them without re-scanning.
    query: String,
    matches: Vec<usize>,
    current_match: Option<usize>,
    searching: bool,
    // Shown in the status bar until the next keystroke.
    message: Option<String>,
}

// What one keystroke did, from the driving loop's point of view.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Quit,
}

impl Pager {
    pub fn new(title: &str, lines: Vec<String>, rows: usize, cols: usize) -> Pager {
        Pager {
            title: title.to_string(),
            lines,
            top: 0,
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

    // Which line is at the top, so a re-render at a new width can come
    // back to roughly where the reader was.
    pub fn top_line(&self) -> usize {
        self.top
    }

    // Clamped, because the caller's remembered line may be past the end
    // of a document that has just been re-wrapped narrower or wider.
    pub fn scroll_to(&mut self, line: usize) {
        self.top = line;
        self.clamp();
    }

    // The largest `top` that still shows content: scrolling stops with
    // the last line on screen rather than running off into blank rows.
    fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.rows)
    }

    fn clamp(&mut self) {
        self.top = self.top.min(self.max_top());
    }

    fn scroll_by(&mut self, delta: isize) {
        let next = self.top as isize + delta;
        self.top = next.clamp(0, self.max_top() as isize) as usize;
    }

    // Puts `line` on screen, roughly a third from the top when it isn't
    // already visible -- enough context above a search hit to read it in.
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
            // Half a screen, whole screen: less(1)'s own pair, which is
            // also vim's.
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
            Key::Mouse(ev) => {
                if ev.is_scroll_down() {
                    self.scroll_by(3);
                } else if ev.is_scroll_up() {
                    self.scroll_by(-3);
                }
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    // Search is over the *visible* text: the lines carry SGR sequences,
    // and a reader looking for "table" means the word, not whatever
    // escape happens to sit inside it.
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
        // Start from the first match at or after what's on screen, so
        // searching doesn't jump backwards for no reason.
        let from = self.matches.iter().position(|&l| l >= self.top).unwrap_or(0);
        self.current_match = Some(from);
        let line = self.matches[from];
        self.reveal(line);
    }

    fn step_match(&mut self, delta: isize) {
        if self.matches.is_empty() {
            self.message = Some(if self.query.is_empty() {
                "no search yet -- press / to search".to_string()
            } else {
                format!("no match for {:?}", self.query)
            });
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

    // The whole screen: title, the visible slice, and a status bar.
    pub fn render(&self, term_rows: usize) -> String {
        let mut out = String::new();
        out.push_str("\x1b[?25l\x1b[H");
        out.push_str(&format!("\x1b[1;7m{}\x1b[0m", pad_to_width(&self.title, self.cols)));
        for row in 0..self.rows {
            out.push_str(&format!("\x1b[{};1H", row + 2));
            let line = self.lines.get(self.top + row).map(String::as_str).unwrap_or("");
            out.push_str(&pad_to_width(line, self.cols));
        }
        out.push_str(&format!("\x1b[{};1H", term_rows.max(2)));
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
        format!("{}-{} of {}  {}{}   j/k \u{2191}\u{2193} scroll  / search  q quit", self.top + 1, last, self.lines.len(), position, found)
    }
}

// Pads (or truncates) to exactly `cols` columns, measuring what the line
// draws rather than how many bytes it holds and keeping every SGR
// sequence it passes -- dropping one would leave the rest of the screen
// in whatever style happened to be active.
fn pad_to_width(line: &str, cols: usize) -> String {
    let mut out = String::new();
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
        let w = char_width(c);
        if width + w > cols {
            break;
        }
        width += w;
        out.push(c);
    }
    // The reset matters: a line that ended mid-style would otherwise
    // paint the padding, and the row below it, in that style.
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
        // 10 lines, 12 terminal rows -> 10 content rows.
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

    // Search is over what the line *shows*: the lines carry styling, and
    // a word split by an escape sequence still has to be findable.
    #[test]
    fn search_ignores_the_styling_in_a_line() {
        let lines = vec!["plain".to_string(), "a \x1b[1mstyled\x1b[0m word".to_string()];
        let mut p = Pager::new("t", lines, 12, 40);
        for c in "styled".chars() {
            p.handle_key(Key::Char(c));
        }
        // ...that was typed outside search mode, so it did nothing.
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

    // Every rendered row is exactly the terminal's width, styling or no
    // styling -- a row that came up short would leave whatever was on
    // screen before showing through.
    #[test]
    fn every_rendered_row_is_exactly_the_width() {
        let lines = vec![
            "short".to_string(),
            "\x1b[1mstyled\x1b[0m".to_string(),
            "\u{65e5}\u{672c}\u{8a9e} wide".to_string(),
            "x".repeat(200),
        ];
        let p = Pager::new("title", lines, 10, 40);
        for row in p.render(10).split("\x1b[").filter(|s| s.ends_with('H') || s.contains('H')) {
            let _ = row;
        }
        // Measured directly rather than by parsing the frame back out:
        // pad_to_width is what every row goes through.
        for line in ["short", "\x1b[1mstyled\x1b[0m", "\u{65e5}\u{672c}\u{8a9e} wide", &"x".repeat(200)] {
            assert_eq!(visible_width(&pad_to_width(line, 40)), 40, "{line:?}");
        }
    }

    fn visible_width(s: &str) -> usize {
        strip_sgr(s).chars().map(char_width).sum()
    }

    // A resize re-renders the whole *document* (the wrap width changed,
    // and so did every table's layout), so the driving loop builds a
    // fresh pager and puts it back where the reader was -- which has to
    // survive the document having become shorter.
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
        assert!(!p.render(10).is_empty());
    }
}
