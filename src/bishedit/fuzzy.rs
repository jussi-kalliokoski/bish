// Hand-rolled fuzzy matcher for tab-completion candidates (see plan.md).
// Zero crates, consistent with the rest of this codebase. Subsequence match
// (candidate must contain query's characters in order, not necessarily
// contiguous) with fzf-style greedy-first-match scoring -- not a globally
// optimal alignment, just cheap and good enough to rank completion lists.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i32,
    pub positions: Vec<usize>,
}

fn lower_char(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c
    }
}

fn is_boundary_start(cand_chars: &[char], pos: usize) -> bool {
    if pos == 0 {
        return true;
    }
    let prev = cand_chars[pos - 1];
    if prev == '-' || prev == '_' || prev == '/' {
        return true;
    }
    prev.is_lowercase() && cand_chars[pos].is_uppercase()
}

/// Empty query matches everything with score 0 -- this is what lets Tab on
/// a bare `git ` show every subcommand unranked-but-present.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }

    let query_lower: Vec<char> = query.chars().map(lower_char).collect();
    let cand_chars: Vec<char> = candidate.chars().collect();
    let cand_lower: Vec<char> = cand_chars.iter().copied().map(lower_char).collect();

    let mut positions = Vec::with_capacity(query_lower.len());
    let mut cand_idx = 0usize;
    for &qc in &query_lower {
        let mut found = None;
        while cand_idx < cand_lower.len() {
            if cand_lower[cand_idx] == qc {
                found = Some(cand_idx);
                cand_idx += 1;
                break;
            }
            cand_idx += 1;
        }
        positions.push(found?);
    }

    let query_chars: Vec<char> = query.chars().collect();
    let mut score = 0i32;
    let mut prev_pos: Option<usize> = None;
    for (i, &pos) in positions.iter().enumerate() {
        score += 10;
        if is_boundary_start(&cand_chars, pos) {
            score += 10;
        }
        if let Some(p) = prev_pos {
            if pos == p + 1 {
                score += 15;
            }
        }
        // A small tiebreak nudge, not a real ranking factor: without it,
        // a case-insensitive match against candidates differing only by
        // case (e.g. "-l" vs "-L") ties on every other term above and
        // falls back to alphabetical order, where a bare uppercase
        // letter sorts before its lowercase counterpart in plain ASCII
        // -- surprising when the query's own case exactly matched one of
        // them. +1 is deliberately smaller than any other scoring term
        // here, so it only ever breaks an otherwise-exact tie.
        if cand_chars[pos] == query_chars[i] {
            score += 1;
        }
        prev_pos = Some(pos);
    }
    score -= positions[0] as i32;
    score -= (cand_chars.len() as i32 - query_lower.len() as i32).max(0);

    Some(FuzzyMatch { score, positions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_everything() {
        let m = fuzzy_match("", "anything").unwrap();
        assert_eq!(m.score, 0);
        assert!(m.positions.is_empty());
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert!(fuzzy_match("xyz", "abc").is_none());
        assert!(fuzzy_match("ba", "abc").is_none()); // wrong order
    }

    #[test]
    fn basic_subsequence_matches_with_positions() {
        let m = fuzzy_match("gco", "git-checkout").unwrap();
        // g=0, c=4 ("git-c..."), then the next 'o' after index 4 is at index 9.
        assert_eq!(m.positions, vec![0, 4, 9]);
    }

    #[test]
    fn case_insensitive() {
        let a = fuzzy_match("GC", "git-checkout").unwrap();
        let b = fuzzy_match("gc", "GIT-CHECKOUT").unwrap();
        assert_eq!(a.positions, vec![0, 4]);
        assert_eq!(b.positions, vec![0, 4]);
    }

    // Found via interactive verification: `ls -l` + Tab was completing to
    // "-L" instead of "-l" -- both matched the case-insensitive query
    // equally well on every other scoring term, so it fell back to plain
    // alphabetical order, where "-L" (uppercase) sorts first. An exact-
    // case match should win a tie like this.
    #[test]
    fn exact_case_match_breaks_a_tie_with_a_differently_cased_candidate() {
        let lower = fuzzy_match("-l", "-l").unwrap();
        let upper = fuzzy_match("-l", "-L").unwrap();
        assert!(lower.score > upper.score, "{lower:?} vs {upper:?}");
    }

    #[test]
    fn contiguous_match_scores_higher() {
        let commit = fuzzy_match("co", "commit").unwrap();
        let checkout = fuzzy_match("co", "checkout").unwrap();
        // commit: c=0,o=1 contiguous; checkout: c=0,o=5 not contiguous.
        assert!(commit.score > checkout.score, "{:?} vs {:?}", commit, checkout);
    }

    #[test]
    fn word_boundary_start_bonus() {
        // "co" in "count-objects" -> boundary at 0; in "xxco" -> not a boundary.
        let boundary = fuzzy_match("co", "count-objects").unwrap();
        let no_boundary = fuzzy_match("co", "xxco").unwrap();
        assert!(boundary.score > no_boundary.score);
    }

    #[test]
    fn shorter_candidate_preferred() {
        let exact = fuzzy_match("co", "co").unwrap();
        let longer = fuzzy_match("co", "count").unwrap();
        assert!(exact.score > longer.score);
    }

    #[test]
    fn earlier_match_preferred() {
        let early = fuzzy_match("a", "aardvark").unwrap();
        let late = fuzzy_match("a", "banana").unwrap();
        assert!(early.score > late.score, "{:?} vs {:?}", early, late);
    }

    #[test]
    fn git_co_ranks_subcommands_as_expected() {
        // The plan's own worked example: commit/config/count-objects should
        // all rank above checkout for the query "co".
        let candidates = ["commit", "config", "count-objects", "checkout"];
        let mut scored: Vec<(&str, i32)> = candidates
            .iter()
            .filter_map(|c| fuzzy_match("co", c).map(|m| (*c, m.score)))
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        let checkout_rank = scored.iter().position(|(name, _)| *name == "checkout").unwrap();
        assert_eq!(checkout_rank, scored.len() - 1, "{:?}", scored);
    }
}
