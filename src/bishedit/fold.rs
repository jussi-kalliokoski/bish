// Folds: ranges of lines the editor shows as one row.
//
// vim's manual folds, which is what `vim -u NONE` has and so what the
// editor corpus can hold this to. A fold is a range of lines, open or
// closed. Folds nest, and a closed fold hides everything inside it --
// folds of its own included -- behind the one row that stands for it.
//
// Line-indexed state, so it has to move when lines do: see `remap`, and
// `TextBuffer`'s content mutators, which are what call it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    pub start: usize,
    pub end: usize,
    pub closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folds {
    // Outermost first: by start, then the longer of two that start on
    // the same line. `containing` relies on that order.
    list: Vec<Fold>,
    // `zi`. Off, every fold still exists and none of them hides
    // anything, which is what makes it a toggle rather than a `zE`.
    pub enabled: bool,
}

impl Default for Folds {
    fn default() -> Folds {
        Folds { list: Vec::new(), enabled: true }
    }
}

impl Folds {
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Fold> {
        self.list.iter()
    }

    fn sort(&mut self) {
        self.list.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        self.list.dedup_by(|later, earlier| later.start == earlier.start && later.end == earlier.end);
    }

    /// `zf`, `zF`, `:fold`: a fold over `start..=end`, closed -- which is
    /// what a new fold is in vim with the default `foldlevel` of 0.
    pub fn create(&mut self, start: usize, end: usize) {
        let (start, end) = (start.min(end), start.max(end));
        match self.list.iter_mut().find(|f| f.start == start && f.end == end) {
            Some(existing) => existing.closed = true,
            None => {
                self.list.push(Fold { start, end, closed: true });
                self.sort();
            }
        }
    }

    // Outermost first, by the list's own order.
    fn containing(&self, line: usize) -> impl Iterator<Item = &Fold> {
        self.list.iter().filter(move |f| f.start <= line && line <= f.end)
    }

    fn indices_containing(&self, line: usize) -> Vec<usize> {
        (0..self.list.len()).filter(|&i| self.list[i].start <= line && line <= self.list[i].end).collect()
    }

    /// The closed fold hiding `line`, if one is: the outermost, since a
    /// closed fold hides every fold inside it.
    pub fn closed_at(&self, line: usize) -> Option<(usize, usize)> {
        if !self.enabled {
            return None;
        }
        self.containing(line).find(|f| f.closed).map(|f| (f.start, f.end))
    }

    /// The lines that show as the one row `line` is drawn on.
    pub fn span(&self, line: usize) -> (usize, usize) {
        self.closed_at(line).unwrap_or((line, line))
    }

    /// Whether anything is hidden at all -- the question that decides
    /// whether counting rows can be done by subtracting line numbers.
    pub fn any_closed(&self) -> bool {
        self.enabled && self.list.iter().any(|f| f.closed)
    }

    /// How deeply the fold over `start..=end` sits, 1 for one inside no
    /// other: the number of dashes vim draws on its row.
    pub fn level(&self, start: usize, end: usize) -> usize {
        self.list.iter().filter(|f| f.start <= start && end <= f.end).count().max(1)
    }

    /// `zo`: opens `count` levels of what hides `line`, outermost first.
    pub fn open(&mut self, line: usize, count: usize) {
        for _ in 0..count.max(1) {
            let Some(i) = self.indices_containing(line).into_iter().find(|&i| self.list[i].closed) else { return };
            self.list[i].closed = false;
        }
    }

    /// `zO`, `zv`: every fold `line` is in, open, so `line` shows.
    pub fn open_all(&mut self, line: usize) {
        for i in self.indices_containing(line) {
            self.list[i].closed = false;
        }
    }

    /// `zc`: closes one level more around `line`. On a line no fold
    /// hides that is the innermost fold holding it; on a hidden line it
    /// is the open fold around the one hiding it.
    pub fn close(&mut self, line: usize, count: usize) {
        for _ in 0..count.max(1) {
            let chain = self.indices_containing(line);
            let visible = match chain.iter().position(|&i| self.list[i].closed) {
                Some(hider) => &chain[..hider],
                None => &chain[..],
            };
            let Some(&i) = visible.last() else { return };
            self.list[i].closed = true;
        }
    }

    /// `zC`: every fold `line` is in, closed.
    pub fn close_all(&mut self, line: usize) {
        for i in self.indices_containing(line) {
            self.list[i].closed = true;
        }
    }

