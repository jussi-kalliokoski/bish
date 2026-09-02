// History-backed inline suggestions (see plan.md): a single, dimmed
// "ghost text" guess at what comes next, sourced from command history
// rather than triggered by a key -- the counterpart to completion.rs's
// menu, not a variant of it. Only one suggestion is ever produced (no
// ranked list), so unlike fuzzy.rs's scoring this is deliberately a
// strict-prefix filter plus a small confidence ordering, not a fuzzy
// match.
#![allow(dead_code)]

use crate::history::{History, HistoryEntry};
use std::path::Path;

// How much a single *occurrence* of a candidate command supports
// suggesting it again right now. Ordered so a derived Ord ranks
// DirectoryAndSequence highest -- see best_suggestion's own doc comment
// for why scoring is per-occurrence (candidates aren't classified as a
// whole).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    // Loaded from the history file: neither the directory it ran in nor
    // what it followed is known. Pure fallback -- the information loss
    // this feature explicitly accepts for now (see history.rs's own
    // doc comments on why `load()` never has a cwd to attach).
    Legacy,
    // Recorded live this session, but in some other directory.
    Elsewhere,
    // Recorded live this session, in this exact directory.
    CurrentDirectory,
    // ...and immediately after the same command that just ran -- the
    // feature request's own "100% match" case.
    DirectoryAndSequence,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub text: String,
    pub confidence: Confidence,
}

// `{ line, cursor }` rather than a bare prefix, for symmetry with
// completion.rs's own CompletionRequest -- cursor is a char index into
// `line`, matching LineEditor's own semantics. Suggestions only apply
// at the end of the line in this pass (the caller gates on that before
// ever constructing a request), but carrying cursor rather than
// asserting "always the whole line" means a future mid-line suggestion
// needs no signature change.
pub struct SuggestionRequest<'a> {
    pub line: &'a str,
    pub cursor: usize,
}

pub trait SuggestionProvider {
    fn suggest(&self, req: SuggestionRequest) -> Option<Suggestion>;
}

// The shell's own suggestion source, built entirely on history.rs's own
// directory-aware entries() accessor.
pub struct HistorySuggestionProvider<'a> {
    pub history: &'a History,
    pub cwd: Option<&'a Path>,
}

impl<'a> SuggestionProvider for HistorySuggestionProvider<'a> {
    fn suggest(&self, req: SuggestionRequest) -> Option<Suggestion> {
        let chars: Vec<char> = req.line.chars().collect();
        let cursor = req.cursor.min(chars.len());
        let prefix: String = chars[..cursor].iter().collect();
        let entries = self.history.entries();
        // "What did I just run" -- the newest entry, but only if it was
        // itself recorded live (the same live-recorded gate entries()
        // already applies to its own `prev` field, kept here rather
        // than in a separate History::last() so the rule lives in
        // exactly one place).
        let prev = entries.last().filter(|e| e.cwd.is_some()).map(|e| e.text);
        best_suggestion(&entries, &prefix, self.cwd, prev)
    }
}

// How strongly this one occurrence of a history entry supports
// suggesting it again, given the current directory and what was just run.
fn occurrence_confidence(entry: &HistoryEntry, cwd: Option<&Path>, prev: Option<&str>) -> Confidence {
    let Some(entry_cwd) = entry.cwd else { return Confidence::Legacy };
    if Some(entry_cwd) != cwd {
        return Confidence::Elsewhere;
    }
    if prev.is_some() && entry.prev == prev { Confidence::DirectoryAndSequence } else { Confidence::CurrentDirectory }
}

