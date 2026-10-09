// Ctrl+T's opener: one list of everywhere you might want to be, narrowed
// by typing at it.
//
// Keys in, a frame out, and nothing about terminals, git or windows in
// between -- the same arrangement as commitlist.rs and review.rs, so
// every rule here is a unit test. What goes *in* the list is gathered by
// the caller, which is the only thing that knows what windows are open
// and what git tracks.

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
    /// Which of the three groups this row belongs to. The groups are
    /// strict rather than a nudge in the scoring, because what Enter
    /// *does* differs between them: going to a pane is free and
    /// reversible, opening a file or a shell makes a window. A list that
    /// re-sorted across that line as you typed would mean the same
    /// keystroke doing different things depending on a score nobody can
    /// see.
    fn group(&self) -> usize {
        match self {
            Target::Pane { .. } => 0,
            Target::File(_) => 1,
            Target::Directory(_) => 2,
        }
    }

    /// What the row says Enter will do with it. Not part of the text the
    /// query matches: it is bish's word for the row, not the row's own
    /// name.
    fn verb(&self) -> &'static str {
        match self {
            Target::Pane { .. } => "go",
            Target::File(_) => "edit",
            Target::Directory(_) => "cd",
        }
    }
}

/// Where a chosen file or directory opens.
///
/// A pane is not one of these: it is somewhere that already exists, so
/// `go` goes there and there is nothing to decide. These are about what
/// a choice *makes*, which is why Ctrl+T offers them at all -- the
/// question "open it where" has four answers and no good default for
/// everybody, and it is the same question whichever file you are about
/// to pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// A window of its own -- `::bish window new`, a tab in the bar.
    Tab,
    /// The pane you are already in.
    Here,
    /// A new pane beside this one -- `::bish window vsplit`.
    VSplit,
    /// A new pane below this one -- `::bish window split`.
    HSplit,
}

impl Destination {
    /// Every destination, in the order Ctrl+T cycles them.
    pub const ALL: [Destination; 4] = [Destination::Tab, Destination::Here, Destination::VSplit, Destination::HSplit];

    /// The one word that names it, three times over: what
    /// `opener_target` is set to, what `opener_targets` globs against,
    /// and what the dialog says it is going to do. One spelling, so
    /// what you read in the border is what you would type.
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
    /// Indices into `items`, best first -- the filter's own answer, and
    /// what `selected` indexes into.
    view: Vec<usize>,
    /// Char positions in `items[view[i]].text` that the query matched,
    /// for underlining. Empty for an empty query, which matches
    /// everything with nothing in particular.
    matches: Vec<Vec<usize>>,
    selected: usize,
    top: usize,
    height: usize,
    cols: usize,
    /// The destinations Ctrl+T cycles, in `Destination::ALL`'s order.
    /// Never empty: an `opener_targets` glob that matches nothing would
    /// otherwise leave the dialog unable to open anything at all, which
    /// is a worse answer to a typo than ignoring it.
    destinations: Vec<Destination>,
    at: usize,
}

// The verb column, plus the gutter after it.
const VERB_WIDTH: usize = 4;
const GUTTER: usize = 2;

