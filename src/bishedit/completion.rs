// Tab-completion (see plan.md). Foundation stage: the candidate/provider
// types every source will implement against, plus the cursor-targeted
// word-role walker (`find_word_start` / `classify_word_role`) that decides
// *what kind* of completion applies to the word under the cursor --
// command name, flag, subcommand, or file. The actual shell-backed
// candidate sources are a later stage in this same file.
#![allow(dead_code)]

use crate::bishedit::fuzzy;
use crate::bishedit::highlight::{self, is_assignment_prefix_word, resets_command_position, KNOWN_BUILTINS};
use crate::bishedit::manpages;
use crate::compgen;
use crate::lexer::{self, Chunk, SpannedItem, Tok};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct CompletionCandidate {
    pub display: String,
    pub matched_positions: Vec<usize>,
}

pub struct CompletionRequest<'a> {
    pub line: &'a str,
    // Char index (not byte offset) into `line`'s chars() sequence --
    // matches LineEditor's own cursor semantics, since that's this
    // request's real origin.
    pub cursor: usize,
}

pub struct CompletionResult {
    // Char index. `word_start..cursor` is always the replaced range -- v1
    // never touches text after the cursor within the same word.
    pub word_start: usize,
    pub candidates: Vec<CompletionCandidate>,
}

pub trait CompletionProvider {
    fn complete(&self, req: CompletionRequest) -> CompletionResult;
}

// Whitespace and `| & ; ( ) < >` are word boundaries; quotes/`$`/backslash
// are deliberately NOT -- a documented v1 gap (this scans plain characters,
// it doesn't understand quoting), matching the same "no crash, just a
// plausible-not-guaranteed answer" tolerance the rest of this feature
// leans on for anything quote/substitution-shaped.
fn is_word_char(c: char) -> bool {
    !(c.is_whitespace() || matches!(c, '|' | '&' | ';' | '(' | ')' | '<' | '>'))
}

