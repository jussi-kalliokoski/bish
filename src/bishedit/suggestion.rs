// History-backed inline suggestions: one dimmed guess at the rest of the
// line, from history, without a key press. One suggestion, not a ranked
// list, so a strict prefix filter and a confidence order rather than
// fuzzy matching.
#![allow(dead_code)]

use crate::history::{History, HistoryEntry};
use std::path::Path;

// How much one occurrence of a command supports suggesting it now.
// Ordered so the derived `Ord` ranks `DirectoryAndSequence` highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    // From the history file: its directory and predecessor are unknown
    // (`History::load` has no cwd to attach).
    Legacy,
    // Recorded this session, in another directory.
    Elsewhere,
    // Recorded this session, in this directory.
    CurrentDirectory,
    // ...and right after the command that just ran.
    DirectoryAndSequence,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub text: String,
    pub confidence: Confidence,
}

// `cursor` is a char index into `line`, as in completion.rs's
// `CompletionRequest`. Suggestions only apply at the end of the line; the
// caller checks that.
pub struct SuggestionRequest<'a> {
    pub line: &'a str,
    pub cursor: usize,
}

pub trait SuggestionProvider {
    fn suggest(&self, req: SuggestionRequest) -> Option<Suggestion>;
}

// Suggestions from history.rs's directory-aware `entries()`.
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
        // What just ran: the newest entry, if it was recorded live (the same
        // gate `entries()` applies to `prev`).
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

// Takes history data directly, so tests can use canned entries.
//
// Scores each matching occurrence and judges a candidate by its best one,
// rather than requiring every occurrence to qualify: one stray run
// elsewhere mustn't sink a command run fifty times here. A command always
// run here, right after the same previous one, still reaches
// `DirectoryAndSequence`.
//
// Ties within a level go to the most recent (entries are oldest first),
// so a command run once a moment ago beats one run eighty times long
// ago.
pub(crate) fn best_suggestion(entries: &[HistoryEntry], prefix: &str, cwd: Option<&Path>, prev: Option<&str>) -> Option<Suggestion> {
    if prefix.is_empty() {
        // A ghost of the last command on an empty prompt, accepted by a stray
        // Right arrow, would surprise more than help.
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
        // The candidate must start with the query, not the other way round.
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

    // Run only here and always right after the same command: the strongest
    // match.
    #[test]
    fn perfect_directory_and_sequence_match() {
        let cwd = PathBuf::from("/proj");
        let entries = vec![entry("git add -A", Some(&cwd), None), entry("git commit -m fix", Some(&cwd), Some("git add -A"))];
        let s = best_suggestion(&entries, "git commit", Some(&cwd), Some("git add -A")).unwrap();
        assert_eq!(s.text, "git commit -m fix");
        assert_eq!(s.confidence, Confidence::DirectoryAndSequence);
    }

    // Mostly here plus one stray run elsewhere still beats only elsewhere.
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

// `= EXPR`: the inline calculator

/// Answers a line starting with `= ` with its arithmetic result, and
/// hands anything else to `inner`. `= 3*(2+7)` shows `27` as ghost text
/// before Enter runs the `=` builtin, which prints it.
///
/// A wrapper, not a second provider, because `read_line` takes one, and the
/// line's first character decides which answers.
pub struct ArithSuggestionProvider<'a> {
    pub inner: &'a dyn SuggestionProvider,
    /// Where a name in the expression gets its value. Unset is 0, as in shell
    /// arithmetic.
    pub vars: &'a dyn Fn(&str) -> i64,
}

/// The expression of an `= ...` line, or `None`. The space is required:
/// `=` is a command, so `=1+1` is one word naming no program, and the
/// preview shouldn't answer a line Enter would reject. A line merely
/// containing `=`, or starting `==`, is left alone.
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
        // A half-typed expression (`= 3*(`) has no answer yet, and an error
        // there would be worse than nothing; the next key usually fixes it.
        let value = crate::arith::eval(body, &mut ctx).ok()?;
        Some(Suggestion {
            // A suggestion is the whole line it proposes, and the renderer
            // ghosts what's past the typed part (see
            // `editor::compute_suggestion`): so the line plus the answer,
            // `= 3*(2+7) 27`, which Tab can accept.
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

    // No assignment in a preview: `= (x = 1)` is still being typed.
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
        assert_eq!(ghost("= 3*(2+7)", &Never).as_deref(), Some("= 3*(2+7) 27"));
        assert_eq!(ghost("= 1 + 2", &Never).as_deref(), Some("= 1 + 2 3"));
        assert_eq!(ghost("= x*2", &Never).as_deref(), Some("= x*2 10"), "names resolve");
        assert_eq!(ghost("= unset+1", &Never).as_deref(), Some("= unset+1 1"), "and an unset one is 0");
    }

    #[test]
    fn a_half_typed_expression_says_nothing() {
        assert_eq!(ghost("= 3*(", &Never), None);
        assert_eq!(ghost("= ", &Never), None);
        assert_eq!(ghost("=", &Never), None);
    }

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
