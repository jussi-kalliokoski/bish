// A diff to read, or to decide on one change at a time.
//
// Two versions of some text -- what it was and what it would become --
// shown as one unified view with every change marked. It has two uses,
// which differ only in what a key does to a change:
//
// - Deciding (`:review`, `openDiff`): one file, and a verdict on each
//   change -- taken, refused, or not decided yet. `reverts` turns the
//   verdicts into the edits that undo the refused ones in a buffer
//   already holding the new version.
// - Reading (`:git show`): a commit's header and every file it touched,
//   each under a heading of its own, with nothing to decide.
//
// Keys in, a frame out, and nothing about terminals, editors or git in
// between, so every rule here can be a unit test. Shaped after pager.rs,
// which is the same arrangement for text that is not a diff.

use crate::bishedit::highlight::{self, ColorOverrides, HighlightContext, StyledSpan};
use crate::bishedit::unicode_width::char_width;
use crate::editor::Key;
use crate::vt100::{Cell, CellAttrs, Color};
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

/// One file's two versions, for a view of several.
#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    /// What its heading says: a path, or how it was renamed.
    pub label: String,
    pub old: Vec<String>,
    pub new: Vec<String>,
    /// Either side is not text. Said so, and not diffed.
    pub binary: bool,
    /// What it is written in, for its colours -- a name
    /// `highlight::highlighter_for_language` knows, or anything else for
    /// none.
    pub language: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Deciding,
    Reading,
}

// Unchanged lines kept either side of a change. `diff -u`'s own number.
const CONTEXT: usize = 3;

// One row of the unified view. `file` is an index into the view's files,
// `hunk` into its hunks, which run on across files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    // A line of the header a reading view opens with -- a commit's hash,
    // author, date and message.
    Message(usize),
    Blank,
    // A file's heading.
    File(usize),
    // A file that is not text, or whose contents did not change (a mode
    // change, a rename and nothing more).
    Binary(usize),
    Unchanged(usize),
    // An unchanged line: its index in the old text and in the new.
    Context { file: usize, old: usize, new: usize },
    // A run of unchanged lines too far from any change to be worth showing.
    Skipped(usize),
    Header(usize),
    Removed { file: usize, line: usize, hunk: usize },
    Added { file: usize, line: usize, hunk: usize },
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Continue,
    /// Finished: the verdicts stand. For a reading view, just closed.
    Done,
    /// Walked away from: nothing decided here should happen.
    Cancel,
}

pub struct Review {
    title: String,
    mode: Mode,
    header: Vec<String>,
    files: Vec<FileDiff>,
    // Every change in every file, in order, with the file it is in.
    hunks: Vec<(usize, Hunk)>,
    verdicts: Vec<Verdict>,
    rows: Vec<Row>,
    current: usize,
    top: usize,
    height: usize,
    cols: usize,
    // Each file's syntax colours, old side and new, one list of spans per
    // line. Empty until `highlighted`, and for a language with no
    // highlighter.
    paint: Vec<[Vec<Vec<StyledSpan>>; 2]>,
}

impl Review {
    /// A view to decide on the changes from `old` to `new`, one file.
    pub fn new(title: &str, old: Vec<String>, new: Vec<String>, language: &str, rows: usize, cols: usize) -> Review {
        let file = FileDiff { label: String::new(), old, new, binary: false, language: language.to_string() };
        let mut review = Review::build(title, Mode::Deciding, Vec::new(), vec![file], rows, cols);
        review.reveal_current();
        review
    }

    /// A view to read: `header` above every file in `files`, each under a
    /// heading. `start` is the file to open at; with none it opens at the
    /// top, so the header is the first thing on screen.
    pub fn reading(title: &str, header: Vec<String>, files: Vec<FileDiff>, start: Option<usize>, rows: usize, cols: usize) -> Review {
        let mut review = Review::build(title, Mode::Reading, header, files, rows, cols);
        if let Some(file) = start {
            review.go_to_file(file);
        }
        review
    }

    fn build(title: &str, mode: Mode, header: Vec<String>, files: Vec<FileDiff>, rows: usize, cols: usize) -> Review {
        let mut hunks_all = Vec::new();
        for (f, file) in files.iter().enumerate() {
            if !file.binary {
                hunks_all.extend(hunks(&file.old, &file.new).into_iter().map(|h| (f, h)));
            }
        }
        let rows_all = layout(mode, &header, &files, &hunks_all);
        Review {
            title: title.to_string(),
            mode,
            header,
            verdicts: vec![Verdict::Undecided; hunks_all.len()],
            files,
            hunks: hunks_all,
            rows: rows_all,
            current: 0,
            top: 0,
            // One row for the title, one for the status bar.
            height: rows.saturating_sub(2).max(1),
            cols,
            paint: Vec::new(),
        }
    }