// Scans backward from `cursor` over word characters to the enclosing
// boundary. Always lands exactly on a boundary (or 0), which is what lets
// `classify_word_role` treat `chars[0..word_start]` as a clean,
// fully-terminated prefix safe to re-tokenize on its own.
pub(crate) fn find_word_start(chars: &[char], cursor: usize) -> usize {
    let mut i = cursor.min(chars.len());
    while i > 0 && is_word_char(chars[i - 1]) {
        i -= 1;
    }
    i
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CmdRole {
    Command,
    Argument { command: Option<String>, arg_index: usize },
}

enum CmdPosLite {
    ExpectCommand,
    InCommand { name: Option<String>, arg_index: usize },
}

// Re-tokenizes `prefix_text` (always a clean prefix ending right at a word
// boundary, per find_word_start) and walks it with the same
// ExpectCommand/InCommand shape highlight.rs's own CmdPos state machine
// uses -- reusing its now-pub(crate) `resets_command_position` /
// `is_assignment_prefix_word` primitives rather than sharing that
// module's actual stateful walker, which is built for a different job
// (emitting spans across a whole line, not answering "what's the role of
// the word starting right after this text"). The *terminal* state after
// walking every token in `prefix_text` is the role of the word being
// completed.
//
// Tolerates anything tokenize_spanned tolerates: an unclosed `$(...)` etc.
// degrades to whatever partial token stream the lexer managed to collect
// before erroring (see SpannedResult::error's own doc comment) -- never a
// panic, just a role that may not be the "real" one a full parse would
// give.
pub(crate) fn classify_word_role(prefix_text: &str) -> CmdRole {
    let res = lexer::tokenize_spanned(prefix_text);
    let mut cmd_pos = CmdPosLite::ExpectCommand;
    for item in &res.items {
        let SpannedItem::Tok(tok, _) = item else { continue };
        if resets_command_position(tok) {
            cmd_pos = CmdPosLite::ExpectCommand;
        }
        if let Tok::Word(chunks, _) = tok {
            cmd_pos = match cmd_pos {
                CmdPosLite::ExpectCommand if is_assignment_prefix_word(chunks) => CmdPosLite::ExpectCommand,
                CmdPosLite::ExpectCommand => {
                    let name = if let [Chunk::Str(s)] = chunks.as_slice() { Some(s.clone()) } else { None };
                    CmdPosLite::InCommand { name, arg_index: 0 }
                }
                CmdPosLite::InCommand { name, arg_index } => CmdPosLite::InCommand { name, arg_index: arg_index + 1 },
            };
        }
    }
    match cmd_pos {
        CmdPosLite::ExpectCommand => CmdRole::Command,
        CmdPosLite::InCommand { name, arg_index } => CmdRole::Argument { command: name, arg_index },
    }
}

// Fuzzy-scores and ranks `names` against `prefix`, dropping non-matches
// (fuzzy::fuzzy_match already handles "does this even qualify"). Ties
// break alphabetically for a stable, predictable order rather than
// insertion order, which would otherwise vary source to source (HashSet
// iteration for command names, read_dir order for files).
fn rank(prefix: &str, names: Vec<String>) -> Vec<CompletionCandidate> {
    let mut scored: Vec<(i32, CompletionCandidate)> = names
        .into_iter()
        .filter_map(|name| {
            fuzzy::fuzzy_match(prefix, &name)
                .map(|m| (m.score, CompletionCandidate { display: name, matched_positions: m.positions }))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.display.cmp(&b.1.display)));
    scored.into_iter().map(|(_, c)| c).collect()
}

// A registered completion spec's own candidates are shown exactly as it
// produced them, in its own order, unfiltered -- never re-run through
// `rank`'s own fuzzy_match/drop-non-matches step, since a -F/-C source's
// entries are deliberately not required to start with (or otherwise
// contain, in fuzzy-subsequence order) `prefix` at all (see compgen.rs's
// own doc comment on why -F/-C are trusted to have already applied their
// own logic) -- fuzzy-filtering them here could silently make a real,
// intentional COMPREPLY entry disappear from the popup. Still highlights
// the matched prefix span when a candidate happens to actually start with
// it (the common case for -W/-A/-G-sourced entries), for the same visual
// bolding every other source gets.
fn as_unranked_candidates(prefix: &str, names: Vec<String>) -> Vec<CompletionCandidate> {
    let prefix_len = prefix.chars().count();
    names
        .into_iter()
        .map(|display| {
            let matched_positions = if display.starts_with(prefix) { (0..prefix_len).collect() } else { Vec::new() };
            CompletionCandidate { display, matched_positions }
        })
        .collect()
}

// Thin wrapper: resolves `command`'s man-page data if ready (None
// otherwise -- Pending/Missing both mean "nothing to offer yet"), mirroring
// highlight.rs's own classify_plain_argument's identical resolve-then-
// delegate shape.
fn ready_man_data(command: &str) -> Option<std::sync::Arc<manpages::ManPageData>> {
    match manpages::query(command) {
        manpages::ManStatus::Ready(data) => Some(data),
        manpages::ManStatus::Pending | manpages::ManStatus::Missing => None,
    }
}

// Takes the man data directly rather than going through the real
// cache/thread -- the seam this module's own tests use to stay
// deterministic without spawning `man`, matching
// classify_plain_argument_core's own precedent in highlight.rs.
fn flag_candidates_core(man: Option<&manpages::ManPageData>, prefix: &str) -> Vec<CompletionCandidate> {
    let Some(man) = man else { return Vec::new() };
    rank(prefix, man.flags.clone())
}

// None means "no usable subcommand data" -- the caller's cue to fall
// through to file candidates instead, per this feature's "arg 0 without
// subcommand data completes as a file" rule.
fn subcommand_candidates_core(man: Option<&manpages::ManPageData>, prefix: &str) -> Option<Vec<CompletionCandidate>> {
    let man = man?;
    if man.subcommands.is_empty() {
        return None;
    }
    Some(rank(prefix, man.subcommands.clone()))
}

// The shell's own built-in completion source, built entirely on
// infrastructure the highlighting feature already established:
// KNOWN_BUILTINS/known_functions/enumerate_path_matches for command names,
// manpages::query for subcommands and flags (same cache the highlighter
// itself populates, so a command that's already been typed once tends to
// have `Ready` data by the time its own completion is requested), and a
// single-level `read_dir` for files. `cwd`/`known_functions` are the same
// owned-snapshot pattern HighlightContext already uses -- borrowed here,
// not re-snapshotted, since the caller already did that work.
pub struct ShellCompletionProvider<'a> {
    pub cwd: Option<&'a Path>,
    pub known_functions: Option<&'a HashSet<String>>,
    // `complete NAME`-registered specs (a per-prompt snapshot of
    // Shell::completions/default_completion -- see repl.rs's own
    // construction site) and the contextual data (aliases, PATH commands,
    // jobs, ...) compgen::resolve_spec needs to actually evaluate one, plus
    // a snapshot of the caller's own functions/vars (Shell::
    // functions_preamble's output) for -F/-C specs, which run via
    // subprocess -- see compgen.rs's own doc comment on why this needs no
    // live, mutably-borrowable Shell at all. `None` (rather than an empty
    // map) is what a caller with no completions concept at all -- e.g. this
    // module's own tests -- passes, distinct from "no specs registered
    // yet" (an empty map still consults `default_completion` if any).
    pub completions: Option<&'a HashMap<String, compgen::CompgenSpec>>,
    pub default_completion: Option<&'a compgen::CompgenSpec>,
    pub action_ctx: Option<&'a compgen::ActionContext>,
    pub functions_preamble: Option<&'a str>,
}

