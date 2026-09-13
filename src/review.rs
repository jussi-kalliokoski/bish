// A diff to decide on, one change at a time.
//
// Two versions of one text -- what it was and what it would become --
// shown as one unified view with every change marked, and a verdict on
// each: taken, refused, or not decided yet. `reverts` turns the verdicts
// into the edits that undo the refused changes in a buffer already
// holding the new version.
//
// Keys in, a frame out, and nothing about terminals or editors in
// between, so every rule here can be a unit test. Shaped after pager.rs,
// which is the same arrangement for reading rather than deciding.

use crate::bishedit::unicode_width::char_width;
use crate::editor::Key;
use crate::window::Rect;
use std::ops::Range;

/// One change: the lines of the old text it replaces, and the lines of
/// the new text it puts there. Either range may be empty -- a pure
/// addition replaces nothing, a pure removal puts nothing back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Undecided,
    Accepted,
    Rejected,
}

/// Every change from `old` to `new`, in order. A removal directly next to
/// an addition is one change -- a line rewritten, not a line deleted and
/// an unrelated one added -- because that is what someone deciding on it
/// is deciding.
pub fn hunks(old: &[String], new: &[String]) -> Vec<Hunk> {
    use crate::diff::DiffOp;
    let ops = crate::diff::diff(old, new);
    let mut out = Vec::new();
    let (mut a, mut b) = (0, 0);
    let mut i = 0;
    while i < ops.len() {
        match ops[i] {
            DiffOp::Equal { a: at, b: bt, len } => {
                (a, b) = (at + len, bt + len);
            }
            DiffOp::Delete { a: at, len } => {
                let added = match ops.get(i + 1) {
                    Some(DiffOp::Insert { b: bt, len: added }) => {
                        i += 1;
                        *bt..*bt + *added
                    }
                    _ => b..b,
                };
                (a, b) = (at + len, added.end);
                out.push(Hunk { old: at..at + len, new: added });
            }
            DiffOp::Insert { b: bt, len } => {
                let removed = match ops.get(i + 1) {
                    Some(DiffOp::Delete { a: at, len: removed }) => {
                        i += 1;
                        *at..*at + *removed
                    }
                    _ => a..a,
                };
                (a, b) = (removed.end, bt + len);
                out.push(Hunk { old: removed, new: bt..bt + len });
            }
        }
        i += 1;
    }
    out
}

/// The text the verdicts add up to: the new version of every accepted
/// change and the old version of every other, with `undecided` saying
/// which way a change nobody ruled on goes.
pub fn resolve(old: &[String], new: &[String], hunks: &[Hunk], verdicts: &[Verdict], undecided: Verdict) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    for (hunk, verdict) in hunks.iter().zip(verdicts) {
        out.extend_from_slice(&old[at..hunk.old.start]);
        let verdict = if *verdict == Verdict::Undecided { undecided } else { *verdict };
        match verdict {
            Verdict::Accepted => out.extend_from_slice(&new[hunk.new.clone()]),
            _ => out.extend_from_slice(&old[hunk.old.clone()]),
        }
        at = hunk.old.end;
    }
    out.extend_from_slice(&old[at..]);
    out
}

// Unchanged lines kept either side of a change. `diff -u`'s own number.
const CONTEXT: usize = 3;

// One row of the unified view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    // An unchanged line: its index in the old text and in the new.
    Context { old: usize, new: usize },
    // A run of unchanged lines too far from any change to be worth showing.
    Skipped(usize),
    Header(usize),
    Removed { line: usize, hunk: usize },
    Added { line: usize, hunk: usize },
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Continue,
    /// Finished: the verdicts stand.
    Done,
    /// Walked away from: nothing decided here should happen.
    Cancel,
}

pub struct Review {
    title: String,
    old: Vec<String>,
    new: Vec<String>,
    hunks: Vec<Hunk>,
    verdicts: Vec<Verdict>,
    rows: Vec<Row>,
    current: usize,
    top: usize,
    height: usize,
    cols: usize,
}

impl Review {
    pub fn new(title: &str, old: Vec<String>, new: Vec<String>, rows: usize, cols: usize) -> Review {
        let hunks = hunks(&old, &new);
        let layout = layout(&hunks, old.len());
        let verdicts = vec![Verdict::Undecided; hunks.len()];
        let mut review = Review {
            title: title.to_string(),
            old,
            new,
            hunks,
            verdicts,
            rows: layout,
            current: 0,
            top: 0,
            // One row for the title, one for the status bar.
            height: rows.saturating_sub(2).max(1),
            cols,
        };
        review.reveal_current();
        review
    }