    /// `za`: open what hides `line`, or close around it if nothing does.
    pub fn toggle(&mut self, line: usize, count: usize) {
        if self.closed_at(line).is_some() { self.open(line, count) } else { self.close(line, count) }
    }

    /// `zA`: `za`, all the way in or out.
    pub fn toggle_all(&mut self, line: usize) {
        if self.closed_at(line).is_some() { self.open_all(line) } else { self.close_all(line) }
    }

    /// `:{range}foldopen`: one level of every fold the range reaches --
    /// what `zo` would open on each of its lines, decided before any of
    /// them opens, so a long range does not peel a nest several deep.
    /// `all` (the `!`) opens every fold the range touches.
    pub fn open_range(&mut self, first: usize, last: usize, all: bool) {
        let targets: Vec<usize> = if all {
            (0..self.list.len()).filter(|&i| self.list[i].start <= last && first <= self.list[i].end).collect()
        } else {
            (first..=last).filter_map(|l| self.indices_containing(l).into_iter().find(|&i| self.list[i].closed)).collect()
        };
        for i in targets {
            self.list[i].closed = false;
        }
    }

    /// `:{range}foldclose`: what `zc` would close on each line of the
    /// range, decided up front the same way; `all` closes every fold it
    /// touches.
    pub fn close_range(&mut self, first: usize, last: usize, all: bool) {
        let targets: Vec<usize> = if all {
            (0..self.list.len()).filter(|&i| self.list[i].start <= last && first <= self.list[i].end).collect()
        } else {
            (first..=last)
                .filter_map(|l| {
                    let chain = self.indices_containing(l);
                    let visible = match chain.iter().position(|&i| self.list[i].closed) {
                        Some(hider) => &chain[..hider],
                        None => &chain[..],
                    };
                    visible.last().copied()
                })
                .collect()
        };
        for i in targets {
            self.list[i].closed = true;
        }
    }

    /// `zR` (`false`) and `zM` (`true`).
    pub fn set_all(&mut self, closed: bool) {
        for f in &mut self.list {
            f.closed = closed;
        }
    }

    /// `zd`: the fold hiding `line`, or the innermost one holding it.
    /// What was inside it stays, one level further out.
    pub fn delete(&mut self, line: usize) {
        let chain = self.indices_containing(line);
        let target = match chain.iter().find(|&&i| self.list[i].closed && self.enabled) {
            Some(&i) => i,
            None => match chain.last() {
                Some(&i) => i,
                None => return,
            },
        };
        self.list.remove(target);
    }

    /// `zD`: the fold hiding `line` and every fold inside it, or, on a
    /// line nothing hides, every fold holding it.
    pub fn delete_all(&mut self, line: usize) {
        match self.closed_at(line) {
            Some((start, end)) => self.list.retain(|f| !(start <= f.start && f.end <= end)),
            None => self.list.retain(|f| !(f.start <= line && line <= f.end)),
        }
    }

    /// `zE`.
    pub fn clear(&mut self) {
        self.list.clear();
    }

    /// `zj`: where the next fold below `line` starts, a closed fold
    /// counting as one.
    pub fn next_start(&self, line: usize) -> Option<usize> {
        let after = self.span(line).1;
        self.list.iter().map(|f| f.start).filter(|&s| s > after).min()
    }

    /// `zk`: where the nearest fold above `line` ends.
    pub fn previous_end(&self, line: usize) -> Option<usize> {
        let before = self.span(line).0;
        self.list.iter().map(|f| f.end).filter(|&e| e < before).max()
    }

    /// `[z` (`end == false`) and `]z`: the start or end of the open fold
    /// `line` is in -- or, already there, of the one around that.
    pub fn edge(&self, line: usize, end: bool) -> Option<usize> {
        let mut chain: Vec<&Fold> = self.containing(line).filter(|f| !f.closed || !self.enabled).collect();
        chain.reverse();
        chain.into_iter().map(|f| if end { f.end } else { f.start }).find(|&at| at != line)
    }