impl Opener {
    pub fn new(items: Vec<Item>, destinations: Vec<Destination>, start: Destination, rows: usize, cols: usize) -> Opener {
        // A pane's text is whatever the program in it called itself, and
        // a path's is whatever somebody named a file: both are spliced
        // into the terminal's own escape stream a few lines down, where
        // `evil<ESC>[2J` would be an instruction rather than a name.
        // Sanitised here, once, so that the text matched, the text shown
        // and the positions tying them together are all the same string.
        let items = items.into_iter().map(|i| Item { text: crate::term::safe_text(&i.text), ..i }).collect();
        // Four rows go to the frame: its two borders, the query and the
        // keys.
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

    // Groups first, score within a group, and the order they were
    // gathered in to break a tie -- which for panes is the order the
    // windows are in, so a query that tells two of them apart no better
    // than the other still lists them the way the tab bar does.
    fn refilter(&mut self) {
        self.view.clear();
        self.matches.clear();
        let mut scored: Vec<(usize, i32, usize, Vec<usize>)> = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            if let Some(m) = fuzzy_match(&self.query, &item.text) {
                scored.push((item.target.group(), m.score, i, m.positions));
            }
        }
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
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
            // are one gesture here, not two modes, so there is no key
            // left over to mean "down" except the ones that aren't text.
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
            // One Esc per level, as in the file browser: what is typed is
            // a level, so the first Esc takes the list back to everything
            // and the second puts it away.
            Key::Escape if !self.query.is_empty() => {
                self.query.clear();
                self.refilter();
            }
            // Ctrl+T again asks the same question again: *where*. It
            // used to close the dialog, which Escape already did -- and
            // the key that opened the thing is a better place to put
            // "open it somewhere else" than a second key nobody would
            // find. A no-op when one destination is enabled, since the
            // list wraps onto itself.
            Key::CtrlT => self.at = (self.at + 1) % self.destinations.len(),
            Key::Escape | Key::CtrlC => return Outcome::Close,
            _ => {}
        }
        Outcome::Continue
    }

    pub fn render(&self, rect: Rect) -> String {
        let width = crate::bishedit::unicode_width::str_width;
        let at = |row: usize| format!("\x1b[{};{}H", rect.row + row + 1, rect.col + 1);
        // Inside the border. Every row below writes exactly `rect.cols`
        // columns, border included, so the dialog covers what is under it
        // rather than letting it show through.
        let mut out = String::new();
        // A frame spends a column on each of its sides, so below two
        // columns there is no frame to draw -- and whatever this is
        // standing in a rectangle that narrow, it is not somewhere to
        // pick from. Blanked rather than drawn, because the one rule this
        // function cannot break is writing outside the rectangle it was
        // given.
        if self.cols < 2 {
            for row in 0..rect.rows {
                out.push_str(&at(row));
                out.push_str(&" ".repeat(self.cols));
            }
            return out;
        }
        let inner = self.cols - 2;

        // The glyphs the hover popup and the completion list already use
        // for a box that floats over real content -- but not their
        // reverse video: this box has a row picked out inside it, and
        // that is what reverse video means here.
        // The destination goes in the border rather than the hint row
        // because it is not advice, it is what Enter is about to do --
        // and it is the one thing in here that changes without the list
        // changing with it. Too narrow to name it, and the box is named
        // alone rather than half-named.
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

        // The query line, with how much of the list survives it on the
        // right -- dropped rather than crowding the query out when the
        // dialog is too narrow for both.
        let count = format!("{}/{}", self.view.len(), self.items.len());
        let (room, tail) = match inner > width(&count) + 1 {
            true => (inner - width(&count) - 1, format!(" \x1b[2m{count}\x1b[0m")),
            false => (inner, String::new()),
        };
        let typed = keep_end(&format!("> {}", self.query), room);
        out.push_str(&at(1));
        out.push_str(&format!("│\x1b[1m{}\x1b[0m{tail}│", fit(&typed, room)));

        // Narrow enough and the verb is what gives way: a row with no
        // room for its name says nothing at all.
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
                    // The verb is dim and the name is not, so the eye
                    // runs down the names; the reverse video of the row
                    // picked out has to be re-asserted after the dim
                    // ends, since ending an attribute here means a reset.
                    out.push_str(&fit(item.target.verb(), verb_width));
                    out.push_str(&" ".repeat(gutter));
                    let (pieces, used) = fit_marked(&item.text, &self.matches[self.top + row], name_width);
                    for (cluster, matched) in pieces {
                        // Underlined, not coloured: the row picked out is
                        // already reverse video, and a colour on top of
                        // that is a second thing to read.
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
            // `C-t` is only worth a column when there is somewhere else
            // for it to go.
            false => match self.destinations.len() > 1 {
                true => "Enter open  C-n/C-p move  C-t where  Esc close".to_string(),
                false => "Enter open  C-n/C-p move  Esc close".to_string(),
            },
        };
        out.push_str(&at(rect.rows.saturating_sub(2)));
        out.push_str(&format!("│\x1b[2m{}\x1b[0m│", fit(&hint, inner)));
        out.push_str(&at(rect.rows.saturating_sub(1)));
        out.push_str(&format!("╰{}╯", "─".repeat(inner)));
        // Left where it is being typed, and visible: this is an input
        // line, so the cursor belongs in it rather than parked on the row
        // picked out. Two columns in: past the border, past the `>`.
        out.push_str(&format!("\x1b[{};{}H\x1b[?25h", rect.row + 2, rect.col + 2 + width(&typed).min(room)));
        out
    }
}