    /// The pane changed size. Where the reader was stays put.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.height = rows.saturating_sub(2).max(1);
        self.cols = cols;
        self.top = self.top.min(self.max_top());
    }

    /// The text the verdicts add up to, with `undecided` saying which way
    /// a change nobody ruled on goes -- see `resolve`.
    pub fn resolved(&self, undecided: Verdict) -> Vec<String> {
        resolve(&self.old, &self.new, &self.hunks, &self.verdicts, undecided)
    }

    /// Whether anything at all was taken.
    pub fn accepted_any(&self) -> bool {
        self.verdicts.contains(&Verdict::Accepted)
    }

    /// What undoes every refused change in a text that already holds the
    /// new version: which of its lines to replace, and with what. Last
    /// change first, so applying them in order never moves a line a later
    /// one still refers to.
    pub fn reverts(&self) -> Vec<(Range<usize>, Vec<String>)> {
        self.hunks
            .iter()
            .zip(&self.verdicts)
            .rev()
            .filter(|(_, v)| **v == Verdict::Rejected)
            .map(|(h, _)| (h.new.clone(), self.old[h.old.clone()].to_vec()))
            .collect()
    }

    fn max_top(&self) -> usize {
        self.rows.len().saturating_sub(self.height)
    }

    fn scroll_by(&mut self, delta: isize) {
        self.top = (self.top as isize + delta).clamp(0, self.max_top() as isize) as usize;
    }

    fn header_row(&self, hunk: usize) -> Option<usize> {
        self.rows.iter().position(|r| *r == Row::Header(hunk))
    }

    // The current change on screen, a third of the way down when it was
    // not already, so the lines leading into it can be read too.
    fn reveal_current(&mut self) {
        let Some(header) = self.header_row(self.current) else { return };
        let end = self
            .rows
            .iter()
            .rposition(|r| matches!(r, Row::Removed { hunk, .. } | Row::Added { hunk, .. } if *hunk == self.current))
            .unwrap_or(header);
        if header >= self.top && end < self.top + self.height {
            return;
        }
        self.top = header.saturating_sub(self.height / 3).min(self.max_top());
    }

    fn go_to(&mut self, hunk: usize) {
        if hunk < self.hunks.len() {
            self.current = hunk;
            self.reveal_current();
        }
    }

    // After a verdict: on to the next change still waiting for one,
    // wrapping round, or stay put when there is none.
    fn decide(&mut self, verdict: Verdict) {
        if self.hunks.is_empty() {
            return;
        }
        self.verdicts[self.current] = verdict;
        let n = self.hunks.len();
        if let Some(next) = (1..=n).map(|step| (self.current + step) % n).find(|&i| self.verdicts[i] == Verdict::Undecided) {
            self.go_to(next);
        }
    }

    fn decide_rest(&mut self, verdict: Verdict) {
        for v in &mut self.verdicts {
            if *v == Verdict::Undecided {
                *v = verdict;
            }
        }
    }

    pub fn handle_key(&mut self, key: Key) -> Outcome {
        match key {
            Key::Char('j') | Key::Down => self.scroll_by(1),
            Key::Char('k') | Key::Up => self.scroll_by(-1),
            Key::PageDown | Key::Char(' ') => self.scroll_by(self.height as isize),
            Key::PageUp => self.scroll_by(-(self.height as isize)),
            Key::Char('g') | Key::Home => self.top = 0,
            Key::Char('G') | Key::End => self.top = self.max_top(),
            Key::Char('n') | Key::Char(']') => self.go_to(self.current + 1),
            Key::Char('N') | Key::Char('[') => {
                if self.current > 0 {
                    self.go_to(self.current - 1);
                }
            }
            Key::Char('a') | Key::Char('y') => self.decide(Verdict::Accepted),
            Key::Char('r') | Key::Char('x') => self.decide(Verdict::Rejected),
            Key::Char('u') => {
                if let Some(v) = self.verdicts.get_mut(self.current) {
                    *v = Verdict::Undecided;
                }
            }
            Key::Char('A') => self.decide_rest(Verdict::Accepted),
            Key::Char('R') => self.decide_rest(Verdict::Rejected),
            Key::Char('q') | Key::Enter => return Outcome::Done,
            Key::Escape => return Outcome::Cancel,
            _ => {}
        }
        Outcome::Continue
    }

    pub fn render(&self, rect: Rect) -> String {
        let mut out = String::from("\x1b[?25l");
        let at = |row: usize| format!("\x1b[{};{}H", rect.row + row + 1, rect.col + 1);
        out.push_str(&at(0));
        let position = if self.hunks.is_empty() { "no changes".to_string() } else { format!("change {} of {}", self.current + 1, self.hunks.len()) };
        out.push_str(&format!("\x1b[1;7m{}\x1b[0m", fit(&format!("{}  {position}", self.title), self.cols)));
        for row in 0..self.height {
            out.push_str(&at(row + 1));
            match self.rows.get(self.top + row) {
                Some(r) => out.push_str(&self.render_row(*r)),
                None => out.push_str(&" ".repeat(self.cols)),
            }
        }
        out.push_str(&at(rect.rows.saturating_sub(1)));
        out.push_str(&format!("\x1b[7m{}\x1b[0m", fit(&self.status(), self.cols)));
        out
    }

    // One row, styled. What a verdict does shows on the lines it applies
    // to: an accepted change's removed lines and a refused change's added
    // ones are the lines that will not be there, so they are dimmed and
    // struck through.
    fn render_row(&self, row: Row) -> String {
        let verdict = |hunk: usize| self.verdicts[hunk];
        let (sgr, text) = match row {
            Row::Context { new, .. } => ("", format!("  {}", self.new[new])),
            Row::Skipped(n) => ("2", format!("  \u{22ef} {n} unchanged line{}", if n == 1 { "" } else { "s" })),
            Row::Header(hunk) => {
                let h = &self.hunks[hunk];
                let label = match verdict(hunk) {
                    Verdict::Undecided => "",
                    Verdict::Accepted => "  accepted",
                    Verdict::Rejected => "  rejected",
                };
                let text = format!("@@ -{},{} +{},{} @@{label}", h.old.start + 1, h.old.len(), h.new.start + 1, h.new.len());
                (if hunk == self.current { "1;7" } else { "36" }, text)
            }
            Row::Removed { line, hunk } => (if verdict(hunk) == Verdict::Accepted { "31;2;9" } else { "31" }, format!("- {}", self.old[line])),
            Row::Added { line, hunk } => (if verdict(hunk) == Verdict::Rejected { "32;2;9" } else { "32" }, format!("+ {}", self.new[line])),
        };
        let fitted = fit(&text, self.cols);
        if sgr.is_empty() { fitted } else { format!("\x1b[{sgr}m{fitted}\x1b[0m") }
    }

    fn status(&self) -> String {
        let count = |v: Verdict| self.verdicts.iter().filter(|x| **x == v).count();
        format!(
            "{} accepted, {} rejected, {} to go   a accept  r reject  A/R the rest  n/N change  q done  Esc cancel",
            count(Verdict::Accepted),
            count(Verdict::Rejected),
            count(Verdict::Undecided)
        )
    }
}