    /// Moves every fold the way its lines moved. `map` says where a line
    /// of the old text is now, `None` for one that is gone. A fold keeps
    /// whichever of its lines survived -- lines inserted between two of
    /// them are inside it by the same token -- and goes when none did.
    pub fn remap(&mut self, map: impl Fn(usize) -> Option<usize>) {
        self.list.retain_mut(|f| {
            let first = (f.start..=f.end).find_map(&map);
            let last = (f.start..=f.end).rev().find_map(&map);
            match (first, last) {
                (Some(start), Some(end)) => {
                    (f.start, f.end) = (start, end);
                    true
                }
                _ => false,
            }
        });
        self.sort();
    }

    /// `count` new lines now start at `at`; what was there moved down.
    pub fn lines_inserted(&mut self, at: usize, count: usize) {
        if count > 0 && !self.list.is_empty() {
            self.remap(|l| Some(if l < at { l } else { l + count }));
        }
    }

    /// `count` lines starting at `first` are gone.
    pub fn lines_deleted(&mut self, first: usize, count: usize) {
        if count > 0 && !self.list.is_empty() {
            self.remap(|l| {
                if l < first {
                    Some(l)
                } else if l < first + count {
                    None
                } else {
                    Some(l - count)
                }
            });
        }
    }

    /// The text changed some other way -- undo, redo, a reload -- with no
    /// record of which lines moved, so the two versions are diffed to
    /// find out. A line replaced rather than removed keeps its place,
    /// which is what keeps an undone `x` on a fold's last line from
    /// shrinking the fold.
    pub fn text_replaced<T: PartialEq>(&mut self, old: &[T], new: &[T]) {
        if self.list.is_empty() {
            return;
        }
        let mut map: Vec<Option<usize>> = vec![None; old.len()];
        let ops = crate::diff::diff(old, new);
        let mut i = 0;
        while i < ops.len() {
            match ops[i] {
                crate::diff::DiffOp::Equal { a, b, len } => {
                    for k in 0..len {
                        map[a + k] = Some(b + k);
                    }
                }
                crate::diff::DiffOp::Delete { a, len } => {
                    if let Some(crate::diff::DiffOp::Insert { b, len: inserted }) = ops.get(i + 1) {
                        for k in 0..len.min(*inserted) {
                            map[a + k] = Some(b + k);
                        }
                        i += 1;
                    }
                }
                crate::diff::DiffOp::Insert { .. } => {}
            }
            i += 1;
        }
        self.remap(|l| map.get(l).copied().flatten());
    }
}