// The pure core: takes history data directly rather than going through
// a real History/HistorySuggestionProvider -- the same
// classify_plain_argument/_core and flag_candidates/_core split
// highlight.rs and completion.rs already established, for deterministic
// tests against canned data.
//
// Scores every matching *occurrence*, not every distinct candidate
// command as a whole, and keeps each candidate's single best occurrence
// -- deliberately not "does every occurrence of this candidate satisfy
// the same tier." An earlier design tried the latter (group by exact
// candidate text, promote a candidate only if *every* directory-tracked
// occurrence qualified) and had a real inversion bug: a command run 50
// times in the current directory and once, months ago, elsewhere would
// permanently lose its "always this directory" status to a command run
// exactly once, ever, that happened to be here -- one stray historical
// run poisoning an otherwise-perfect candidate forever, with no way for
// later correct usage to recover it. Scoring occurrences and taking the
// max avoids that entirely: a candidate's best-ever occurrence is what
// it's judged by, and the request's own "100% match" case (every
// occurrence in this directory, every time right after the same prior
// command) still reaches Confidence::DirectoryAndSequence exactly as
// described -- it's just not the *only* way to reach a useful tier.
//
// Ties within a confidence level break on recency (a later position in
// `entries`, which is oldest-first) rather than frequency -- matches
// fish/zsh-autosuggestions/atuin's own default of "what did I just do,"
// and avoids a command run 80 times drowning out one run 30 seconds ago
// that's clearly about to be repeated. Frequency is a clean additional
// sort key to layer on later without a type change.
pub(crate) fn best_suggestion(entries: &[HistoryEntry], prefix: &str, cwd: Option<&Path>, prev: Option<&str>) -> Option<Suggestion> {
    if prefix.is_empty() {
        // An empty-prompt ghost of the last command run, with a stray
        // Right-arrow silently filling it in, is surprising rather than
        // useful -- fish suppresses this too.
        return None;
    }

    let mut best: Option<(Confidence, usize, &str)> = None;
    for (i, entry) in entries.iter().enumerate() {
        if !entry.text.starts_with(prefix) {
            continue;
        }
        if entry.text.len() == prefix.len() {
            continue; // exact match -- nothing left to show as a ghost
        }
        if entry.text.contains('\n') {
            continue; // can't render a multi-line entry on one row
        }
        let confidence = occurrence_confidence(entry, cwd, prev);
        let candidate = (confidence, i);
        let is_better = match &best {
            None => true,
            Some((best_confidence, best_i, _)) => candidate > (*best_confidence, *best_i),
        };
        if is_better {
            best = Some((confidence, i, entry.text));
        }
    }
    best.map(|(confidence, _, text)| Suggestion { text: text.to_string(), confidence })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry<'a>(text: &'a str, cwd: Option<&'a Path>, prev: Option<&'a str>) -> HistoryEntry<'a> {
        HistoryEntry { text, cwd, time: None, prev }
    }

    #[test]
    fn strict_prefix_rejection() {
        // "ca" is not a prefix of "git ca..." -- the candidate must
        // start with the query, not the other way around.
        let cwd = PathBuf::from("/proj");
        let entries = vec![entry("git ca whatever", Some(&cwd), None)];
        assert_eq!(best_suggestion(&entries, "ca", Some(&cwd), None), None);
    }

    #[test]
    fn exact_match_produces_nothing() {
        let cwd = PathBuf::from("/proj");
        let entries = vec![entry("ls", Some(&cwd), None)];
        assert_eq!(best_suggestion(&entries, "ls", Some(&cwd), None), None);
    }

    #[test]
    fn newline_containing_candidate_is_skipped() {
        let cwd = PathBuf::from("/proj");
        let entries = vec![entry("for i in 1 2 3\ndo echo $i\ndone", Some(&cwd), None)];
        assert_eq!(best_suggestion(&entries, "for", Some(&cwd), None), None);
    }

    #[test]
    fn empty_prefix_yields_none() {
        let cwd = PathBuf::from("/proj");
        let entries = vec![entry("git status", Some(&cwd), None)];
        assert_eq!(best_suggestion(&entries, "", Some(&cwd), None), None);
    }

    #[test]
    fn disk_loaded_entry_loses_to_same_directory_live_entry() {
        let cwd = PathBuf::from("/proj");
        let entries = vec![entry("git commit -m legacy", None, None), entry("git commit -m live", Some(&cwd), None)];
        let s = best_suggestion(&entries, "git commit", Some(&cwd), None).unwrap();
        assert_eq!(s.text, "git commit -m live");
        assert_eq!(s.confidence, Confidence::CurrentDirectory);
    }

    // The user's own literal worked example: a candidate run only in
    // the current directory, always immediately after the same prior
    // command, is a "100% match."
    #[test]
    fn perfect_directory_and_sequence_match() {
        let cwd = PathBuf::from("/proj");
        let entries = vec![entry("git add -A", Some(&cwd), None), entry("git commit -m fix", Some(&cwd), Some("git add -A"))];
        let s = best_suggestion(&entries, "git commit", Some(&cwd), Some("git add -A")).unwrap();
        assert_eq!(s.text, "git commit -m fix");
        assert_eq!(s.confidence, Confidence::DirectoryAndSequence);
    }

    // The specific failure mode per-occurrence scoring exists to avoid:
    // a candidate run mostly in this directory, plus one stray run
    // elsewhere, must still beat a candidate that's only ever been run
    // elsewhere -- one bad historical occurrence must not poison an
    // otherwise-strong candidate.
    #[test]
    fn one_stray_occurrence_elsewhere_does_not_poison_an_otherwise_strong_candidate() {
        let here = PathBuf::from("/proj");
        let elsewhere = PathBuf::from("/tmp");
        let mut entries = vec![entry("cargo bench", Some(&elsewhere), None)];
        for _ in 0..50 {
            entries.push(entry("cargo build", Some(&here), None));
        }
        entries.push(entry("cargo build", Some(&elsewhere), None));

        let s = best_suggestion(&entries, "cargo", Some(&here), None).unwrap();
        assert_eq!(s.text, "cargo build");
        assert_eq!(s.confidence, Confidence::CurrentDirectory);
    }

    #[test]
    fn recency_breaks_a_same_confidence_tie() {
        let cwd = PathBuf::from("/proj");
        let entries = vec![entry("echo old", Some(&cwd), None), entry("echo new", Some(&cwd), None)];
        let s = best_suggestion(&entries, "echo", Some(&cwd), None).unwrap();
        assert_eq!(s.text, "echo new");
    }

    #[test]
    fn sequence_match_beats_same_directory_only_even_if_less_recent() {
        let cwd = PathBuf::from("/proj");
        let entries = vec![
            entry("npm run build", Some(&cwd), Some("npm install")),
            entry("npm run lint", Some(&cwd), None), // more recent, no sequence match
        ];
        let s = best_suggestion(&entries, "npm run", Some(&cwd), Some("npm install")).unwrap();
        assert_eq!(s.text, "npm run build");
        assert_eq!(s.confidence, Confidence::DirectoryAndSequence);
    }
}

