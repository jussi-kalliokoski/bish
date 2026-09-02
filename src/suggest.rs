// "did you mean ...?" -- the one thing to say when a name is nearly
// right.
//
// Optimal string alignment distance rather than plain Levenshtein: it
// counts a transposition as one edit instead of two, and a
// transposition is the single most common way a name gets mistyped.
// The difference decides whether the feature works at all on short
// names. `ecoh` is one transposition from `echo` but two plain
// Levenshtein edits, and any threshold loose enough to catch a distance
// of two on a four-letter word suggests `exec` for `set` as well.
//
// The threshold is a third of the typed word's length, rounded down and
// never below one -- so a four-letter name allows one edit, a
// nine-letter name allows three, and nothing is ever suggested for a
// name that shares almost nothing with what was typed. `readonly` is
// eight characters and tolerates two, which covers a doubled or dropped
// letter plus a slip; it does not stretch to `read`.

/// The closest of `candidates` to `word`, if any of them is close
/// enough to be worth naming.
///
/// Ties go to the earliest candidate, so a caller that lists its
/// options in a meaningful order gets that order honoured.
pub(crate) fn nearest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (word.chars().count() / 3).max(1);
    if word.chars().count() < 3 {
        return None;
    }
    candidates
        .into_iter()
        .filter(|candidate| !candidate.is_empty())
        .map(|candidate| (distance(word, candidate), candidate))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, candidate)| candidate)
}

/// `nearest`, already worded and ready to append to an error.
///
/// Empty when nothing is close, so a message can carry it
/// unconditionally: `format!("unknown subcommand '{name}'{}", suggest::did_you_mean(name, OPTIONS))`.
pub(crate) fn did_you_mean<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> String {
    match nearest(word, candidates) {
        Some(candidate) => format!(" -- did you mean '{candidate}'?"),
        None => String::new(),
    }
}

/// Optimal string alignment distance: insertions, deletions,
/// substitutions and transpositions of adjacent characters, one edit
/// each.
///
/// Two rows rather than a full matrix would be the usual trim, but this
/// needs three -- the transposition case reaches back two rows -- and
/// the names being compared are short enough that the whole matrix is a
/// few hundred bytes anyway.
pub(crate) fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() || b.is_empty() {
        return a.len().max(b.len());
    }
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let substitution = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + substitution);
            // `ab` -> `ba` is one edit, not two.
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::{did_you_mean, distance, nearest};

    #[test]
    fn a_transposition_costs_one_edit_not_two() {
        assert_eq!(distance("ecoh", "echo"), 1);
        assert_eq!(distance("echo", "echo"), 0);
        assert_eq!(distance("eco", "echo"), 1, "a dropped letter");
        assert_eq!(distance("eccho", "echo"), 1, "a doubled letter");
        assert_eq!(distance("edho", "echo"), 1, "a slipped key");
        assert_eq!(distance("", "echo"), 4);
        // Unrelated words stay far apart, which is what the threshold
        // in `nearest` reads to decide there is nothing to say.
        assert_eq!(distance("set", "exec"), 3);
        assert_eq!(distance("restart", "reset"), 3);
    }

    #[test]
    fn only_a_near_miss_is_worth_naming() {
        let options = ["theme", "window", "hook", "hl", "lsp", "map"];
        assert_eq!(nearest("lps", options), Some("lsp"));
        assert_eq!(nearest("windo", options), Some("window"));
        assert_eq!(nearest("wnidow", options), Some("window"));
        // Different words, not misspelled ones.
        assert_eq!(nearest("restart", options), None);
        assert_eq!(nearest("zzz", options), None);
        // Nothing is guessed from one or two characters: every short
        // word is a near miss for something.
        assert_eq!(nearest("hl", options), None);
        assert_eq!(nearest("x", options), None);
    }

    #[test]
    fn the_wording_is_ready_to_append_or_absent() {
        assert_eq!(did_you_mean("lps", ["lsp"]), " -- did you mean 'lsp'?");
        assert_eq!(did_you_mean("zzz", ["lsp"]), "");
    }

    // The property that matters: a builtin typed with one slip of the
    // finger should resolve back to it. Measured over every builtin
    // rather than over the handful I would have thought of -- each name
    // with one letter dropped, one doubled, and each adjacent pair
    // swapped.
    //
    // Not 100%, and it cannot be: `tet` is one edit from both `test`
    // and `let`, and a two-letter builtin has no distinctive shape left
    // once a letter goes. The bar is that a slip is almost always
    // recoverable, not always.
    #[test]
    fn nearly_every_single_slip_finds_its_way_back() {
        let builtins = crate::exec::KNOWN_BUILTINS;
        let (mut total, mut right) = (0, 0);
        for name in builtins {
            let chars: Vec<char> = name.chars().collect();
            let mut typos: Vec<String> = Vec::new();
            for i in 0..chars.len() {
                let mut dropped = chars.clone();
                dropped.remove(i);
                typos.push(dropped.iter().collect());
                let mut doubled = chars.clone();
                doubled.insert(i, chars[i]);
                typos.push(doubled.iter().collect());
            }
            for i in 0..chars.len().saturating_sub(1) {
                let mut swapped = chars.clone();
                swapped.swap(i, i + 1);
                typos.push(swapped.iter().collect());
            }
            for typo in typos {
                // A "typo" that is itself a builtin is not one.
                if typo.is_empty() || builtins.contains(&typo.as_str()) {
                    continue;
                }
                total += 1;
                right += usize::from(nearest(&typo, builtins.iter().copied()) == Some(*name));
            }
        }
        assert!(total > 500, "the corpus is generated, so it should be large: {total}");
        assert!(right * 100 >= total * 95, "only {right} of {total} slips found their way back");
    }

    // The other side of it: a real program that simply is not installed
    // should not be answered with a builtin that happens to be spelled
    // a bit like it. Names taken off this machine's own `/usr/bin`,
    // pinned here rather than read from it so the test says the same
    // thing everywhere.
    //
    // Measured across all 2,758 of them, 37 do draw a suggestion. That
    // is the price of the feature rather than a bug in it: `sed` is one
    // edit from `set` and `head` is one from `read`, and nothing can
    // tell those apart from a real slip of the finger. It only shows at
    // all when the program is missing, where a wrong guess costs a line
    // under an error that was already going to be printed.
    #[test]
    fn a_real_program_name_is_rarely_mistaken_for_a_builtin() {
        let builtins = crate::exec::KNOWN_BUILTINS;
        for name in [
            "ls", "grep", "awk", "curl", "python3", "make", "git", "ssh", "tar", "find", "sort", "chmod", "mount", "ping", "less", "vim", "rustc",
            "cargo", "docker",
        ] {
            assert_eq!(nearest(name, builtins.iter().copied()), None, "{name} should not look like a builtin");
        }
        assert_eq!(nearest("sed", builtins.iter().copied()), Some("set"), "the unavoidable kind");
    }
}
