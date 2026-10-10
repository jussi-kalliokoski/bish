// "Did you mean ...?" for a name that's nearly right.
//
// Optimal string alignment distance, not plain Levenshtein: a
// transposition, the most common typo, counts as one edit. On short
// names that decides whether it works at all: `ecoh` is one edit from
// `echo` but two in Levenshtein, and a threshold allowing two on a
// four-letter word suggests `exec` for `set`.
//
// The threshold is a third of the typed word's length, at least one: one
// edit for four letters, three for nine. `readonly` tolerates two, which
// doesn't stretch to `read`.

/// The closest of `candidates` to `word`, if close enough to name. Ties
/// go to the earliest, so a caller's ordering is honoured.
pub(crate) fn nearest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (word.chars().count() / 3).max(1);
    if word.chars().count() < 3 {
        return None;
    }
    candidates
        .into_iter()
        // Never the word itself: a name that's a builtin elsewhere but not
        // where it was typed got "did you mean 'win'?".
        .filter(|candidate| !candidate.is_empty() && *candidate != word)
        .map(|candidate| (distance(word, candidate), candidate))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, candidate)| candidate)
}

/// The bash builtins this shell answers differently, and what to use
/// instead. Not stubs: a `bind` that did nothing would report success
/// for keybindings not in effect. The editor isn't readline; its keymap
/// is `::bish map`.
///
/// `win` and `window` exist only on the editor's colon line (see their arm
/// in exec.rs), so at a prompt the useful answer is the spelling that
/// works there.
const INSTEAD: &[(&str, &str)] = &[
    ("bind", "this shell's keymap is `::bish map`"),
    ("win", "outside the editor's colon line it is `::bish window`"),
    ("window", "outside the editor's colon line it is `::bish window`"),
];

pub(crate) fn instead_of(word: &str) -> String {
    match INSTEAD.iter().find(|(name, _)| *name == word) {
        Some((_, advice)) => format!(" -- {advice}"),
        None => String::new(),
    }
}

/// `nearest`, worded to append to an error. Empty when nothing is close, so
/// a message can always carry it:
/// `format!("unknown subcommand '{name}'{}", suggest::did_you_mean(name, OPTIONS))`.
pub(crate) fn did_you_mean<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> String {
    match nearest(word, candidates) {
        Some(candidate) => format!(" -- did you mean '{candidate}'?"),
        None => String::new(),
    }
}

/// Optimal string alignment distance: insertions, deletions,
/// substitutions and adjacent transpositions, one edit each. The full
/// matrix, as the transposition case reaches back two rows and the names
/// are short.
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
            // `ab` to `ba` is one edit, not two.
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

    // A word that is one of the candidates isn't a near miss of it: `win`,
    // not found at a prompt, was offered back as the fix.
    #[test]
    fn a_word_is_never_suggested_to_itself() {
        assert_eq!(did_you_mean("win", ["win", "wait"]), "", "nothing else is close enough to say");
        assert_eq!(nearest("ecoh", ["ecoh", "echo"]), Some("echo"));
    }

    #[test]
    fn a_transposition_costs_one_edit_not_two() {
        assert_eq!(distance("ecoh", "echo"), 1);
        assert_eq!(distance("echo", "echo"), 0);
        assert_eq!(distance("eco", "echo"), 1, "a dropped letter");
        assert_eq!(distance("eccho", "echo"), 1, "a doubled letter");
        assert_eq!(distance("edho", "echo"), 1, "a slipped key");
        assert_eq!(distance("", "echo"), 4);
        // Unrelated words stay far apart, which `nearest`'s threshold relies
        // on.
        assert_eq!(distance("set", "exec"), 3);
        assert_eq!(distance("restart", "reset"), 3);
    }

    #[test]
    fn only_a_near_miss_is_worth_naming() {
        let options = ["theme", "window", "hook", "hl", "lsp", "map"];
        assert_eq!(nearest("lps", options), Some("lsp"));
        assert_eq!(nearest("windo", options), Some("window"));
        assert_eq!(nearest("wnidow", options), Some("window"));
        // Different words, not misspellings.
        assert_eq!(nearest("restart", options), None);
        assert_eq!(nearest("zzz", options), None);
        // Nothing is guessed from one or two characters: every short word is
        // near something.
        assert_eq!(nearest("hl", options), None);
        assert_eq!(nearest("x", options), None);
    }

    #[test]
    fn the_wording_is_ready_to_append_or_absent() {
        assert_eq!(did_you_mean("lps", ["lsp"]), " -- did you mean 'lsp'?");
        assert_eq!(did_you_mean("zzz", ["lsp"]), "");
    }

    // A builtin typed with one slip should resolve back to it, measured over
    // every builtin: each with a letter dropped, one doubled, each adjacent
    // pair swapped.
    //
    // Not 100%, nor can it be: `tet` is one edit from `test` and `let`, and
    // a two-letter builtin has no shape left once a letter goes.
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
                // A "typo" that is itself a builtin isn't one.
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

    // A real program that isn't installed shouldn't be answered with a
    // builtin spelled like it. Names from a `/usr/bin`, pinned so the test
    // says the same everywhere.
    //
    // Of 2,758, 37 draw a suggestion (`sed` is one edit from `set`, `head`
    // from `read`), indistinguishable from typos. It shows only when the
    // program is missing, as a line under an error printed anyway.
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