    /// The view with its lines in their language's colours, `overrides`
    /// being `::bish hl`'s. Each side of each file is highlighted whole,
    /// as the editor does a buffer, so a line inside a comment or string
    /// that started above it is coloured as what it is, not as what it
    /// would be on its own.
    pub fn highlighted(mut self, overrides: Option<&ColorOverrides>) -> Review {
        self.paint = self
            .files
            .iter()
            .map(|file| match file.binary {
                true => [Vec::new(), Vec::new()],
                false => [paint(&file.language, &file.old, overrides), paint(&file.language, &file.new, overrides)],
            })
            .collect();
        self
    }

    /// The pane changed size. Where the reader was stays put.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.height = rows.saturating_sub(2).max(1);
        self.cols = cols;
        self.top = self.top.min(self.max_top());
    }

    // The one file a deciding view is about, its hunks and their
    // verdicts.
    fn decided(&self) -> (&FileDiff, Vec<Hunk>, Vec<Verdict>) {
        let hunks = self.hunks.iter().filter(|(f, _)| *f == 0).map(|(_, h)| h.clone()).collect();
        let verdicts = self.hunks.iter().zip(&self.verdicts).filter(|((f, _), _)| *f == 0).map(|(_, v)| *v).collect();
        (&self.files[0], hunks, verdicts)
    }

    /// The text the verdicts add up to, with `undecided` saying which way
    /// a change nobody ruled on goes -- see `resolve`.
    pub fn resolved(&self, undecided: Verdict) -> Vec<String> {
        let (file, hunks, verdicts) = self.decided();
        resolve(&file.old, &file.new, &hunks, &verdicts, undecided)
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
        let (file, hunks, verdicts) = self.decided();
        hunks
            .iter()
            .zip(&verdicts)
            .rev()
            .filter(|(_, v)| **v == Verdict::Rejected)
            .map(|(h, _)| (h.new.clone(), file.old[h.old.clone()].to_vec()))
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

    // A file's heading at the top of the screen, and its first change the
    // current one, so `n` carries on from there.
    fn go_to_file(&mut self, file: usize) {
        // Not held to `max_top`: the last file's heading goes to the top
        // like any other, with blank rows under it, so which file is on
        // screen is never in doubt. The next scroll pulls it back.
        let Some(row) = self.rows.iter().position(|r| *r == Row::File(file)) else { return };
        self.top = row;
        if let Some(hunk) = self.hunks.iter().position(|(f, _)| *f == file) {
            self.current = hunk;
        }
    }

    // The file whose heading is nearest above the top of the screen.
    fn file_on_screen(&self) -> Option<usize> {
        self.rows.iter().take(self.top + 1).rev().find_map(|r| match r {
            Row::File(f) => Some(*f),
            _ => None,
        })
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
            Key::Char('}') if self.mode == Mode::Reading => {
                let next = self.file_on_screen().map_or(0, |f| f + 1);
                self.go_to_file(next);
            }
            Key::Char('{') if self.mode == Mode::Reading => {
                // Back to this file's own heading first, when it has
                // scrolled off; from the heading, to the file before.
                match self.file_on_screen() {
                    Some(f) if self.rows.get(self.top) == Some(&Row::File(f)) && f > 0 => self.go_to_file(f - 1),
                    Some(f) => self.go_to_file(f),
                    None => self.top = 0,
                }
            }
            Key::Char('q') | Key::Enter => return Outcome::Done,
            Key::Escape if self.mode == Mode::Reading => return Outcome::Done,
            Key::Escape => return Outcome::Cancel,
            _ if self.mode == Mode::Reading => {}
            Key::Char('a') | Key::Char('y') => self.decide(Verdict::Accepted),
            Key::Char('r') | Key::Char('x') => self.decide(Verdict::Rejected),
            Key::Char('u') => {
                if let Some(v) = self.verdicts.get_mut(self.current) {
                    *v = Verdict::Undecided;
                }
            }
            Key::Char('A') => self.decide_rest(Verdict::Accepted),
            Key::Char('R') => self.decide_rest(Verdict::Rejected),
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
            Row::Message(i) => (if i == 0 { "33" } else { "" }, self.header[i].clone()),
            Row::Blank => ("", String::new()),
            Row::File(f) => ("1", self.files[f].label.clone()),
            Row::Binary(_) => ("2", "  binary file, not shown".to_string()),
            Row::Unchanged(_) => ("2", "  nothing in it changed".to_string()),
            Row::Context { file, new, .. } => return self.render_line(file, 1, new, "  ", Color::Default, false),
            Row::Skipped(n) => ("2", format!("  \u{22ef} {n} unchanged line{}", if n == 1 { "" } else { "s" })),
            Row::Header(hunk) => {
                let h = &self.hunks[hunk].1;
                let label = match verdict(hunk) {
                    Verdict::Undecided => "",
                    Verdict::Accepted => "  accepted",
                    Verdict::Rejected => "  rejected",
                };
                let text = format!("@@ -{},{} +{},{} @@{label}", h.old.start + 1, h.old.len(), h.new.start + 1, h.new.len());
                (if hunk == self.current { "1;7" } else { "36" }, text)
            }
            Row::Removed { file, line, hunk } => {
                return self.render_line(file, 0, line, "- ", Color::Indexed(1), verdict(hunk) == Verdict::Accepted);
            }
            Row::Added { file, line, hunk } => {
                return self.render_line(file, 1, line, "+ ", Color::Indexed(2), verdict(hunk) == Verdict::Rejected);
            }
        };
        let fitted = fit(&text, self.cols);
        if sgr.is_empty() { fitted } else { format!("\x1b[{sgr}m{fitted}\x1b[0m") }
    }

    // A line of a file, `side` 0 for its old text and 1 for its new, after
    // `marker`. The marker and whatever the language leaves uncoloured are
    // in `base` -- red for a removed line, green for an added one -- so
    // which is which still reads at a glance with the syntax coloured
    // in. `gone` is a line the verdicts leave out: dimmed and struck
    // through, whatever colour it is.
    fn render_line(&self, file: usize, side: usize, line: usize, marker: &str, base: Color, gone: bool) -> String {
        let text = if side == 0 { &self.files[file].old[line] } else { &self.files[file].new[line] };
        let chars: Vec<char> = marker.chars().chain(text.chars()).collect();
        let offset = marker.chars().count();
        let whole = [StyledSpan { start: 0, end: chars.len(), fg: base, attrs: CellAttrs::default() }];
        let syntax: Vec<StyledSpan> = self
            .paint
            .get(file)
            .and_then(|sides| sides[side].get(line))
            .into_iter()
            .flatten()
            .map(|s| StyledSpan {
                start: s.start + offset,
                end: s.end + offset,
                fg: if s.fg == Color::Default { base } else { s.fg },
                attrs: s.attrs,
            })
            .collect();
        let mut cells = highlight::compose(&chars, &[&whole, &syntax]);
        if gone {
            let struck = CellAttrs { dim: true, strikethrough: true, ..CellAttrs::default() };
            let end = cells.len();
            highlight::compose_attrs(&mut cells, &[StyledSpan { start: 0, end, fg: base, attrs: struck }]);
        }
        highlight::render_styled(&fit_cells(&cells, self.cols))
    }

    fn status(&self) -> String {
        if self.mode == Mode::Reading {
            let file = match self.file_on_screen() {
                Some(f) => format!("file {} of {}: {}   ", f + 1, self.files.len(), self.files[f].label),
                None => format!("{} file{}   ", self.files.len(), if self.files.len() == 1 { "" } else { "s" }),
            };
            return format!("{file}j/k scroll  n/N change  {{/}} file  q close");
        }
        let count = |v: Verdict| self.verdicts.iter().filter(|x| **x == v).count();
        format!(
            "{} accepted, {} rejected, {} to go   a accept  r reject  A/R the rest  n/N change  q done  Esc cancel",
            count(Verdict::Accepted),
            count(Verdict::Rejected),
            count(Verdict::Undecided)
        )
    }
}

