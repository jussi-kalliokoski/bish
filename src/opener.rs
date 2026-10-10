// Ctrl+T's opener: one list of everywhere you might want to be, narrowed
// by typing. Keys in, a frame out, nothing about terminals, git or
// windows, like commitlist.rs and review.rs, so every rule is unit
// tested. The caller, which knows the windows and what git tracks,
// gathers the list.

use crate::bishedit::fuzzy::fuzzy_match;
use crate::browser::fit_marked;
use crate::editor::Key;
use crate::review::fit;
use crate::window::Rect;
use std::path::PathBuf;

/// What Enter on a row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Go to a pane that is already open.
    Pane { window: u32, pane: u32 },
    /// Open a file in the editor, in a window of its own.
    File(PathBuf),
    /// Start a shell in a directory, in a window of its own.
    Directory(PathBuf),
}

impl Target {
    /// Which of the three groups this row is in. Strict groups rather than a
    /// scoring nudge, since Enter differs between them: going to a pane is
    /// free and reversible, opening a file or shell makes a window, and a
    /// list re-sorting across that line would make one keystroke do
    /// different things by an invisible score.
    fn group(&self) -> usize {
        match self {
            Target::Pane { .. } => 0,
            Target::File(_) => 1,
            Target::Directory(_) => 2,
        }
    }

    /// What the row says Enter will do: bish's word for it, not part of the
    /// matched text.
    fn verb(&self) -> &'static str {
        match self {
            Target::Pane { .. } => "go",
            Target::File(_) => "edit",
            Target::Directory(_) => "cd",
        }
    }
}

/// Where a chosen file or directory opens. A pane isn't one: it already
/// exists. "Open it where" has four answers and no default that suits
/// everyone, and it's the same question for any file, so Ctrl+T offers
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// A window of its own (`::bish window new`), a tab in the bar.
    Tab,
    /// The pane you are already in.
    Here,
    /// A new pane beside this one (`::bish window vsplit`).
    VSplit,
    /// A new pane below this one (`::bish window split`).
    HSplit,
}

impl Destination {
    /// Every destination, in the order Ctrl+T cycles them.
    pub const ALL: [Destination; 4] = [Destination::Tab, Destination::Here, Destination::VSplit, Destination::HSplit];

    /// The one word naming it: what `opener_target` is set to, what
    /// `opener_targets` globs, and what the dialog says it'll do, so what you
    /// read is what you'd type.
    pub fn name(self) -> &'static str {
        match self {
            Destination::Tab => "tab",
            Destination::Here => "here",
            Destination::VSplit => "vsplit",
            Destination::HSplit => "hsplit",
        }
    }
}

pub struct Item {
    pub target: Target,
    /// What the query is matched against, and what the row shows.
    pub text: String,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Continue,
    /// The item at this index, to be acted on.
    Open(usize),
    Close,
}

pub struct Opener {
    items: Vec<Item>,
    query: String,
    /// Indices into `items`, best first, which `selected` indexes.
    view: Vec<usize>,
    /// Char positions in `items[view[i]].text` the query matched, for
    /// underlining. Empty for an empty query.
    matches: Vec<Vec<usize>>,
    selected: usize,
    top: usize,
    height: usize,
    cols: usize,
    /// The destinations Ctrl+T cycles, in `Destination::ALL`'s order. Never
    /// empty: an `opener_targets` glob matching nothing would leave the
    /// dialog unable to open anything, worse than ignoring the typo.
    destinations: Vec<Destination>,
    at: usize,
}

// The verb column and its gutter.
const VERB_WIDTH: usize = 4;
const GUTTER: usize = 2;

impl Opener {
    pub fn new(items: Vec<Item>, destinations: Vec<Destination>, start: Destination, rows: usize, cols: usize) -> Opener {
        // A pane's text is what its program called itself, a path's what
        // someone named a file, and both go into the terminal's escape
        // stream, where `evil<ESC>[2J` is an instruction. Sanitized here,
        // once, so the matched text, shown text and positions are one
        // string.
        let items = items.into_iter().map(|i| Item { text: crate::term::safe_text(&i.text), ..i }).collect();
        // Four rows go to the frame: two borders, the query and the keys.
        let destinations = match destinations.is_empty() {
            true => vec![start],
            false => destinations,
        };
        let at = destinations.iter().position(|d| *d == start).unwrap_or(0);
        let mut opener = Opener {
            items,
            query: String::new(),
            view: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            top: 0,
            height: rows.saturating_sub(4).max(1),
            cols,
            destinations,
            at,
        };
        opener.refilter();
        opener
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.height = rows.saturating_sub(4).max(1);
        self.cols = cols;
        self.reveal();
    }