// The rows of the unified view: every change with `CONTEXT` unchanged
// lines either side of it, and whatever unchanged run is longer than
// that folded into one row saying how long.
fn layout(hunks: &[Hunk], old_len: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    // Where the unchanged run before the next change starts, in both
    // texts. The two only ever differ by how much the changes so far
    // added or removed.
    let (mut old_at, mut new_at) = (0, 0);
    let unchanged = |rows: &mut Vec<Row>, old_at: usize, new_at: usize, len: usize, first: bool, last: bool| {
        let head = if first { 0 } else { len.min(CONTEXT) };
        let tail = if last { 0 } else { len.saturating_sub(head).min(CONTEXT) };
        for k in 0..head {
            rows.push(Row::Context { old: old_at + k, new: new_at + k });
        }
        if len > head + tail {
            rows.push(Row::Skipped(len - head - tail));
        }
        for k in len - tail..len {
            rows.push(Row::Context { old: old_at + k, new: new_at + k });
        }
    };
    for (i, hunk) in hunks.iter().enumerate() {
        unchanged(&mut rows, old_at, new_at, hunk.old.start - old_at, i == 0, false);
        rows.push(Row::Header(i));
        rows.extend(hunk.old.clone().map(|line| Row::Removed { line, hunk: i }));
        rows.extend(hunk.new.clone().map(|line| Row::Added { line, hunk: i }));
        (old_at, new_at) = (hunk.old.end, hunk.new.end);
    }
    if !hunks.is_empty() {
        unchanged(&mut rows, old_at, new_at, old_len - old_at, false, true);
    }
    rows
}