// The rows of the unified view. A reading view starts with its header
// and puts each file under a heading; a deciding view is one file and
// needs neither. In both, every change gets `CONTEXT` unchanged lines
// either side of it, and whatever unchanged run is longer than that is
// folded into one row saying how long.
fn layout(mode: Mode, header: &[String], files: &[FileDiff], hunks: &[(usize, Hunk)]) -> Vec<Row> {
    let mut rows: Vec<Row> = (0..header.len()).map(Row::Message).collect();
    let unchanged = |rows: &mut Vec<Row>, file: usize, old_at: usize, new_at: usize, len: usize, first: bool, last: bool| {
        let head = if first { 0 } else { len.min(CONTEXT) };
        let tail = if last { 0 } else { len.saturating_sub(head).min(CONTEXT) };
        for k in 0..head {
            rows.push(Row::Context { file, old: old_at + k, new: new_at + k });
        }
        if len > head + tail {
            rows.push(Row::Skipped(len - head - tail));
        }
        for k in len - tail..len {
            rows.push(Row::Context { file, old: old_at + k, new: new_at + k });
        }
    };
    for (f, file) in files.iter().enumerate() {
        if mode == Mode::Reading {
            if !rows.is_empty() {
                rows.push(Row::Blank);
            }
            rows.push(Row::File(f));
        }
        if file.binary {
            rows.push(Row::Binary(f));
            continue;
        }
        // Where the unchanged run before the next change starts, in both
        // texts. The two only ever differ by how much the changes so far
        // added or removed.
        let (mut old_at, mut new_at) = (0, 0);
        let mut any = false;
        for (i, (_, hunk)) in hunks.iter().enumerate().filter(|(_, (hf, _))| *hf == f) {
            unchanged(&mut rows, f, old_at, new_at, hunk.old.start - old_at, !any, false);
            rows.push(Row::Header(i));
            rows.extend(hunk.old.clone().map(|line| Row::Removed { file: f, line, hunk: i }));
            rows.extend(hunk.new.clone().map(|line| Row::Added { file: f, line, hunk: i }));
            (old_at, new_at) = (hunk.old.end, hunk.new.end);
            any = true;
        }
        if any {
            unchanged(&mut rows, f, old_at, new_at, file.old.len() - old_at, false, true);
        } else if mode == Mode::Reading {
            rows.push(Row::Unchanged(f));
        }
    }
    rows
}