impl<'a> CompletionProvider for ShellCompletionProvider<'a> {
    fn complete(&self, req: CompletionRequest) -> CompletionResult {
        let chars: Vec<char> = req.line.chars().collect();
        let cursor = req.cursor.min(chars.len());
        let word_start = find_word_start(&chars, cursor);
        let prefix: String = chars[word_start..cursor].iter().collect();
        let prefix_text: String = chars[..word_start].iter().collect();

        let role = classify_word_role(&prefix_text);
        if let CmdRole::Argument { command, .. } = &role
            && let Some(candidates) = self.registered_spec_candidates(command.as_deref(), &prefix)
        {
            return CompletionResult { word_start, candidates };
        }

        let candidates = match role {
            CmdRole::Command => self.command_name_candidates(&prefix),
            CmdRole::Argument { command, .. } if prefix.starts_with('-') => self.flag_candidates(command.as_deref(), &prefix),
            CmdRole::Argument { command, arg_index: 0 } => self.subcommand_or_file_candidates(command.as_deref(), &prefix),
            CmdRole::Argument { .. } => self.file_candidates(&prefix),
        };

        CompletionResult { word_start, candidates }
    }
}

impl<'a> ShellCompletionProvider<'a> {
    // `None` means "no registered spec applies here -- fall through to the
    // built-in flag/subcommand/file logic below"; `Some(candidates)` (even
    // if empty) means a spec *did* match and fully owns this completion,
    // matching real bash's own model: once a command has a registered
    // `complete`, every one of its argument completions goes through that
    // spec -- flags included, never blended with the built-in fallback.
    // `command`'s own registered spec wins if one exists; the `-D` default
    // spec (see Shell::default_completion) is the fallback for every other
    // command, including one bish doesn't otherwise recognize at all, and
    // even when `command` itself couldn't be resolved to a literal name
    // (e.g. it's behind a variable).
    fn registered_spec_candidates(&self, command: Option<&str>, prefix: &str) -> Option<Vec<CompletionCandidate>> {
        let completions = self.completions?;
        let spec = command.and_then(|c| completions.get(c)).or(self.default_completion)?;
        let default_ctx = compgen::ActionContext::default();
        let ctx = self.action_ctx.unwrap_or(&default_ctx);
        let cwd = self.cwd.unwrap_or_else(|| Path::new("."));
        let preamble = self.functions_preamble.unwrap_or("");
        let names = compgen::resolve_spec(spec, prefix, ctx, cwd, preamble);
        Some(as_unranked_candidates(prefix, names))
    }