    /// Where a choice would open right now.
    pub fn destination(&self) -> Destination {
        self.destinations[self.at]
    }

    pub fn item(&self, index: usize) -> &Item {
        &self.items[self.view[index]]
    }

    // Groups first, score within a group, gathering order to break ties,
    // which for panes is the windows' order, so ties list as the tab bar
    // does.
    fn refilter(&mut self) {
        self.view.clear();
        self.matches.clear();
        let mut scored: Vec<(usize, i32, usize, Vec<usize>)> = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            if let Some(m) = fuzzy_match(&self.query, &item.text) {
                scored.push((item.target.group(), m.score, i, m.positions));
            }
        }
        crate::sort::sort_by(&mut scored, |a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        for (_, _, i, positions) in scored {
            self.view.push(i);
            self.matches.push(positions);
        }
        self.selected = 0;
        self.top = 0;
    }

    fn reveal(&mut self) {
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + self.height {
            self.top = self.selected + 1 - self.height;
        }
    }

    fn move_by(&mut self, delta: isize) {
        let last = self.view.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.reveal();
    }

    pub fn handle_key(&mut self, key: Key) -> Outcome {
        match key {
            // The filter owns every printable key: narrowing and picking
            // are one gesture, so only non-text keys move.
            Key::Char(c) => {
                self.query.push(c);
                self.refilter();
            }
            Key::Backspace => {
                self.query.pop();
                self.refilter();
            }
            Key::CtrlU => {
                self.query.clear();
                self.refilter();
            }
            Key::CtrlN | Key::Down => self.move_by(1),
            Key::CtrlP | Key::Up => self.move_by(-1),
            Key::PageDown => self.move_by(self.height as isize),
            Key::PageUp => self.move_by(-(self.height as isize)),
            Key::Enter if !self.view.is_empty() => return Outcome::Open(self.selected),
            // One Esc per level, as in the file browser: the first clears
            // the query, the second closes.
            Key::Escape if !self.query.is_empty() => {
                self.query.clear();
                self.refilter();
            }
            // Ctrl+T again asks the same question: where. The key that
            // opened it is the natural one for "somewhere else". A no-op
            // with one destination, the list wrapping onto itself.
            Key::CtrlT => self.at = (self.at + 1) % self.destinations.len(),
            Key::Escape | Key::CtrlC => return Outcome::Close,
            _ => {}
        }
        Outcome::Continue
    }