// One side of a file's syntax colours, a list of spans per line, in
// character offsets from the start of that line. A span that runs over
// several lines -- a block comment, a string with a newline in it -- is
// cut into a piece on each.
fn paint(language: &str, lines: &[String], overrides: Option<&ColorOverrides>) -> Vec<Vec<StyledSpan>> {
    let mut out = vec![Vec::new(); lines.len()];
    let Some(highlighter) = highlight::highlighter_for_language(language) else { return out };
    let lengths: Vec<usize> = lines.iter().map(|l| l.chars().count()).collect();
    let mut starts = Vec::with_capacity(lines.len());
    let mut at = 0;
    for len in &lengths {
        starts.push(at);
        at += len + 1;
    }
    for span in highlighter.highlight(&lines.join("\n"), HighlightContext::default()) {
        let (fg, attrs) = highlight::resolve_style(span.kind, overrides);
        let mut line = starts.partition_point(|&start| start <= span.start).saturating_sub(1);
        while line < lines.len() && starts[line] < span.end {
            let start = span.start.saturating_sub(starts[line]).min(lengths[line]);
            let end = (span.end - starts[line]).min(lengths[line]);
            if start < end {
                out[line].push(StyledSpan { start, end, fg, attrs });
            }
            line += 1;
        }
    }
    out
}