    // KNOWN_BUILTINS union known_functions union PATH -- deliberately
    // excludes aliases; see HighlightContext::known_functions's own doc
    // comment for why an alias name isn't safe to offer as "this will
    // run" here (this shell's `alias` builtin never expands them at
    // command-run time).
    fn command_name_candidates(&self, prefix: &str) -> Vec<CompletionCandidate> {
        let mut names: HashSet<String> = KNOWN_BUILTINS.iter().map(|s| s.to_string()).collect();
        if let Some(functions) = self.known_functions {
            names.extend(functions.iter().cloned());
        }
        names.extend(highlight::enumerate_path_matches(prefix));
        rank(prefix, names.into_iter().collect())
    }

    // Flags only, never falling through to subcommands/files even on a
    // miss -- exact parity with classify_plain_argument_core's own
    // flag-never-falls-through rule in highlight.rs. No command name, or
    // no ready man-page data yet, just yields nothing (not files: a
    // `-`-shaped word is never meaningfully a file path either).
    fn flag_candidates(&self, command: Option<&str>, prefix: &str) -> Vec<CompletionCandidate> {
        let Some(cmd) = command else { return Vec::new() };
        flag_candidates_core(ready_man_data(cmd).as_deref(), prefix)
    }

    // arg_index == 0, non-flag: subcommands if the man page actually has
    // subcommand data, else falls through to files -- this is the `git
    // co` case (manpages::query("git") is likely already `Ready` from
    // highlighting the same line).
    fn subcommand_or_file_candidates(&self, command: Option<&str>, prefix: &str) -> Vec<CompletionCandidate> {
        if let Some(cmd) = command {
            if let Some(candidates) = subcommand_candidates_core(ready_man_data(cmd).as_deref(), prefix) {
                return candidates;
            }
        }
        self.file_candidates(prefix)
    }