    pub fn render(&self, rect: Rect) -> String {
        let width = crate::bishedit::unicode_width::str_width;
        let at = |row: usize| format!("\x1b[{};{}H", rect.row + row + 1, rect.col + 1);
        // Inside the border. Every row writes exactly `rect.cols` columns, so
        // the dialog covers what's under it.
        let mut out = String::new();
        // A frame needs a column each side, so below two there's nothing to
        // draw or pick from: blanked, since the one rule here is never
        // writing outside the rectangle.
        if self.cols < 2 {
            for row in 0..rect.rows {
                out.push_str(&at(row));
                out.push_str(&" ".repeat(self.cols));
            }
            return out;
        }
        let inner = self.cols - 2;

        // The box glyphs the hover popup and completion list use, without
        // their reverse video, which here marks the picked row. The
        // destination is in the border, not the hint row: it's what Enter
        // will do, and the one thing that changes without the list. Too
        // narrow to name it, the box is named alone.
        let title = fit_or_empty(&format!(" open \u{b7} {} ", self.destination().name()), inner.saturating_sub(2));
        let title = match title.is_empty() {
            true => fit_or_empty(" open ", inner.saturating_sub(2)),
            false => title,
        };
        let fill = match title.is_empty() {
            true => "─".repeat(inner),
            false => format!("─{title}{}", "─".repeat(inner - 1 - width(&title))),
        };
        out.push_str(&at(0));
        out.push_str(&format!("╭{fill}╮"));

        // The query line, with how much of the list survives on the right,
        // dropped when too narrow for both.
        let count = format!("{}/{}", self.view.len(), self.items.len());
        let (room, tail) = match inner > width(&count) + 1 {
            true => (inner - width(&count) - 1, format!(" \x1b[2m{count}\x1b[0m")),
            false => (inner, String::new()),
        };
        let typed = keep_end(&format!("> {}", self.query), room);
        out.push_str(&at(1));
        out.push_str(&format!("│\x1b[1m{}\x1b[0m{tail}│", fit(&typed, room)));

        // When narrow, the verb gives way: a row with no room for its name
        // says nothing.
        let verb_width = VERB_WIDTH.min(inner);
        let gutter = GUTTER.min(inner - verb_width);
        let name_width = inner - verb_width - gutter;
        for row in 0..self.height {
            out.push_str(&at(row + 2));
            out.push('│');
            match self.view.get(self.top + row) {
                None => out.push_str(&" ".repeat(inner)),
                Some(&index) => {
                    let item = &self.items[index];
                    let focused = self.top + row == self.selected;
                    if focused {
                        out.push_str("\x1b[7m");
                    }
                    // The verb is dim and the name not, so the eye runs
                    // down the names. The picked row's reverse video is
                    // re-asserted after the dim ends, since ending an
                    // attribute here is a reset.
                    out.push_str(&fit(item.target.verb(), verb_width));
                    out.push_str(&" ".repeat(gutter));
                    let (pieces, used) = fit_marked(&item.text, &self.matches[self.top + row], name_width);
                    for (cluster, matched) in pieces {
                        // Underlined, not coloured: the picked row is
                        // already reverse video.
                        match matched {
                            true => out.push_str(&format!("\x1b[4m{cluster}\x1b[24m")),
                            false => out.push_str(&cluster),
                        }
                    }
                    out.push_str(&" ".repeat(name_width - used));
                    out.push_str("\x1b[0m");
                }
            }
            out.push('│');
        }

        let hint = match self.view.is_empty() && !self.items.is_empty() {
            true => "no match  Esc clears".to_string(),
            // `C-t` earns a column only when there's somewhere else to go.
            false => match self.destinations.len() > 1 {
                true => "Enter open  C-n/C-p move  C-t where  Esc close".to_string(),
                false => "Enter open  C-n/C-p move  Esc close".to_string(),
            },
        };
        out.push_str(&at(rect.rows.saturating_sub(2)));
        out.push_str(&format!("│\x1b[2m{}\x1b[0m│", fit(&hint, inner)));
        out.push_str(&at(rect.rows.saturating_sub(1)));
        out.push_str(&format!("╰{}╯", "─".repeat(inner)));
        // The cursor goes in the query line being typed: two columns in, past
        // the border and the `>`.
        out.push_str(&format!("\x1b[{};{}H\x1b[?25h", rect.row + 2, rect.col + 2 + width(&typed).min(room)));
        out
    }
}

/// `text` if it fits in `width`, else nothing: for the border title, a
/// box too narrow is unnamed rather than showing a letter and a half.
fn fit_or_empty(text: &str, width: usize) -> String {
    match crate::bishedit::unicode_width::str_width(text) <= width {
        true => text.to_string(),
        false => String::new(),
    }
}