// `fit`, for cells already styled: a tab is four spaces in the style of
// the tab, and the row is padded out in the terminal's own colours.
fn fit_cells(cells: &[Cell], cols: usize) -> Vec<Cell> {
    let mut out = Vec::new();
    let mut width = 0;
    for cell in cells.iter().flat_map(|c| if c.ch == '\t' { vec![Cell { ch: ' ', ..*c }; 4] } else { vec![*c] }) {
        let w = char_width(cell.ch);
        if width + w > cols {
            break;
        }
        width += w;
        out.push(cell);
    }
    out.extend(std::iter::repeat_n(Cell::default(), cols - width));
    out
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

    fn file(label: &str, old: &str, new: &str) -> FileDiff {
        FileDiff { label: label.to_string(), old: lines(old), new: lines(new), binary: false, language: "text".to_string() }
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
        let mut review = Review::new("t", old.clone(), new.clone(), "text", 20, 40);
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
        let review = Review::new("t", old, new, "text", 40, 40);
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
        let mut review = Review::new("t", old, new, "text", 40, 40);
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
        let mut review = Review::new("t", old, new, "text", 12, 40);
        review.handle_key(Key::Char('n'));
        assert!(shown(&review).iter().any(|row| row == "+ one fifty"), "{:?}", shown(&review));
    }

    #[test]
    fn the_frame_says_where_you_are_and_what_is_left() {
        let review = Review::new("file.rs", lines("a\nb"), lines("a\nc"), "text", 10, 120);
        let frame = plain(&review.render(Rect { row: 0, col: 0, rows: 10, cols: 120 }));
        assert!(frame.contains("file.rs  change 1 of 1"), "{frame}");
        assert!(frame.contains("0 accepted, 0 rejected, 1 to go"), "{frame}");
    }

    #[test]
    fn a_reading_view_shows_the_header_then_every_file_under_its_own_heading() {
        let files = vec![
            file("src/a.rs", "one\ntwo", "one\nTWO"),
            FileDiff { label: "logo.png".to_string(), old: Vec::new(), new: Vec::new(), binary: true, language: "text".to_string() },
            file("docs/b.md -> docs/c.md", "same", "same"),
        ];
        let review = Review::reading("git show abc", lines("commit abc\nAuthor: x"), files, None, 40, 60);
        assert_eq!(
            shown(&review),
            vec![
                "commit abc",
                "Author: x",
                "",
                "src/a.rs",
                "  one",
                "@@ -2,1 +2,1 @@",
                "- two",
                "+ TWO",
                "",
                "logo.png",
                "  binary file, not shown",
                "",
                "docs/b.md -> docs/c.md",
                "  nothing in it changed"
            ]
        );
    }

    #[test]
    fn a_reading_view_steps_through_changes_across_files_and_decides_nothing() {
        let files = vec![file("a", "1\n2", "1\nX"), file("b", "3\n4", "Y\n4")];
        let mut review = Review::reading("t", Vec::new(), files, None, 40, 40);
        assert_eq!(review.current, 0);
        review.handle_key(Key::Char('n'));
        assert_eq!(review.current, 1, "on into the next file's change");
        assert_eq!(review.hunks[1].0, 1);
        review.handle_key(Key::Char('a'));
        review.handle_key(Key::Char('r'));
        assert!(review.verdicts.iter().all(|v| *v == Verdict::Undecided), "nothing here to decide");
        assert_eq!(review.handle_key(Key::Escape), Outcome::Done, "Escape closes it too; there is nothing to cancel");
    }

    #[test]
    fn a_reading_view_opens_at_the_file_asked_for_and_jumps_between_files() {
        let many = |n: usize| (0..n).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let files = vec![file("first", &many(30), &(many(30) + "\nmore")), file("second", "x", "y"), file("third", "p", "q")];
        let mut review = Review::reading("t", lines("commit abc"), files, Some(1), 8, 40);
        assert_eq!(shown(&review)[0], "second", "its heading at the top");
        assert_eq!(review.current, 1, "and its first change the current one");
        review.handle_key(Key::Char('}'));
        assert_eq!(shown(&review)[0], "third");
        review.handle_key(Key::Char('{'));
        assert_eq!(shown(&review)[0], "second");
        let frame = plain(&review.render(Rect { row: 0, col: 0, rows: 8, cols: 60 }));
        assert!(frame.contains("file 2 of 3: second"), "{frame}");
    }

    #[test]
    fn a_line_is_in_its_language_s_colours_even_when_what_colours_it_started_above() {
        let old = lines("/* a comment\nstill it\n*/\nfn main() {}");
        let new = lines("/* a comment\nstill it, changed\n*/\nfn main() { let x = 1; }");
        let review = Review::new("t.rs", old, new, "rust", 40, 80).highlighted(None);
        let row = |wanted: &str| {
            let row = review.rows.iter().find(|r| plain(&review.render_row(**r)).trim_end() == wanted).unwrap_or_else(|| panic!("no row {wanted:?}"));
            review.render_row(*row)
        };
        let (comment, _) = highlight::default_style(highlight::HighlightKind::Comment);
        let (keyword, _) = highlight::default_style(highlight::HighlightKind::Keyword);
        let sgr = |fg: Color| {
            crate::vt100::sgr_codes(fg, Color::Default, CellAttrs::default()).trim_end_matches('m').rsplit(';').next().unwrap().to_string()
        };
        assert!(
            row("+ still it, changed").contains(&format!("{}m", sgr(comment))),
            "inside the comment opened above: {:?}",
            row("+ still it, changed")
        );
        let added = row("+ fn main() { let x = 1; }");
        assert!(added.contains(&format!("{}mfn", sgr(keyword))), "the keyword in its own colour: {added:?}");
        assert!(added.starts_with("\x1b[0;32m+ "), "the marker still says it was added: {added:?}");
        let plain_review = Review::new("t", lines("a"), lines("b"), "text", 10, 40).highlighted(None);
        let removed = plain_review.render_row(Row::Removed { file: 0, line: 0, hunk: 0 });
        assert!(removed.starts_with("\x1b[0;31m- a"), "no language, the whole line red as before: {removed:?}");
    }
}