    // Splits `prefix` at its last '/' into a directory part (kept as a
    // literal prefix of every candidate's own display string, so the
    // returned candidates are always the *whole* replacement word, e.g.
    // "src/fuzzy.rs" not just "fuzzy.rs") and resolves that directory
    // against `cwd` if relative. Exactly one read_dir -- no recursion,
    // the explicit "just the explicitly typed level" scope limit.
    // Directories get a trailing '/' appended, both a UX convention and
    // what invites pressing Tab again for the next level rather than this
    // feature ever walking ahead on its own. Matching the full `prefix`
    // against the full display (rather than just the trailing segment)
    // works out naturally: the directory part is a literal prefix of
    // both, so it always matches contiguously at the front, and the fuzzy
    // step only has real work left to do on the filename itself.
    fn file_candidates(&self, prefix: &str) -> Vec<CompletionCandidate> {
        let Some(cwd) = self.cwd else { return Vec::new() };
        let dir_part = match prefix.rfind('/') {
            Some(idx) => &prefix[..idx + 1],
            None => "",
        };
        let dir_path = if let Some(rest) = dir_part.strip_prefix('~') {
            // `~/...` -- bare-tilde home expansion, the same shape
            // lexer.rs's own tilde expansion recognizes (no `~user`
            // lookup). Only the filesystem lookup gets expanded: `dir_part`
            // itself stays literal, so every candidate this returns still
            // starts with the `~/` text the user actually typed, matching
            // what read_line's accept step will splice back into the
            // buffer.
            let Ok(home) = std::env::var("HOME") else { return Vec::new() };
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        } else if dir_part.is_empty() {
            cwd.to_path_buf()
        } else if Path::new(dir_part).is_absolute() {
            PathBuf::from(dir_part)
        } else {
            cwd.join(dir_part)
        };
        let Ok(entries) = std::fs::read_dir(&dir_path) else { return Vec::new() };
        let mut names = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            names.push(format!("{dir_part}{name}{}", if is_dir { "/" } else { "" }));
        }
        rank(prefix, names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_word_start_scans_back_to_whitespace() {
        let chars: Vec<char> = "git checkout".chars().collect();
        assert_eq!(find_word_start(&chars, chars.len()), 4); // start of "checkout"
        assert_eq!(find_word_start(&chars, 4), 4); // already at boundary
        assert_eq!(find_word_start(&chars, 0), 0);
    }

    #[test]
    fn find_word_start_treats_pipe_and_semicolon_as_boundaries() {
        let chars: Vec<char> = "true|false".chars().collect();
        assert_eq!(find_word_start(&chars, chars.len()), 5); // start of "false"
        let chars: Vec<char> = "true;false".chars().collect();
        assert_eq!(find_word_start(&chars, chars.len()), 5);
    }

    #[test]
    fn classify_word_role_bare_prefix_is_command_position() {
        assert_eq!(classify_word_role(""), CmdRole::Command);
    }

    #[test]
    fn classify_word_role_git_co_is_arg_zero_of_git() {
        // The plan's own worked example: "git co" with the cursor at the
        // end -- word_start is 4 ("co" starts there), and prefix_text
        // (chars[0..4]) is "git ".
        let line = "git co";
        let chars: Vec<char> = line.chars().collect();
        let cursor = chars.len();
        let word_start = find_word_start(&chars, cursor);
        assert_eq!(word_start, 4);
        let prefix_text: String = chars[..word_start].iter().collect();
        assert_eq!(prefix_text, "git ");
        assert_eq!(
            classify_word_role(&prefix_text),
            CmdRole::Argument { command: Some("git".to_string()), arg_index: 0 }
        );
    }

    #[test]
    fn classify_word_role_resets_to_command_after_a_pipe() {
        assert_eq!(classify_word_role("true | "), CmdRole::Command);
    }

    #[test]
    fn classify_word_role_skips_an_assignment_prefix() {
        // "FOO=bar " leaves the position still expecting the real command
        // name, not treating FOO=bar as arg 0 of anything.
        assert_eq!(classify_word_role("FOO=bar "), CmdRole::Command);
    }

    #[test]
    fn classify_word_role_degrades_gracefully_inside_an_unclosed_substitution() {
        // Must not panic; the exact role isn't asserted since an unclosed
        // $(...) is documented out-of-scope for a guaranteed-correct
        // answer here -- only that this stays inert.
        let _ = classify_word_role("echo $(git chec");
    }

    fn display_names(candidates: Vec<CompletionCandidate>) -> Vec<String> {
        candidates.into_iter().map(|c| c.display).collect()
    }

    #[test]
    fn command_name_candidates_includes_known_builtins_matching_prefix() {
        let provider = ShellCompletionProvider { cwd: None, known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None };
        let names = display_names(provider.command_name_candidates("ech"));
        assert!(names.iter().any(|n| n == "echo"), "{names:?}");
    }

    #[test]
    fn command_name_candidates_includes_a_known_function() {
        // A deliberately unusual prefix (not just "my") -- this
        // environment's real PATH (WSL interop with Windows system32)
        // can contain unrelated executables matching short, common
        // prefixes, which would make the assertion flaky.
        let mut functions = HashSet::new();
        functions.insert("zz_bish_test_func".to_string());
        let provider = ShellCompletionProvider { cwd: None, known_functions: Some(&functions), completions: None, default_completion: None, action_ctx: None, functions_preamble: None };
        let names = display_names(provider.command_name_candidates("zz_bish_test"));
        assert_eq!(names, vec!["zz_bish_test_func".to_string()]);
    }

    #[test]
    fn command_name_candidates_includes_a_real_path_executable() {
        // coreutils -- same real-PATH assumption this whole feature
        // already leans on elsewhere (highlight.rs's own is_in_path
        // tests).
        let provider = ShellCompletionProvider { cwd: None, known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None };
        let names = display_names(provider.command_name_candidates("tru"));
        assert!(names.iter().any(|n| n == "true"), "{names:?}");
    }

    #[test]
    fn flag_candidates_core_ranks_flags_by_prefix() {
        let man = manpages::ManPageData {
            flags: vec!["-l".to_string(), "--long".to_string(), "-a".to_string()],
            subcommands: vec![],
        };
        let names = display_names(flag_candidates_core(Some(&man), "-l"));
        assert_eq!(names, vec!["-l".to_string(), "--long".to_string()]);
    }

    #[test]
    fn flag_candidates_core_yields_nothing_without_man_data() {
        assert_eq!(flag_candidates_core(None, "-l"), Vec::new());
    }

    #[test]
    fn subcommand_candidates_core_returns_none_when_page_has_no_subcommands() {
        let man = manpages::ManPageData { flags: vec!["-l".to_string()], subcommands: vec![] };
        assert_eq!(subcommand_candidates_core(Some(&man), "co"), None);
        assert_eq!(subcommand_candidates_core(None, "co"), None);
    }

    // The plan's own literal worked example, via canned man-page data
    // (never spawns real `man`): commit/config/count-objects should all
    // rank above checkout for the query "co".
    #[test]
    fn subcommand_candidates_core_git_co_worked_example() {
        let man = manpages::ManPageData {
            flags: vec![],
            subcommands: vec!["commit".to_string(), "config".to_string(), "count-objects".to_string(), "checkout".to_string()],
        };
        let names = display_names(subcommand_candidates_core(Some(&man), "co").unwrap());
        assert_eq!(names.last().map(String::as_str), Some("checkout"), "{names:?}");
        let leaders = &names[..names.len() - 1];
        assert!(leaders.iter().any(|n| n == "commit"), "{names:?}");
        assert!(leaders.iter().any(|n| n == "config"), "{names:?}");
        assert!(leaders.iter().any(|n| n == "count-objects"), "{names:?}");
    }

    // Real temp-dir fixture, self-cleaning: one subdirectory and one
    // regular file sharing a prefix, confirming both the prefix filter and
    // the directory-gets-a-trailing-slash convention.
    #[test]
    fn file_candidates_lists_a_real_directory_with_prefix_filter_and_dir_slash() {
        let dir = std::env::temp_dir().join(format!("bish-completion-files-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("widgets")).unwrap();
        std::fs::write(dir.join("widget-notes.txt"), b"hi").unwrap();
        std::fs::write(dir.join("unrelated.txt"), b"hi").unwrap();

        let provider = ShellCompletionProvider { cwd: Some(&dir), known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None };
        let names = display_names(provider.file_candidates("widg"));

        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().any(|n| n == "widgets/"), "{names:?}");
        assert!(names.iter().any(|n| n == "widget-notes.txt"), "{names:?}");
        assert!(!names.iter().any(|n| n == "unrelated.txt"), "{names:?}");
    }

    // Regression test for a real bug caught during interactive
    // verification: `ls ~/.co<Tab>` offered nothing even when `~/.config`
    // exists -- `dir_part` ("~/") was joined onto `cwd` literally (`~`
    // isn't special to the filesystem), producing a nonexistent
    // "$cwd/~/" directory that read_dir always fails to open.
    #[test]
    fn file_candidates_expands_a_leading_tilde_slash_for_the_lookup() {
        let dir = std::env::temp_dir().join(format!("bish-completion-tilde-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".config")).unwrap();

        let original_home = std::env::var("HOME").unwrap_or_default();
        let names = {
            // SAFETY: same reasoning as enumerate_path_matches_filters_by_
            // prefix_and_executable_bit's own PATH mutation above -- no
            // other thread in this test binary depends on HOME being
            // atomically consistent across this narrow window, and the
            // value is restored before returning.
            unsafe { std::env::set_var("HOME", &dir) };
            // cwd is deliberately a different, unrelated directory --
            // confirms the lookup actually goes to $HOME, not cwd.
            let provider = ShellCompletionProvider { cwd: Some(Path::new("/")), known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None };
            let names = display_names(provider.file_candidates("~/.co"));
            unsafe { std::env::set_var("HOME", &original_home) };
            names
        };

        std::fs::remove_dir_all(&dir).ok();

        // The candidate must keep the literal "~/" prefix the user typed
        // (not the expanded $HOME path) -- it gets spliced straight back
        // into the buffer on accept.
        assert_eq!(names, vec!["~/.config/".to_string()]);
    }

    #[test]
    fn file_candidates_yields_nothing_without_a_cwd() {
        let provider = ShellCompletionProvider { cwd: None, known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None };
        assert_eq!(provider.file_candidates("anything"), Vec::new());
    }

    #[test]
    fn complete_dispatches_bare_prefix_to_command_names() {
        let provider = ShellCompletionProvider { cwd: None, known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None };
        let result = provider.complete(CompletionRequest { line: "ech", cursor: 3 });
        assert_eq!(result.word_start, 0);
        let names = display_names(result.candidates);
        assert!(names.iter().any(|n| n == "echo"), "{names:?}");
    }

    #[test]
    fn complete_dispatches_argument_of_unknown_command_to_files() {
        let dir = std::env::temp_dir().join(format!("bish-completion-complete-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("readme.txt"), b"hi").unwrap();

        let provider = ShellCompletionProvider { cwd: Some(&dir), known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None };
        let line = "some-dynamic-cmd read";
        let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
        let word_start = result.word_start;
        let names = display_names(result.candidates);

        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(word_start, "some-dynamic-cmd ".chars().count());
        assert_eq!(names, vec!["readme.txt".to_string()]);
    }

    #[test]
    fn complete_prefers_a_registered_spec_over_the_built_in_file_fallback() {
        let mut completions = HashMap::new();
        completions.insert("fruit".to_string(), compgen::CompgenSpec { wordlist: Some("apple avocado banana".to_string()), ..Default::default() });
        let provider =
            ShellCompletionProvider { cwd: None, known_functions: None, completions: Some(&completions), default_completion: None, action_ctx: None, functions_preamble: None };
        let line = "fruit a";
        let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
        assert_eq!(display_names(result.candidates), vec!["apple".to_string(), "avocado".to_string()]);
    }

    #[test]
    fn complete_falls_back_to_the_default_spec_for_an_unregistered_command() {
        let default_completion = compgen::CompgenSpec { wordlist: Some("defaultA defaultB".to_string()), ..Default::default() };
        let completions = HashMap::new();
        let provider = ShellCompletionProvider {
            cwd: None,
            known_functions: None,
            completions: Some(&completions),
            default_completion: Some(&default_completion),
            action_ctx: None,
            functions_preamble: None,
        };
        let line = "unknowncmd12345 def";
        let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
        assert_eq!(display_names(result.candidates), vec!["defaultA".to_string(), "defaultB".to_string()]);
    }

    #[test]
    fn complete_never_consults_a_registered_spec_when_none_is_supplied() {
        // completions: None (not Some(empty)) -- the "this caller has no
        // completions concept at all" case, distinct from "nothing
        // registered yet" -- must fall straight through to the built-in
        // command-name-candidates path, same as today.
        let provider = ShellCompletionProvider { cwd: None, known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None };
        let line = "ech";
        let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
        assert!(display_names(result.candidates).iter().any(|n| n == "echo"));
    }

    #[test]
    fn as_unranked_candidates_keeps_every_entry_even_ones_not_starting_with_the_prefix() {
        // The whole point of not routing a registered spec's own output
        // through `rank`'s fuzzy_match/drop-non-matches step -- an -F/-C
        // source is trusted to have already applied its own filtering
        // logic, so an entry that doesn't even start with `prefix` must
        // still come through untouched (see compgen.rs's own doc comment).
        let out = as_unranked_candidates("xyz", vec!["foo".to_string(), "xyzzy".to_string()]);
        assert_eq!(display_names(out.clone()), vec!["foo".to_string(), "xyzzy".to_string()]);
        assert_eq!(out[0].matched_positions, Vec::<usize>::new(), "no highlight for a non-matching entry");
        assert_eq!(out[1].matched_positions, vec![0, 1, 2], "highlights the matched prefix span");
    }
}