// ---------------------------------------------------------------------
// `= EXPR` -- the inline calculator
// ---------------------------------------------------------------------

/// Answers a line that starts with `=` with its own arithmetic result,
/// and hands everything else to `inner`.
///
/// The calculator every terminal person opens a second shell for. Typing
/// `= 3*(2+7)` shows `21` in the same ghost text history suggestions use
/// -- so the answer is there before you decide whether you even wanted
/// to run anything -- and pressing Enter runs the `=` builtin, which
/// prints it.
///
/// A wrapper rather than a second provider slot because `read_line`
/// takes one: this is a *decision* about which source answers, and the
/// line's first character makes it unambiguous.
pub struct ArithSuggestionProvider<'a> {
    pub inner: &'a dyn SuggestionProvider,
    /// Where a name in the expression gets its value. Unset reads as 0,
    /// which is what shell arithmetic does everywhere else.
    pub vars: &'a dyn Fn(&str) -> i64,
}

/// The expression body of an `= ...` line, or `None` when this is not
/// one.
///
/// The space is required, and that is not fussiness: `=` is a *command*,
/// and a shell splits commands on words, so `=1+1` is a single word
/// naming a program nobody has. Suggesting an answer for a line that
/// Enter would then reject is worse than suggesting nothing, so the
/// preview holds itself to exactly what the builtin can run.
///
/// A line that merely *contains* an `=` is left alone, and so is `==`,
/// which is a comparison someone is in the middle of typing.
pub fn arith_line(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('=')?;
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let body = rest.trim();
    (!body.is_empty()).then_some(body)
}