/// `s` fitted to `width` by dropping its start, marking what went. The
/// one place bish fits text this way: the end of a query is what's being
/// typed.
fn keep_end(s: &str, width: usize) -> String {
    let w = crate::bishedit::unicode_width::str_width;
    if w(s) <= width {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut start = chars.len();
    let mut used = 0;
    while start > 0 {
        let next = used + crate::bishedit::unicode_width::char_width(chars[start - 1]);
        if next + 1 > width {
            break;
        }
        used = next;
        start -= 1;
    }
    format!("\u{2026}{}", chars[start..].iter().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Item> {
        vec![
            Item { target: Target::Pane { window: 0, pane: 1 }, text: "[0] vim repl.rs  ~/bish".to_string() },
            Item { target: Target::Pane { window: 2, pane: 1 }, text: "[2] ~/work/notes".to_string() },
            Item { target: Target::File(PathBuf::from("src/repl.rs")), text: "src/repl.rs".to_string() },
            Item { target: Target::File(PathBuf::from("src/opener.rs")), text: "src/opener.rs".to_string() },
            Item { target: Target::Directory(PathBuf::from("src")), text: "src/".to_string() },
        ]
    }

    // Every destination enabled, starting at the shipped default, as a `*`
    // glob gives.
    fn opener(items: Vec<Item>, rows: usize, cols: usize) -> Opener {
        Opener::new(items, Destination::ALL.to_vec(), Destination::Tab, rows, cols)
    }

    fn texts(o: &Opener) -> Vec<&str> {
        o.view.iter().map(|&i| o.items[i].text.as_str()).collect()
    }

    fn typed(o: &mut Opener, query: &str) {
        for c in query.chars() {
            assert_eq!(o.handle_key(Key::Char(c)), Outcome::Continue);
        }
    }

    // Groups are an order, not a scoring nudge: "repl.rs" naming a file
    // exactly still doesn't climb above a pane matching the same letters.
    #[test]
    fn the_groups_stay_in_order_however_well_a_later_one_matches() {
        let mut o = opener(items(), 10, 60);
        assert_eq!(
            texts(&o),
            vec!["[0] vim repl.rs  ~/bish", "[2] ~/work/notes", "src/repl.rs", "src/opener.rs", "src/"],
            "with nothing typed, the order they were gathered in"
        );
        typed(&mut o, "repl.rs");
        assert_eq!(texts(&o), vec!["[0] vim repl.rs  ~/bish", "src/repl.rs"]);
        assert_eq!(o.handle_key(Key::Enter), Outcome::Open(0));
        assert!(matches!(o.item(0).target, Target::Pane { window: 0, pane: 1 }));
        // Within a group, rank: "opener" matches one file and not the other,
        // and a matching directory stays below both.
        let mut o = opener(items(), 10, 60);
        typed(&mut o, "sop");
        assert_eq!(texts(&o), vec!["src/opener.rs"]);
    }

    #[test]
    fn typing_narrows_and_backspace_widens_with_the_pick_back_at_the_top() {
        let mut o = opener(items(), 10, 60);
        o.handle_key(Key::CtrlN);
        assert_eq!(o.selected, 1);
        typed(&mut o, "src");
        assert_eq!(texts(&o), vec!["src/repl.rs", "src/opener.rs", "src/"]);
        assert_eq!(o.selected, 0, "a narrowed list is a new list: the pick goes back to the best of it");
        typed(&mut o, "zz");
        assert!(texts(&o).is_empty());
        assert_eq!(o.handle_key(Key::Enter), Outcome::Continue, "nothing matched, so there is nothing to open");
        o.handle_key(Key::Backspace);
        o.handle_key(Key::Backspace);
        assert_eq!(texts(&o), vec!["src/repl.rs", "src/opener.rs", "src/"]);
        // Esc with a query is one level, not the way out.
        assert_eq!(o.handle_key(Key::Escape), Outcome::Continue);
        assert_eq!(texts(&o).len(), 5);
        assert_eq!(o.handle_key(Key::Escape), Outcome::Close);
        typed(&mut o, "src");
        o.handle_key(Key::CtrlU);
        assert_eq!(texts(&o).len(), 5);
    }

    // Ctrl+T in the dialog answers the question it opened: where.
    #[test]
    fn the_key_that_opened_it_cycles_where_a_choice_would_go() {
        let mut o = opener(items(), 10, 60);
        assert_eq!(o.destination(), Destination::Tab);
        for expected in [Destination::Here, Destination::VSplit, Destination::HSplit, Destination::Tab] {
            assert_eq!(o.handle_key(Key::CtrlT), Outcome::Continue);
            assert_eq!(o.destination(), expected, "the cycle wraps in ALL's own order");
        }
        // It changes where, not what: the list and pick are untouched.
        typed(&mut o, "src");
        o.handle_key(Key::CtrlN);
        let before: Vec<String> = texts(&o).iter().map(|s| (*s).to_string()).collect();
        let row = o.selected;
        o.handle_key(Key::CtrlT);
        assert_eq!((texts(&o), o.selected), (before.iter().map(String::as_str).collect::<Vec<_>>(), row));

        // With some destinations enabled, the cycle visits only those,
        // starting at `start`.
        let two = vec![Destination::Here, Destination::HSplit];
        let mut o = Opener::new(items(), two, Destination::HSplit, 10, 60);
        assert_eq!(o.destination(), Destination::HSplit);
        o.handle_key(Key::CtrlT);
        assert_eq!(o.destination(), Destination::Here);
        o.handle_key(Key::CtrlT);
        assert_eq!(o.destination(), Destination::HSplit);

        // With one enabled the key is accepted and does nothing.
        let mut o = Opener::new(items(), vec![Destination::Tab], Destination::Tab, 10, 60);
        assert_eq!(o.handle_key(Key::CtrlT), Outcome::Continue);
        assert_eq!(o.destination(), Destination::Tab);

        // A start not enabled, or no enabled list: misconfiguration, which
        // mustn't leave nowhere to open.
        let o = Opener::new(items(), vec![Destination::VSplit], Destination::Tab, 10, 60);
        assert_eq!(o.destination(), Destination::VSplit);
        let o = Opener::new(items(), Vec::new(), Destination::HSplit, 10, 60);
        assert_eq!(o.destination(), Destination::HSplit);
    }

    // The dialog says which destination is live without being asked.
    #[test]
    fn the_border_names_the_destination_and_the_keys_mention_the_toggle() {
        let mut o = opener(items(), 10, 60);
        let frame = o.render(Rect { row: 0, col: 0, rows: 10, cols: 60 });
        assert!(frame.contains("open \u{b7} tab"), "{frame:?}");
        assert!(frame.contains("C-t where"), "{frame:?}");
        o.handle_key(Key::CtrlT);
        let frame = o.render(Rect { row: 0, col: 0, rows: 10, cols: 60 });
        assert!(frame.contains("open \u{b7} here"), "{frame:?}");
        assert!(!frame.contains("open \u{b7} tab"), "{frame:?}");

        // With one destination the keys row offers no key that does nothing.
        let o = Opener::new(items(), vec![Destination::Tab], Destination::Tab, 10, 60);
        let frame = o.render(Rect { row: 0, col: 0, rows: 10, cols: 60 });
        assert!(frame.contains("open \u{b7} tab"), "{frame:?}");
        assert!(!frame.contains("C-t"), "{frame:?}");

        // Too narrow for both words: the box keeps its own name.
        let o = opener(items(), 10, 14);
        let frame = o.render(Rect { row: 0, col: 0, rows: 10, cols: 14 });
        assert!(frame.contains("open"), "{frame:?}");
        assert!(!frame.contains("tab"), "{frame:?}");
    }

    #[test]
    fn moving_stops_at_either_end_and_the_row_picked_stays_on_screen() {
        let many: Vec<Item> = (0..50).map(|i| Item { target: Target::File(PathBuf::from(format!("f{i}"))), text: format!("f{i}") }).collect();
        // Nine rows: two borders, the query, the keys, and five of list.
        let mut o = opener(many, 9, 40);
        o.handle_key(Key::CtrlP);
        assert_eq!((o.selected, o.top), (0, 0), "nothing above the best match");
        for _ in 0..60 {
            o.handle_key(Key::CtrlN);
        }
        assert_eq!((o.selected, o.top), (49, 45), "nothing below the last, and it is on screen");
        o.handle_key(Key::PageUp);
        assert_eq!((o.selected, o.top), (44, 44), "a page is the five rows this dialog shows");
        // With room for one row, the list scrolls to the picked one.
        o.resize(5, 40);
        assert_eq!((o.selected, o.top), (44, 44));
        o.handle_key(Key::CtrlN);
        assert_eq!((o.selected, o.top), (45, 45));
    }

    // Rows are raw SGR text, so a pane named `evil<ESC>[2J` would clear the
    // screen just by being listed.
    #[test]
    fn a_hostile_name_cannot_paint_with_the_terminals_own_escapes() {
        let o = opener(vec![Item { target: Target::Pane { window: 0, pane: 1 }, text: "evil\x1b[2J\u{7}name".to_string() }], 5, 40);
        assert_eq!(o.items[0].text, "evil\u{fffd}[2J\u{fffd}name");
        let frame = o.render(Rect { row: 0, col: 0, rows: 5, cols: 40 });
        assert!(frame.contains("evil\u{fffd}[2J\u{fffd}name"), "{frame:?}");
        assert!(!frame.contains("\x1b[2J"), "{frame:?}");
        assert!(!frame.contains('\u{7}'), "{frame:?}");
    }

    // Everything in a row that takes no columns.
    fn without_escapes(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        }
        out
    }

    #[test]
    fn a_query_longer_than_the_line_keeps_the_end_being_typed() {
        assert_eq!(keep_end("> src", 10), "> src");
        assert_eq!(keep_end("> src/opener", 8), "\u{2026}/opener");
        assert_eq!(keep_end("> abc", 1), "\u{2026}");
    }

    // The opener owns a rect in a window, so an overrunning row writes into
    // its neighbour.
    #[test]
    fn no_row_is_wider_than_the_rect() {
        for cols in [1, 2, 4, 6, 8, 20, 40, 60] {
            let mut o = opener(items(), 6, cols);
            typed(&mut o, "sr");
            let frame = o.render(Rect { row: 3, col: 5, rows: 6, cols });
            // Each run between cursor placements is one row's text.
            let rows: Vec<&str> = frame.split("\x1b[").filter(|s| s.contains('H')).filter_map(|s| s.split_once('H')).map(|(_, rest)| rest).collect();
            for row in rows {
                let visible = crate::bishedit::unicode_width::str_width(&without_escapes(row));
                assert!(visible <= cols, "{cols} cols: row {row:?} is {visible} wide");
            }
        }
    }
}
