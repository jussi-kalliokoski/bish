//! Sorting with the sort algorithm compiled once.
//!
//! `slice::sort_by` is instantiated per closure type, so every call site
//! carries its own copy of the whole algorithm. These sort a vector of
//! indices through a `dyn` comparator instead, then permute the elements
//! into that order. Stable, like `slice::sort`.
//!
//! The guard test at the bottom fails if a slice sort appears anywhere else.

use std::cmp::Ordering;

pub(crate) fn sort<T: Ord>(items: &mut [T]) {
    sort_by(items, |a, b| a.cmp(b));
}

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

pub(crate) fn sort_by_key<T, K: Ord>(items: &mut [T], mut key: impl FnMut(&T) -> K) {
    sort_by(items, |a, b| key(a).cmp(&key(b)));
}

/// Builds a `BTreeMap` by inserting; `collect()` would sort the input
/// first, with the slice sort instantiated for its element type.
pub(crate) fn btree_map<K: Ord, V>(items: impl IntoIterator<Item = (K, V)>) -> std::collections::BTreeMap<K, V> {
    let mut map = std::collections::BTreeMap::new();
    map.extend(items);
    map
}

/// Builds a `BTreeSet` by inserting, for the same reason as `btree_map`.
pub(crate) fn btree_set<T: Ord>(items: impl IntoIterator<Item = T>) -> std::collections::BTreeSet<T> {
    let mut set = std::collections::BTreeSet::new();
    set.extend(items);
    set
}

/// The one instantiation of the standard sort.
#[inline(never)]
fn order(len: usize, compare: &mut dyn FnMut(usize, usize) -> Ordering) -> Vec<usize> {
    let mut order: Vec<usize> = (0..len).collect();
    order.sort_by(|&a, &b| compare(a, b));
    order
}

/// Moves `items` so that position `i` holds what was at `order[i]`,
/// walking each cycle of the permutation once.
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

    /// Deterministic, so a failure reproduces.
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
        // The second field is each element's original position.
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

    /// Reads the source as text, skipping test modules. `self.sort()` is
    /// let through because it is `Folds::sort`. A `collect()` into a
    /// B-tree sorts too but can't be seen here; that's what `btree_map` and
    /// `btree_set` are for.
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
                let mut in_tests = false;
                let mut previous = "";
                for (number, line) in source.lines().enumerate() {
                    // A test module runs from `#[cfg(test)]` + `mod name {`
                    // to the `}` rustfmt puts alone in column 1.
                    if !in_tests && previous.trim() == "#[cfg(test)]" && line.trim_start().starts_with("mod ") && line.trim_end().ends_with('{') {
                        in_tests = true;
                    }
                    let skip = in_tests || line.trim_start().starts_with("//");
                    if in_tests && line == "}" {
                        in_tests = false;
                    }
                    previous = line;
                    if skip {
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