impl SuggestionProvider for ArithSuggestionProvider<'_> {
    fn suggest(&self, req: SuggestionRequest) -> Option<Suggestion> {
        let Some(body) = arith_line(req.line) else {
            return self.inner.suggest(req);
        };
        let mut ctx = ClosureVars { get: self.vars };
        // A half-typed expression (`= 3*(`) simply has no answer yet;
        // showing an error where a suggestion goes would be worse than
        // showing nothing, since the next keystroke usually fixes it.
        let value = crate::arith::eval(body, &mut ctx).ok()?;
        Some(Suggestion {
            // A `Suggestion`'s text is the *whole line* it proposes --
            // the renderer ghosts whatever of it extends past what has
            // been typed (see `editor::compute_suggestion`). So this is
            // the line plus the answer, which draws as `= 3*(2+7) 27`:
            // an answer alongside the question rather than a
            // replacement for it, and one that Tab turns into real text
            // if you want to keep it.
            text: format!("{} {value}", req.line),
            confidence: Confidence::DirectoryAndSequence,
        })
    }
}

struct ClosureVars<'a> {
    get: &'a dyn Fn(&str) -> i64,
}

impl crate::arith::VarContext for ClosureVars<'_> {
    fn get(&mut self, name: &str) -> i64 {
        (self.get)(name)
    }

    // An assignment inside a *preview* must not happen: `= (x = 1)` is
    // still being typed, and the whole point of the ghost text is that
    // it costs nothing to look at.
    fn set(&mut self, _name: &str, _value: i64) {}
}

#[cfg(test)]
mod arith_suggestion_tests {
    use super::*;

    struct Never;
    impl SuggestionProvider for Never {
        fn suggest(&self, _req: SuggestionRequest) -> Option<Suggestion> {
            None
        }
    }

    struct Fixed(&'static str);
    impl SuggestionProvider for Fixed {
        fn suggest(&self, _req: SuggestionRequest) -> Option<Suggestion> {
            Some(Suggestion { text: self.0.to_string(), confidence: Confidence::Legacy })
        }
    }

    fn ghost(line: &str, inner: &dyn SuggestionProvider) -> Option<String> {
        let p = ArithSuggestionProvider { inner, vars: &|name| if name == "x" { 5 } else { 0 } };
        p.suggest(SuggestionRequest { line, cursor: line.chars().count() }).map(|s| s.text)
    }

    #[test]
    fn an_arithmetic_line_previews_its_own_answer() {
        // The whole line plus the answer: the renderer ghosts whatever
        // extends past what has been typed.
        assert_eq!(ghost("= 3*(2+7)", &Never).as_deref(), Some("= 3*(2+7) 27"));
        assert_eq!(ghost("= 1 + 2", &Never).as_deref(), Some("= 1 + 2 3"));
        assert_eq!(ghost("= x*2", &Never).as_deref(), Some("= x*2 10"), "names resolve");
        assert_eq!(ghost("= unset+1", &Never).as_deref(), Some("= unset+1 1"), "and an unset one is 0");
    }

    #[test]
    fn a_half_typed_expression_says_nothing() {
        // Not an error where a suggestion goes: the next keystroke
        // usually fixes it.
        assert_eq!(ghost("= 3*(", &Never), None);
        assert_eq!(ghost("= ", &Never), None);
        assert_eq!(ghost("=", &Never), None);
    }

    // The space is not fussiness: `=` is a command, and a shell splits
    // commands on words, so `=1+1` names a program nobody has.
    // Suggesting an answer Enter would then reject is worse than
    // suggesting nothing.
    #[test]
    fn only_the_spelling_the_builtin_can_actually_run() {
        assert_eq!(arith_line("= 1+1"), Some("1+1"));
        assert_eq!(arith_line("=\t1+1"), Some("1+1"));
        assert_eq!(arith_line("=1+1"), None);
        assert_eq!(arith_line("== 1"), None, "someone mid-comparison");
        assert_eq!(arith_line("x = 1"), None, "only at the start of the line");
    }

    #[test]
    fn everything_else_still_reaches_the_history_provider() {
        assert_eq!(ghost("echo hi", &Fixed("echo history")).as_deref(), Some("echo history"));
        assert_eq!(ghost("=1+1", &Fixed("echo history")).as_deref(), Some("echo history"));
        assert_eq!(ghost("echo hi", &Never), None);
    }
}
