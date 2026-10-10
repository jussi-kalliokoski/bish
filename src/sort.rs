//! Every sort in bish, compiled once.
//!
//! The standard library's slice sort is generic over the comparison as
//! well as the element: each call site's closure is a type of its own,
//! so each one gets its own copy of the whole algorithm -- run detection,
//! quicksort, merges, small-sort networks. Measured on 2026-10-10 that
//! was 265 functions and 131 KiB, 7% of the browser build's code, for 45
//! call sites sorting a few dozen things each.
//!
//! Here the algorithm runs on positions instead. `order` sorts the
//! indices `0..len` through a `dyn` comparator, so the standard sort is
//! instantiated exactly once, for `usize`; then the elements are moved
//! into that order in place. What each call site still compiles is the
//! closure that compares two positions and the cycle-following that
//! applies the order -- a few hundred bytes rather than several
//! kilobytes. The cost is an index vector and an indirect call per
//! comparison, which nothing bish sorts is large enough to notice.
//!
//! Stable, like `slice::sort`: indices start in their original order and
//! the sort that orders them is stable, so equal elements keep theirs.
//! Use these and not the slice methods -- the guard test at the bottom
//! fails the build when a slice sort appears anywhere else.

use std::cmp::Ordering;

/// Sorts `items` ascending.
pub(crate) fn sort<T: Ord>(items: &mut [T]) {
    sort_by(items, |a, b| a.cmp(b));
}

/// Sorts `items` by `compare`, keeping equal elements in their order.
pub(crate) fn sort_by<T>(items: &mut [T], mut compare: impl FnMut(&T, &T) -> Ordering) {
    if items.len() < 2 {
        return;
    }
    let order = {
        let items = &*items;
        order(items.len(), &mut |a, b| compare(&items[a], &items[b]))
    };
    apply(items, order);
}

/// Sorts `items` by the key `key` gives each, keeping equal keys in
/// their order.
pub(crate) fn sort_by_key<T, K: Ord>(items: &mut [T], mut key: impl FnMut(&T) -> K) {
    sort_by(items, |a, b| key(a).cmp(&key(b)));
}

/// The one instantiation of the standard sort: positions, compared
/// through whatever the caller says about the elements at them.
#[inline(never)]
fn order(len: usize, compare: &mut dyn FnMut(usize, usize) -> Ordering) -> Vec<usize> {
    let mut order: Vec<usize> = (0..len).collect();
    order.sort_by(|&a, &b| compare(a, b));
    order
}

/// Moves `items` so that position `i` holds what was at `order[i]`.
///
/// Each cycle of the permutation is walked once, swapping as it goes:
/// starting at `i`, swapping `i` with `order[i]` puts the right element
/// at `i` and carries the displaced one forward to where the cycle needs
/// it next. A visited position is marked so its cycle is not walked
/// twice.
fn apply<T>(items: &mut [T], mut order: Vec<usize>) {
    const DONE: usize = usize::MAX;
    for start in 0..items.len() {
        let mut at = start;
        while order[at] != DONE {
            let next = order[at];
            order[at] = DONE;
            if next == start {
                break;
            }
            items.swap(at, next);
            at = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator, so a failure reproduces.
    fn numbers(seed: u64, count: usize, range: u64) -> Vec<u64> {
        let mut state = seed;
        (0..count)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state % range
            })
            .collect()
    }

    #[test]
    fn sorts_the_same_as_the_standard_library() {
        for (seed, count, range) in [(1, 0, 5), (2, 1, 5), (3, 2, 5), (4, 17, 3), (5, 200, 1000), (6, 1000, 10), (7, 513, 1 << 40)] {
            let mut ours = numbers(seed, count, range);
            let mut theirs = ours.clone();
            sort(&mut ours);
            theirs.sort();
            assert_eq!(ours, theirs, "seed {seed}");
        }
    }

    #[test]
    fn equal_elements_keep_their_order() {
        // Sorted by key alone, the second field records where each one
        // started -- a stable sort leaves those ascending within a key.
        let keys = numbers(9, 300, 7);
        let mut tagged: Vec<(u64, usize)> = keys.iter().copied().zip(0..).collect();
        sort_by_key(&mut tagged, |&(key, _)| key);
        for pair in tagged.windows(2) {
            assert!(pair[0].0 < pair[1].0 || (pair[0].0 == pair[1].0 && pair[0].1 < pair[1].1), "{pair:?}");
        }
    }

    #[test]
    fn a_comparison_that_reverses_reverses() {
        let mut words = vec!["pear".to_string(), "apple".to_string(), "fig".to_string(), "apple".to_string()];
        sort_by(&mut words, |a, b| b.cmp(a));
        assert_eq!(words, ["pear", "fig", "apple", "apple"]);
    }

    /// The point of this module is that there is one copy of the sort in
    /// the binary; a slice sort anywhere else brings its own back. Read
    /// as text, like `platform::os_guard`, and only the part of each file
    /// before its tests -- test code is not in the binary, and may sort
    /// however it likes.
    ///
    /// `self.sort()` is not a slice method in this codebase (it is
    /// `Folds::sort`, which itself sorts through here), so a call on
    /// `self` is let through.
    #[test]
    fn nothing_outside_this_module_uses_a_slice_sort() {
        let methods = ["sort", "sort_by", "sort_by_key", "sort_by_cached_key", "sort_unstable", "sort_unstable_by", "sort_unstable_by_key"];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src];
        let mut found: Vec<String> = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("src is readable").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") || path.ends_with("sort.rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("a source file is readable");
                let shipped = source.split("\n#[cfg(test)]\nmod ").next().unwrap_or("");
                for (number, line) in shipped.lines().enumerate() {
                    if line.trim_start().starts_with("//") {
                        continue;
                    }
                    for method in methods {
                        let call = format!(".{method}(");
                        for (at, _) in line.match_indices(&call) {
                            if !line[..at].ends_with("self") {
                                found.push(format!("{}:{}: {}", path.display(), number + 1, line.trim()));
                            }
                        }
                    }
                }
            }
        }
        assert!(
            found.is_empty(),
            "slice sorts outside src/sort.rs -- use crate::sort::{{sort, sort_by, sort_by_key}} so the algorithm stays compiled once:\n  {}",
            found.join("\n  ")
        );
    }
}