// Exactly `cols` columns of `text`: cut at the last whole character that
// fits, then padded. Tabs become spaces first -- a tab's width depends on
// where it lands, and here every line starts after a two-column marker.
fn fit(text: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars().flat_map(|c| if c == '\t' { vec![' '; 4] } else { vec![c] }) {
        let w = char_width(ch);
        if width + w > cols {
            break;
        }
        width += w;
        out.push(ch);
    }
    out.push_str(&" ".repeat(cols - width));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    fn shown(review: &Review) -> Vec<String> {
        (0..review.height).filter_map(|r| review.rows.get(review.top + r)).map(|row| plain(&review.render_row(*row)).trim_end().to_string()).collect()
    }

    #[test]
    fn a_rewritten_line_is_one_change_and_a_pure_addition_replaces_nothing() {
        let old = lines("a\nb\nc\nd");
        let new = lines("a\nB\nc\nd\ne");
        assert_eq!(hunks(&old, &new), vec![Hunk { old: 1..2, new: 1..2 }, Hunk { old: 4..4, new: 4..5 }]);
        let removed = lines("a\nc");
        assert_eq!(hunks(&old, &removed)[0], Hunk { old: 1..2, new: 1..1 });
    }

    #[test]
    fn the_verdicts_decide_which_version_of_each_change_is_kept() {
        let old = lines("a\nb\nc\nd");
        let new = lines("a\nB\nc\nD");
        let h = hunks(&old, &new);
        let both = [Verdict::Accepted, Verdict::Rejected];
        assert_eq!(resolve(&old, &new, &h, &both, Verdict::Rejected), lines("a\nB\nc\nd"));
        let open = [Verdict::Undecided, Verdict::Rejected];
        assert_eq!(resolve(&old, &new, &h, &open, Verdict::Accepted), lines("a\nB\nc\nd"), "undecided goes the way asked");
        assert_eq!(resolve(&old, &new, &h, &open, Verdict::Rejected), old);
    }

    #[test]
    fn reverts_put_back_the_refused_changes_last_first() {
        let old = lines("a\nb\nc\nd");
        let new = lines("a\nB\nB2\nc\nD");
        let mut review = Review::new("t", old.clone(), new.clone(), 20, 40);
        review.handle_key(Key::Char('r'));
        review.handle_key(Key::Char('r'));
        let reverts = review.reverts();
        assert_eq!(reverts, vec![(4..5, lines("d")), (1..3, lines("b"))]);
        let mut text = new;
        for (range, replacement) in reverts {
            text.splice(range, replacement);
        }
        assert_eq!(text, old, "applied in order, they give the old text back");
    }

    #[test]
    fn unchanged_lines_far_from_any_change_fold_into_one_row() {
        let old: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        let mut new = old.clone();
        new[10] = "ten".to_string();
        let review = Review::new("t", old, new, 40, 40);
        assert_eq!(
            shown(&review),
            vec![
                "  \u{22ef} 7 unchanged lines",
                "  7",
                "  8",
                "  9",
                "@@ -11,1 +11,1 @@",
                "- 10",
                "+ ten",
                "  11",
                "  12",
                "  13",
                "  \u{22ef} 6 unchanged lines"
            ]
        );
    }

    #[test]
    fn deciding_moves_on_to_the_next_undecided_change() {
        let old = lines("a\nb\nc\nd\ne");
        let new = lines("A\nb\nC\nd\nE");
        let mut review = Review::new("t", old, new, 40, 40);
        assert_eq!(review.handle_key(Key::Char('a')), Outcome::Continue);
        assert_eq!(review.current, 1);
        review.handle_key(Key::Char('n'));
        review.handle_key(Key::Char('r'));
        assert_eq!(review.verdicts, [Verdict::Accepted, Verdict::Undecided, Verdict::Rejected]);
        assert_eq!(review.current, 1, "round to the one still waiting");
        review.handle_key(Key::Char('A'));
        assert_eq!(review.verdicts, [Verdict::Accepted, Verdict::Accepted, Verdict::Rejected], "the rest, and only the rest");
        assert_eq!(review.handle_key(Key::Char('q')), Outcome::Done);
        assert_eq!(review.handle_key(Key::Escape), Outcome::Cancel);
    }

    #[test]
    fn a_change_off_screen_is_brought_on_when_it_becomes_current() {
        let old: Vec<String> = (0..200).map(|i| i.to_string()).collect();
        let mut new = old.clone();
        new[5] = "five".to_string();
        new[150] = "one fifty".to_string();
        let mut review = Review::new("t", old, new, 12, 40);
        review.handle_key(Key::Char('n'));
        assert!(shown(&review).iter().any(|row| row == "+ one fifty"), "{:?}", shown(&review));
    }

    #[test]
    fn the_frame_says_where_you_are_and_what_is_left() {
        let review = Review::new("file.rs", lines("a\nb"), lines("a\nc"), 10, 120);
        let frame = plain(&review.render(Rect { row: 0, col: 0, rows: 10, cols: 120 }));
        assert!(frame.contains("file.rs  change 1 of 1"), "{frame}");
        assert!(frame.contains("0 accepted, 0 rejected, 1 to go"), "{frame}");
    }
}
