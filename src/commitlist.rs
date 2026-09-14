// `:git log`'s list of commits: one to a row, newest first, and one of
// them picked out to open in the diff view.
//
// Keys in, a frame out, and nothing about terminals or git in between --
// the same arrangement as review.rs and pager.rs, so every rule here is
// a unit test.

use crate::editor::Key;
use crate::git::LogEntry;
use crate::review::fit;
use crate::window::Rect;

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Continue,
    /// The commit at this index, to be opened.
    Open(usize),
    Close,
}

pub struct CommitList {
    title: String,
    entries: Vec<LogEntry>,
    selected: usize,
    top: usize,
    height: usize,
    cols: usize,
}

impl CommitList {
    pub fn new(title: &str, entries: Vec<LogEntry>, rows: usize, cols: usize) -> CommitList {
        // One row for the title, one for the status bar.
        CommitList { title: title.to_string(), entries, selected: 0, top: 0, height: rows.saturating_sub(2).max(1), cols }
    }

    /// The pane changed size. The commit picked out stays on screen.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.height = rows.saturating_sub(2).max(1);
        self.cols = cols;
        self.reveal();
    }

    pub fn entry(&self, index: usize) -> &LogEntry {
        &self.entries[index]
    }

    fn reveal(&mut self) {
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + self.height {
            self.top = self.selected + 1 - self.height;
        }
    }

    // Moves the pick, stopping at either end rather than wrapping: the
    // newest commit and the oldest are the two a reader goes looking for.
    fn move_by(&mut self, delta: isize) {
        let last = self.entries.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.reveal();
    }

    pub fn handle_key(&mut self, key: Key) -> Outcome {
        match key {
            Key::Char('j') | Key::Down => self.move_by(1),
            Key::Char('k') | Key::Up => self.move_by(-1),
            Key::PageDown | Key::Char(' ') => self.move_by(self.height as isize),
            Key::PageUp => self.move_by(-(self.height as isize)),
            Key::Char('g') | Key::Home => self.move_by(-(self.entries.len() as isize)),
            Key::Char('G') | Key::End => self.move_by(self.entries.len() as isize),
            Key::Enter | Key::Char('l') if !self.entries.is_empty() => return Outcome::Open(self.selected),
            Key::Char('q') | Key::Escape => return Outcome::Close,
            _ => {}
        }
        Outcome::Continue
    }

    pub fn render(&self, rect: Rect) -> String {
        let mut out = String::from("\x1b[?25l");
        let at = |row: usize| format!("\x1b[{};{}H", rect.row + row + 1, rect.col + 1);
        out.push_str(&at(0));
        let count = format!("{} commit{}", self.entries.len(), if self.entries.len() == 1 { "" } else { "s" });
        out.push_str(&format!("\x1b[1;7m{}\x1b[0m", fit(&format!("{}  {count}", self.title), self.cols)));
        // Authors to one width, so the subjects start in one column; a
        // long name is cut rather than pushing every subject off screen.
        let author_width = self.entries.iter().map(|e| e.author.chars().count()).max().unwrap_or(0).min(20);
        for row in 0..self.height {
            out.push_str(&at(row + 1));
            let index = self.top + row;
            let Some(entry) = self.entries.get(index) else {
                out.push_str(&" ".repeat(self.cols));
                continue;
            };
            let author: String = entry.author.chars().take(author_width).collect();
            let hash = &entry.hash[..entry.hash.len().min(8)];
            let text = fit(&format!("{hash}  {}  {author:<author_width$}  {}", entry.date, entry.subject), self.cols);
            if index == self.selected {
                out.push_str(&format!("\x1b[7m{text}\x1b[0m"));
            } else {
                out.push_str(&text);
            }
        }
        out.push_str(&at(rect.rows.saturating_sub(1)));
        out.push_str(&format!("\x1b[7m{}\x1b[0m", fit("j/k move  Enter show  q close", self.cols)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(n: usize) -> Vec<LogEntry> {
        (0..n)
            .map(|i| LogEntry { hash: format!("{i:040x}"), date: "2026-09-14".to_string(), author: "A".to_string(), subject: format!("commit {i}") })
            .collect()
    }

    #[test]
    fn moving_stops_at_either_end_and_enter_opens_the_one_picked() {
        let mut list = CommitList::new("t", entries(3), 20, 60);
        assert_eq!(list.handle_key(Key::Char('k')), Outcome::Continue);
        assert_eq!(list.selected, 0, "nothing above the newest");
        for _ in 0..5 {
            list.handle_key(Key::Char('j'));
        }
        assert_eq!(list.selected, 2, "nothing below the oldest");
        assert_eq!(list.handle_key(Key::Enter), Outcome::Open(2));
        assert_eq!(list.handle_key(Key::Char('q')), Outcome::Close);
        assert_eq!(list.handle_key(Key::Escape), Outcome::Close);
    }

    #[test]
    fn the_commit_picked_out_stays_on_screen() {
        let mut list = CommitList::new("t", entries(50), 7, 60);
        list.handle_key(Key::Char('G'));
        assert_eq!((list.selected, list.top), (49, 45));
        let frame = list.render(Rect { row: 0, col: 0, rows: 7, cols: 60 });
        assert!(frame.contains(&format!("\x1b[7m{}  2026-09-14  A  commit 49", &format!("{:040x}", 49)[..8])), "{frame:?}");
        list.handle_key(Key::Char('g'));
        assert_eq!((list.selected, list.top), (0, 0));
    }

    #[test]
    fn an_empty_list_has_nothing_to_open() {
        let mut list = CommitList::new("t", Vec::new(), 10, 40);
        assert_eq!(list.handle_key(Key::Enter), Outcome::Continue);
        assert!(list.render(Rect { row: 0, col: 0, rows: 10, cols: 40 }).contains("t  0 commits"));
    }
}