/// What a closed fold's row says: vim's own `foldtext()`, dashes for the
/// level, the count, and the first line with its indentation dropped.
pub fn fold_text(level: usize, lines: usize, first_line: &str) -> String {
    let noun = if lines == 1 { "line" } else { "lines" };
    format!("+-{}{:>3} {noun}: {}", "-".repeat(level), lines, first_line.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folds(ranges: &[(usize, usize, bool)]) -> Folds {
        let mut f = Folds::default();
        for &(start, end, closed) in ranges {
            f.create(start, end);
            if !closed {
                f.list.iter_mut().find(|x| x.start == start && x.end == end).unwrap().closed = false;
            }
        }
        f
    }

    fn ranges(f: &Folds) -> Vec<(usize, usize, bool)> {
        f.iter().map(|x| (x.start, x.end, x.closed)).collect()
    }

    #[test]
    fn a_closed_fold_hides_its_lines_behind_its_first() {
        let f = folds(&[(2, 4, true)]);
        assert_eq!(f.span(1), (1, 1));
        assert_eq!(f.span(3), (2, 4));
        assert_eq!(f.span(4), (2, 4));
        assert_eq!(f.span(5), (5, 5));
    }

    #[test]
    fn the_outermost_closed_fold_is_the_one_that_shows() {
        let f = folds(&[(0, 9, true), (2, 4, true)]);
        assert_eq!(f.span(3), (0, 9));
        let f = folds(&[(0, 9, false), (2, 4, true)]);
        assert_eq!(f.span(3), (2, 4));
        assert_eq!(f.span(6), (6, 6));
    }

    #[test]
    fn turning_folding_off_hides_nothing_and_forgets_nothing() {
        let mut f = folds(&[(2, 4, true)]);
        f.enabled = false;
        assert_eq!(f.span(3), (3, 3));
        f.enabled = true;
        assert_eq!(f.span(3), (2, 4));
    }

    #[test]
    fn zo_and_zc_go_one_level_at_a_time() {
        let mut f = folds(&[(0, 9, false), (2, 4, false)]);
        f.close(3, 1);
        assert_eq!(ranges(&f), vec![(0, 9, false), (2, 4, true)], "the innermost first");
        f.close(3, 1);
        assert_eq!(ranges(&f), vec![(0, 9, true), (2, 4, true)], "then the one around it");
        f.open(3, 1);
        assert_eq!(ranges(&f), vec![(0, 9, false), (2, 4, true)], "opening starts from the outside");
        f.open_all(3);
        assert_eq!(ranges(&f), vec![(0, 9, false), (2, 4, false)]);
    }

    #[test]
    fn za_opens_a_hidden_line_and_closes_around_a_shown_one() {
        let mut f = folds(&[(2, 4, true)]);
        f.toggle(3, 1);
        assert_eq!(f.span(3), (3, 3));
        f.toggle(3, 1);
        assert_eq!(f.span(3), (2, 4));
    }

    #[test]
    fn zd_takes_one_fold_and_leaves_what_it_held() {
        let mut f = folds(&[(0, 9, true), (2, 4, true)]);
        f.delete(3);
        assert_eq!(ranges(&f), vec![(2, 4, true)], "the closed one hiding the line");
        let mut f = folds(&[(0, 9, false), (2, 4, false)]);
        f.delete(3);
        assert_eq!(ranges(&f), vec![(0, 9, false)], "or the innermost, when nothing hides it");
        let mut f = folds(&[(0, 9, true), (2, 4, true), (12, 14, true)]);
        f.delete_all(3);
        assert_eq!(ranges(&f), vec![(12, 14, true)]);
    }

    #[test]
    fn zj_and_zk_count_a_closed_fold_as_one() {
        let f = folds(&[(2, 6, true), (3, 4, true), (8, 9, false)]);
        assert_eq!(f.next_start(0), Some(2));
        assert_eq!(f.next_start(2), Some(8), "the fold inside the closed one is not a stop");
        assert_eq!(f.previous_end(8), Some(6));
        assert_eq!(f.next_start(8), None);
    }

    #[test]
    fn square_bracket_z_finds_the_edges_of_the_open_fold_around() {
        let f = folds(&[(0, 9, false), (2, 4, false)]);
        assert_eq!(f.edge(3, false), Some(2));
        assert_eq!(f.edge(2, false), Some(0), "already at its start: the one around it");
        assert_eq!(f.edge(3, true), Some(4));
        assert_eq!(f.edge(12, true), None);
    }

    #[test]
    fn lines_inserted_inside_a_fold_grow_it_and_above_it_move_it() {
        let mut f = folds(&[(2, 4, true)]);
        f.lines_inserted(3, 2);
        assert_eq!(ranges(&f), vec![(2, 6, true)]);
        f.lines_inserted(2, 1);
        assert_eq!(ranges(&f), vec![(3, 7, true)], "at its first line is above it");
        f.lines_inserted(8, 5);
        assert_eq!(ranges(&f), vec![(3, 7, true)], "after its last line is not in it");
    }

    #[test]
    fn deleting_lines_shrinks_a_fold_and_deleting_all_of_them_removes_it() {
        let mut f = folds(&[(2, 4, true), (8, 9, true)]);
        f.lines_deleted(0, 3);
        assert_eq!(ranges(&f), vec![(0, 1, true), (5, 6, true)]);
        f.lines_deleted(0, 2);
        assert_eq!(ranges(&f), vec![(3, 4, true)]);
    }

    #[test]
    fn an_undone_change_moves_folds_by_what_the_text_shows() {
        let old = ["a", "b", "c", "d", "e"];
        let mut f = folds(&[(2, 4, true)]);
        f.text_replaced(&old, &["a", "c", "d", "E"]);
        assert_eq!(ranges(&f), vec![(1, 3, true)], "`b` went, and `e` became `E` without leaving the fold");
        let mut f = folds(&[(2, 4, true)]);
        f.text_replaced(&old, &["new", "a", "b", "c", "d", "e"]);
        assert_eq!(ranges(&f), vec![(3, 5, true)]);
    }

    #[test]
    fn the_fold_row_says_how_deep_how_long_and_what_it_starts_with() {
        assert_eq!(fold_text(1, 3, "    fn main() {"), "+--  3 lines: fn main() {");
        assert_eq!(fold_text(2, 12, "x"), "+--- 12 lines: x");
    }
}