/// `text` if it fits in `width`, nothing at all if it does not.
///
/// For the title in the border: a box too narrow to name is drawn
/// unnamed, rather than with a word cut down to a letter and a half.
fn fit_or_empty(text: &str, width: usize) -> String {
    match crate::bishedit::unicode_width::str_width(text) <= width {
        true => text.to_string(),
        false => String::new(),
    }
}

/// `s` fitted to `width` columns by dropping the *start* of it, marking
/// what went.
///
/// The query line is the one place in bish that fits text this way round:
/// everywhere else the beginning is the identifying part, and here it is
/// the end, because the end is what is being typed.
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

    // Every destination enabled, starting where the shipped default
    // does -- the shape a `*` glob produces.
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

    // What Enter does differs by group, so the groups are an order and
    // not a scoring nudge: "repl.rs" names a file exactly and still does
    // not climb above the pane that merely contains the same letters.
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
        // Within a group, rank: "opener" matches one file and not the
        // other, and the directory that also matches stays below both.
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
        // Esc with something typed is one level, not the way out.
        assert_eq!(o.handle_key(Key::Escape), Outcome::Continue);
        assert_eq!(texts(&o).len(), 5);
        assert_eq!(o.handle_key(Key::Escape), Outcome::Close);
        typed(&mut o, "src");
        o.handle_key(Key::CtrlU);
        assert_eq!(texts(&o).len(), 5);
    }

    // Ctrl+T inside the dialog answers the question it opened: where.
    #[test]
    fn the_key_that_opened_it_cycles_where_a_choice_would_go() {
        let mut o = opener(items(), 10, 60);
        assert_eq!(o.destination(), Destination::Tab);
        for expected in [Destination::Here, Destination::VSplit, Destination::HSplit, Destination::Tab] {
            assert_eq!(o.handle_key(Key::CtrlT), Outcome::Continue);
            assert_eq!(o.destination(), expected, "the cycle wraps in ALL's own order");
        }
        // It changes where, not what: the list and the pick are untouched.
        typed(&mut o, "src");
        o.handle_key(Key::CtrlN);
        let before: Vec<String> = texts(&o).iter().map(|s| (*s).to_string()).collect();
        let row = o.selected;
        o.handle_key(Key::CtrlT);
        assert_eq!((texts(&o), o.selected), (before.iter().map(String::as_str).collect::<Vec<_>>(), row));

        // Only some destinations enabled: the cycle visits exactly
        // those, and `start` picks where it begins rather than the
        // table's own first entry.
        let two = vec![Destination::Here, Destination::HSplit];
        let mut o = Opener::new(items(), two, Destination::HSplit, 10, 60);
        assert_eq!(o.destination(), Destination::HSplit);
        o.handle_key(Key::CtrlT);
        assert_eq!(o.destination(), Destination::Here);
        o.handle_key(Key::CtrlT);
        assert_eq!(o.destination(), Destination::HSplit);

        // One enabled: the key is still accepted and still does
        // nothing, which is what a list of one wrapping onto itself
        // means -- and is better than the key closing the dialog by
        // surprise, which is what it used to do.
        let mut o = Opener::new(items(), vec![Destination::Tab], Destination::Tab, 10, 60);
        assert_eq!(o.handle_key(Key::CtrlT), Outcome::Continue);
        assert_eq!(o.destination(), Destination::Tab);

        // A start nothing enabled, and no enabled list at all: both are
        // misconfiguration, and neither may leave the dialog with
        // nowhere to open anything.
        let o = Opener::new(items(), vec![Destination::VSplit], Destination::Tab, 10, 60);
        assert_eq!(o.destination(), Destination::VSplit);
        let o = Opener::new(items(), Vec::new(), Destination::HSplit, 10, 60);
        assert_eq!(o.destination(), Destination::HSplit);
    }

    // The destination is what Enter is about to do, so the dialog has to
    // say which one is live without being asked.
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

        // With one destination there is nothing to toggle, so the keys
        // row does not offer a key that would do nothing.
        let o = Opener::new(items(), vec![Destination::Tab], Destination::Tab, 10, 60);
        let frame = o.render(Rect { row: 0, col: 0, rows: 10, cols: 60 });
        assert!(frame.contains("open \u{b7} tab"), "{frame:?}");
        assert!(!frame.contains("C-t"), "{frame:?}");

        // Too narrow for both words: the box keeps its own name rather
        // than showing half of the destination's.
        let o = opener(items(), 10, 14);
        let frame = o.render(Rect { row: 0, col: 0, rows: 10, cols: 14 });
        assert!(frame.contains("open"), "{frame:?}");
        assert!(!frame.contains("tab"), "{frame:?}");
    }

    #[test]
    fn moving_stops_at_either_end_and_the_row_picked_stays_on_screen() {
        let many: Vec<Item> = (0..50).map(|i| Item { target: Target::File(PathBuf::from(format!("f{i}"))), text: format!("f{i}") }).collect();
        // Nine rows of dialog: two borders, the query, the keys, and five
        // of list.
        let mut o = opener(many, 9, 40);
        o.handle_key(Key::CtrlP);
        assert_eq!((o.selected, o.top), (0, 0), "nothing above the best match");
        for _ in 0..60 {
            o.handle_key(Key::CtrlN);
        }
        assert_eq!((o.selected, o.top), (49, 45), "nothing below the last, and it is on screen");
        o.handle_key(Key::PageUp);
        assert_eq!((o.selected, o.top), (44, 44), "a page is the five rows this dialog shows");
        // A dialog with room for one row has to scroll to the row picked
        // out, which the five-row one already had on screen.
        o.resize(5, 40);
        assert_eq!((o.selected, o.top), (44, 44));
        o.handle_key(Key::CtrlN);
        assert_eq!((o.selected, o.top), (45, 45));
    }

    // Every row is drawn as raw SGR text, so a name goes into the
    // terminal's own escape stream directly -- a pane running something
    // that called itself `evil<ESC>[2J` would clear the screen just by
    // being open.
    #[test]
    fn a_hostile_name_cannot_paint_with_the_terminals_own_escapes() {
        let o = opener(vec![Item { target: Target::Pane { window: 0, pane: 1 }, text: "evil\x1b[2J\u{7}name".to_string() }], 5, 40);
        assert_eq!(o.items[0].text, "evil\u{fffd}[2J\u{fffd}name");
        let frame = o.render(Rect { row: 0, col: 0, rows: 5, cols: 40 });
        assert!(frame.contains("evil\u{fffd}[2J\u{fffd}name"), "{frame:?}");
        assert!(!frame.contains("\x1b[2J"), "{frame:?}");
        assert!(!frame.contains('\u{7}'), "{frame:?}");
    }

    // Everything a row has that costs no columns.
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

    // The opener owns a rect inside a window, so a row that overruns it
    // writes into whatever is beside it.
    #[test]
    fn no_row_is_wider_than_the_rect() {
        for cols in [1, 2, 4, 6, 8, 20, 40, 60] {
            let mut o = opener(items(), 6, cols);
            typed(&mut o, "sr");
            let frame = o.render(Rect { row: 3, col: 5, rows: 6, cols });
            // Every run between two cursor placements is one row's text.
            let rows: Vec<&str> = frame.split("\x1b[").filter(|s| s.contains('H')).filter_map(|s| s.split_once('H')).map(|(_, rest)| rest).collect();
            for row in rows {
                let visible = crate::bishedit::unicode_width::str_width(&without_escapes(row));
                assert!(visible <= cols, "{cols} cols: row {row:?} is {visible} wide");
            }
        }
    }
}
