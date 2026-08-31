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

/// One candidate the *editor*'s completion popup can show and insert.
///
/// Deliberately not `CompletionCandidate` above: that one is the shell
/// prompt's, and carries fuzzy-match positions for highlighting. This
/// one carries what a language server actually answers with -- a label
/// to show, an elaboration beside it, the text to insert (which is
/// often not the label), and the exact range to replace when the server
/// named one.
///
/// The replace range is in **buffer coordinates** already: whoever
/// builds these owns the position encoding, and resolving it there
/// keeps the editor free of anything protocol-shaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorCompletion {
    pub label: String,
    /// Shown dimmed beside the label -- a kind, a signature, a type.
    pub detail: String,
    pub insert: String,
    /// `(row, start_col, end_col)`, or `None` to replace the word being
    /// typed. A server that names a range is completing something the
    /// editor would not have recognised as one word -- a dotted path,
    /// an import -- so its answer wins where it gave one.
    pub replace: Option<(usize, usize, usize)>,
    /// True when `insert` is a snippet (see `bishedit::snippet`) rather
    /// than finished text: it goes in tentatively, with a caret in its
    /// first tabstop and Tab moving between them, exactly as an `abbr`
    /// expansion does. Only a language server ever sets this -- the
    /// shell's own completions are plain words.
    pub snippet: bool,
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
// Argument completion for a builtin whose arguments are bish's own to
// define, rather than a command's on `$PATH` with a man page. `None`
// means "not one of those, carry on"; `Some` -- even empty -- means this
// command owns its completion outright, the same rule a registered
// `complete` spec follows, so `bishopt --set wrap <Tab>` offers nothing
// rather than falling through and offering files.
//
// `bishopt` and `::bish`, for the same reason: both are registries
// nobody can be expected to remember, and both already print their own
// contents when asked -- this puts that same list under the key people
// actually press.
//
// `hl` is the currently-set `::bish hl` colour names, for the one place
// a fixed list cannot answer: that namespace is open, so what exists is
// whatever has been set. Empty for a caller with no shell to read it
// from (the editor's colon line), which falls back to the names bish
// itself produces.
pub(crate) fn builtin_argument_candidates(role: &CmdRole, prefix_text: &str, prefix: &str, hl: &[String]) -> Option<Vec<CompletionCandidate>> {
    let CmdRole::Argument { command, arg_index } = role else { return None };
    match command.as_deref()? {
        "bishopt" => Some(bishopt_candidates(&preceding_args(prefix_text, *arg_index), prefix)),
        "::bish" => bish_candidates(&preceding_args(prefix_text, *arg_index), prefix, hl),
        _ => None,
    }
}

// `::bish <sub> ...`. `None` means "this position is an ordinary
// command or a free-text value" -- an event's handler, a language
// server's own command line, a CSS colour -- and the generic fallbacks
// should have it instead. `Some(vec![])` would claim the position and
// offer nothing, which is right for `bishopt --set wrap` and wrong
// here.
pub(crate) fn bish_candidates(args: &[String], prefix: &str, hl: &[String]) -> Option<Vec<CompletionCandidate>> {
    let strings = |vs: &[&str]| vs.iter().map(|v| v.to_string()).collect::<Vec<_>>();
    let subs = |sub: &str| strings(crate::exec::bish_sub_subcommands(sub));
    let (sub, rest) = match args.split_first() {
        None => return Some(rank(prefix, strings(crate::exec::bish_subcommands()))),
        Some((sub, rest)) => (sub.as_str(), rest),
    };
    match sub {
        "hl" => Some(hl_candidates(rest, prefix, hl)),
        "lsp" => lsp_candidates(rest, prefix),
        "hook" => hook_candidates(rest, prefix),
        "theme" | "window" | "win" => match rest {
            [] => Some(rank(prefix, subs(sub))),
            // `window rename NAME` and `window select NAME` take a name
            // this cannot know; everything else is already complete.
            _ => Some(Vec::new()),
        },
        _ => Some(Vec::new()),
    }
}

// `::bish hl [NAME | --set NAME COLOUR | --unset NAME]`.
//
// The namespace is open -- a language server's own semantic token type
// names are settable before any version of bish has heard of them (see
// repl::semantic_spans) -- so a completion list here can only ever be a
// starting point, never the set of legal answers. It offers what bish
// itself draws, plus whatever is already set.
fn hl_candidates(args: &[String], prefix: &str, hl: &[String]) -> Vec<CompletionCandidate> {
    let known = || {
        let mut names: Vec<String> = crate::bishedit::highlight::HL_NAMES.iter().map(|(_, name)| name.to_string()).collect();
        for name in hl {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    };
    match args {
        [] if prefix.starts_with('-') => rank(prefix, vec!["--set".to_string(), "--unset".to_string()]),
        // Reading one: anything is a legal question, but the ones that
        // will actually answer are the ones that are set.
        [] => rank(prefix, known()),
        [flag] if flag == "--set" || flag == "-s" => rank(prefix, known()),
        // Unsetting: only what is set can be unset, and saying so is
        // more useful than listing names that would just error.
        [flag] if flag == "--unset" || flag == "-u" => rank(prefix, hl.to_vec()),
        // The colour itself. Free text -- a CSS colour, or a
        // comma-separated list of fallbacks -- so there is nothing to
        // offer, but nor should a file name be offered.
        _ => Vec::new(),
    }
}

// `::bish lsp <sub>`.
fn lsp_candidates(args: &[String], prefix: &str) -> Option<Vec<CompletionCandidate>> {
    let strings = |vs: &[&str]| vs.iter().map(|v| v.to_string()).collect::<Vec<_>>();
    match args.split_first() {
        None => Some(rank(prefix, strings(crate::exec::bish_sub_subcommands("lsp")))),
        Some((sub, rest)) if sub == "add" => {
            // `--apply-edits scoped` -- the separated spelling, where
            // the value is its own word.
            if rest.last().is_some_and(|w| w == "--apply-edits") {
                return Some(rank(prefix, strings(crate::exec::lsp_apply_edits_values())));
            }
            if prefix.starts_with('-') {
                let mut flags = strings(crate::exec::lsp_add_flags());
                // `--apply-edits=` is the one with a fixed set of
                // values, so the whole `--apply-edits=always` is worth
                // offering rather than just the flag.
                if prefix.starts_with("--apply-edits=") {
                    flags.extend(crate::exec::lsp_apply_edits_values().iter().map(|v| format!("--apply-edits={v}")));
                }
                return Some(rank(prefix, flags));
            }
            // Past the flags this is the server's own command line,
            // which is an ordinary command and then its ordinary
            // arguments.
            None
        }
        // `rm`/`log`/`restart` take an id, which is whatever
        // `::bish lsp ls` last printed -- a number, not a name.
        Some(_) => Some(Vec::new()),
    }
}

// `::bish hook <sub>`, whose `add` takes one of a fixed set of events
// and then a command.
fn hook_candidates(args: &[String], prefix: &str) -> Option<Vec<CompletionCandidate>> {
    let strings = |vs: &[&str]| vs.iter().map(|v| v.to_string()).collect::<Vec<_>>();
    match args.split_first() {
        None => Some(rank(prefix, strings(crate::exec::bish_sub_subcommands("hook")))),
        Some((sub, rest)) if sub == "add" || sub == "ls" || sub == "list" => {
            if prefix.starts_with('-') {
                return Some(rank(prefix, vec!["--lang=".to_string()]));
            }
            // Everything before the event is `--lang=GLOB` or
            // `--lang GLOB`; the first word that is neither is the
            // event, and anything after it is the handler command.
            let mut after_flags = rest.iter().peekable();
            let mut positional = 0;
            while let Some(word) = after_flags.next() {
                if word == "--lang" {
                    after_flags.next();
                    continue;
                }
                if word.starts_with("--lang=") {
                    continue;
                }
                positional += 1;
            }
            match (sub.as_str(), positional) {
                ("add", 0) => Some(rank(prefix, strings(crate::exec::HOOK_EVENTS))),
                // The handler: an ordinary command line.
                ("add", _) => None,
                _ => Some(Vec::new()),
            }
        }
        Some(_) => Some(Vec::new()),
    }
}

// The `arg_index` words already typed between the command name and the
// word being completed. Whitespace-split rather than lexed, matching the
// same "plain characters, no quoting" tolerance `find_word_start` is
// documented with -- an option name has never needed quoting.
fn preceding_args(prefix_text: &str, arg_index: usize) -> Vec<String> {
    let words: Vec<&str> = prefix_text.split_whitespace().collect();
    words[words.len().saturating_sub(arg_index)..].iter().map(|w| w.to_string()).collect()
}

pub(crate) fn bishopt_candidates(args: &[String], prefix: &str) -> Vec<CompletionCandidate> {
    let takes_a_name = |w: &String| matches!(w.as_str(), "--set" | "-s" | "--unset" | "-u" | "--quiet" | "-q");
    let names = || crate::exec::bishopt_names().into_iter().map(|n| n.to_string()).collect::<Vec<_>>();
    let strings = |vs: &[&str]| vs.iter().map(|v| v.to_string()).collect::<Vec<_>>();
    match args {
        // The long spellings only: `-s`/`-u`/`-q` work and are
        // understood above, but a completion list is a menu, and a menu
        // should show the name that says what it does.
        [] if prefix.starts_with('-') => rank(prefix, strings(&["--set", "--unset", "--quiet"])),
        // `bishopt <Tab>`, and `bishopt --set <Tab>`: an option name.
        [] => rank(prefix, names()),
        [flag] if takes_a_name(flag) => rank(prefix, names()),
        // `bishopt --set wrap <Tab>`: the value, when there is a fixed
        // set of them to offer.
        [flag, name] if flag == "--set" || flag == "-s" => rank(prefix, strings(crate::exec::bishopt_values(name))),
        // A bare `bishopt NAME` reads it and takes nothing more, and
        // `--unset NAME` is already complete.
        _ => Vec::new(),
    }
}

// What the editor's own colon line gets. A full `ShellCompletionProvider`
// has nothing to be built from there (no single current session, and
// command mode types window-management subcommands rather than shell
// command lines -- see run_command_mode's own doc comment), but the
// builtins whose arguments bish defines need no shell context at all, so
// they can be offered anywhere a line is being typed.
pub struct BuiltinCompletionProvider {
    /// The same `::bish hl` snapshot `ShellCompletionProvider` takes.
    /// The colon line has no shell *completion* context -- no cwd, no
    /// PATH, no registered specs -- but it does have a session behind
    /// it, and changing a colour is something you do while looking at
    /// the buffer it changes, so this one snapshot is worth carrying.
    pub hl_names: Vec<String>,
}

impl CompletionProvider for BuiltinCompletionProvider {
    fn complete(&self, req: CompletionRequest) -> CompletionResult {
        let chars: Vec<char> = req.line.chars().collect();
        let cursor = req.cursor.min(chars.len());
        let word_start = find_word_start(&chars, cursor);
        let prefix: String = chars[word_start..cursor].iter().collect();
        let prefix_text: String = chars[..word_start].iter().collect();
        let role = classify_word_role(&prefix_text);
        let candidates = builtin_argument_candidates(&role, &prefix_text, &prefix, &self.hl_names).unwrap_or_default();
        CompletionResult { word_start, candidates }
    }
}

// The commands whose arguments are hosts, and where.
//
// `ssh`, `sftp`, `mosh` and `ssh-copy-id` take exactly one host and then
// (for ssh) a *remote command*, which nothing here can know anything
// about -- so only the first argument is a host position. `scp` and
// `rsync` take `[user@]host:path` in any argument position, and either
// side of the copy can be local, so those positions offer hosts *and*
// files together rather than replacing one with the other.
fn host_position(command: Option<&str>, arg_index: usize) -> bool {
    match command {
        Some("ssh" | "sftp" | "mosh" | "ssh-copy-id") => arg_index == 0,
        Some("scp" | "rsync") => true,
        _ => false,
    }
}

// A host argument's own shape: `[user@]host[:path]`. Completion applies
// to the host part alone, with whatever came before it kept -- typing
// `ssh deploy@we<Tab>` should finish the host, not offer to replace the
// user with one.
fn host_word(prefix: &str) -> Option<(&str, &str)> {
    // A `:` means the host is already complete and a path has started;
    // remote paths are not something this can enumerate.
    if prefix.contains(':') {
        return None;
    }
    Some(match prefix.rsplit_once('@') {
        Some((user, host)) => (&prefix[..user.len() + 1], host),
        None => ("", prefix),
    })
}

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
    /// The `gitignore` bishopt. `false` for a caller with no shell to
    /// read it from, which offers everything -- the behaviour before
    /// this existed.
    pub honor_gitignore: bool,
    /// The `::bish hl` colour names currently set (a per-prompt
    /// snapshot of `Shell::hl_colors`'s keys), for completing
    /// `::bish hl --unset <Tab>`.
    ///
    /// Needed as a snapshot rather than a fixed list because that
    /// namespace is open: what exists is whatever has been set, which
    /// for a language server's semantic token types is a set no version
    /// of bish knows in advance. Empty for a caller with no shell.
    pub hl_names: Vec<String>,
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
        // After a user's own `complete` spec (which owns a command
        // outright, per real bash) but before the generic flag/
        // subcommand/file fallbacks, since those know nothing about a
        // builtin whose arguments bish itself defines.
        if let Some(candidates) = builtin_argument_candidates(&role, &prefix_text, &prefix, &self.hl_names) {
            return CompletionResult { word_start, candidates };
        }

        let candidates = match role {
            CmdRole::Command => self.command_name_candidates(&prefix),
            CmdRole::Argument { command, .. } if prefix.starts_with('-') => self.flag_candidates(command.as_deref(), &prefix),
            // Before the subcommand/file fallbacks: `ssh web` is not a
            // path and never will be, and a man page has nothing to say
            // about which hosts *this machine* knows.
            CmdRole::Argument { command, arg_index } if host_position(command.as_deref(), arg_index) => {
                self.host_candidates(command.as_deref(), &prefix)
            }
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

    // The hosts this machine knows about (`hosts::known` -- ssh config,
    // known_hosts, /etc/hosts), plus, for the commands where an argument
    // can be either end of a copy, the local files beside them.
    fn host_candidates(&self, command: Option<&str>, prefix: &str) -> Vec<CompletionCandidate> {
        let also_files = matches!(command, Some("scp" | "rsync"));
        let Some((keep, _)) = host_word(prefix) else {
            // Past the `:`, so this is a remote path -- nothing here can
            // enumerate one, and offering local files for it would be
            // offering the wrong machine's.
            return Vec::new();
        };
        let mut names: Vec<String> = crate::hosts::known().into_iter().map(|h| format!("{keep}{h}")).collect();
        // `scp host:` wants the colon, since the path follows it on the
        // same word; `ssh host` is finished as it stands.
        if also_files {
            names = names.into_iter().map(|n| format!("{n}:")).collect();
            names.extend(self.file_candidates(prefix).into_iter().map(|c| c.display));
        }
        rank(prefix, names)
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
        // The `gitignore` bishopt, same one the browser reads. Built
        // once for the directory being listed rather than per entry --
        // and only when Tab was actually pressed, so an ordinary
        // keystroke never touches the filesystem for it.
        let ignore = self.honor_gitignore.then(|| crate::gitignore::Stack::for_directory(&dir_path));
        let mut names = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            // `target/` and `node_modules/` are the whole reason to
            // bother: a completion list they are in is a completion list
            // you have to read past.
            if let Some(ignore) = &ignore
                && ignore.matched(&entry.path(), is_dir).is_ignored()
            {
                continue;
            }
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

    fn strs(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn display_names(candidates: Vec<CompletionCandidate>) -> Vec<String> {
        candidates.into_iter().map(|c| c.display).collect()
    }

    #[test]
    fn command_name_candidates_includes_known_builtins_matching_prefix() {
        let provider = ShellCompletionProvider { cwd: None, known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None, honor_gitignore: false, hl_names: Vec::new() };
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
        let provider = ShellCompletionProvider { cwd: None, known_functions: Some(&functions), completions: None, default_completion: None, action_ctx: None, functions_preamble: None, honor_gitignore: false, hl_names: Vec::new() };
        let names = display_names(provider.command_name_candidates("zz_bish_test"));
        assert_eq!(names, vec!["zz_bish_test_func".to_string()]);
    }

    #[test]
    fn command_name_candidates_includes_a_real_path_executable() {
        // coreutils -- same real-PATH assumption this whole feature
        // already leans on elsewhere (highlight.rs's own is_in_path
        // tests).
        let provider = ShellCompletionProvider { cwd: None, known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None, honor_gitignore: false, hl_names: Vec::new() };
        let names = display_names(provider.command_name_candidates("tru"));
        assert!(names.iter().any(|n| n == "true"), "{names:?}");
    }

    #[test]
    fn flag_candidates_core_ranks_flags_by_prefix() {
        let man = manpages::ManPageData {
            flags: vec!["-l".to_string(), "--long".to_string(), "-a".to_string()],
            subcommands: vec![],
            name_section: None,
            flag_descriptions: std::collections::HashMap::new(),
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
        let man = manpages::ManPageData { flags: vec!["-l".to_string()], subcommands: vec![], name_section: None, flag_descriptions: std::collections::HashMap::new() };
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
            name_section: None,
            flag_descriptions: std::collections::HashMap::new(),
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

        let provider = ShellCompletionProvider { cwd: Some(&dir), known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None, honor_gitignore: false, hl_names: Vec::new() };
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
            let provider = ShellCompletionProvider { cwd: Some(Path::new("/")), known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None, honor_gitignore: false, hl_names: Vec::new() };
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
        let provider = ShellCompletionProvider { cwd: None, known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None, honor_gitignore: false, hl_names: Vec::new() };
        assert_eq!(provider.file_candidates("anything"), Vec::new());
    }

    #[test]
    fn complete_dispatches_bare_prefix_to_command_names() {
        let provider = ShellCompletionProvider { cwd: None, known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None, honor_gitignore: false, hl_names: Vec::new() };
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

        let provider = ShellCompletionProvider { cwd: Some(&dir), known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None, honor_gitignore: false, hl_names: Vec::new() };
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
            ShellCompletionProvider { cwd: None, known_functions: None, completions: Some(&completions), default_completion: None, action_ctx: None, functions_preamble: None, honor_gitignore: false, hl_names: Vec::new() };
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
            honor_gitignore: false,
            hl_names: Vec::new(),
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
        let provider = ShellCompletionProvider { cwd: None, known_functions: None, completions: None, default_completion: None, action_ctx: None, functions_preamble: None, honor_gitignore: false, hl_names: Vec::new() };
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

    fn bishopt_at(line: &str) -> Vec<String> {
        let cursor = line.chars().count();
        BuiltinCompletionProvider { hl_names: Vec::new() }
            .complete(CompletionRequest { line, cursor })
            .candidates
            .into_iter()
            .map(|c| c.display)
            .collect()
    }

    // `::bish` is reachable from the editor's colon line as well as the
    // shell prompt, and the colon line is where most of it gets typed.
    fn bish_at(line: &str) -> Vec<String> {
        bishopt_at(line)
    }

    #[test]
    fn bish_completes_its_own_subcommands() {
        let names = bish_at("::bish ");
        let mut expected: Vec<String> = crate::exec::bish_subcommands().iter().map(|s| s.to_string()).collect();
        expected.sort();
        let mut got = names.clone();
        got.sort();
        assert_eq!(got, expected, "the list is the dispatcher's, nothing invented and nothing missing");
        assert_eq!(bish_at("::bish ls"), vec!["lsp".to_string()], "fuzzy, like every other list here");
    }

    #[test]
    fn bish_completes_the_second_level_of_each_subcommand() {
        for sub in crate::exec::bish_subcommands() {
            let offered = bish_at(&format!("::bish {sub} "));
            let expected = crate::exec::bish_sub_subcommands(sub);
            for name in expected {
                assert!(offered.contains(&name.to_string()), "::bish {sub} <Tab> should offer {name}, got {offered:?}");
            }
        }
        assert!(bish_at("::bish lsp ").contains(&"restart".to_string()));
        assert!(bish_at("::bish theme ").contains(&"begin".to_string()));
    }

    #[test]
    fn bish_lsp_add_completes_its_flags_and_the_one_flag_with_fixed_values() {
        let flags = bish_at("::bish lsp add --");
        assert!(flags.contains(&"--lang=".to_string()) && flags.contains(&"--apply-edits=".to_string()), "{flags:?}");
        // Both spellings of the value: attached to the flag...
        let attached = bish_at("::bish lsp add --apply-edits=");
        assert!(attached.contains(&"--apply-edits=scoped".to_string()), "{attached:?}");
        // ...and as its own word.
        assert_eq!(bish_at("::bish lsp add --apply-edits ").len(), crate::exec::lsp_apply_edits_values().len());
        assert!(bish_at("::bish lsp add --apply-edits ").contains(&"always".to_string()));
    }

    // The server's own command line is an ordinary command line, so this
    // position must be *given up* rather than claimed and answered with
    // nothing -- the distinction `builtin_argument_candidates`'
    // `Option` exists for.
    #[test]
    fn bish_lsp_add_hands_the_server_command_back_to_the_ordinary_completions() {
        assert!(bish_candidates(&strs(&["lsp", "add"]), "rust-ana", &[]).is_none());
        assert!(bish_candidates(&strs(&["lsp", "add", "--lang=rust"]), "rust-ana", &[]).is_none());
        // And a hook's handler, for the same reason -- but only after
        // its event, which is a fixed list.
        assert!(bish_candidates(&strs(&["hook", "add"]), "editor", &[]).is_some());
        assert!(bish_candidates(&strs(&["hook", "add", "editor:file:open"]), "my_", &[]).is_none());
    }

    #[test]
    fn bish_hook_add_completes_the_event_from_the_real_list() {
        let events = bish_at("::bish hook add ");
        let mut expected: Vec<String> = crate::exec::HOOK_EVENTS.iter().map(|e| e.to_string()).collect();
        expected.sort();
        let mut got = events;
        got.sort();
        assert_eq!(got, expected);
        // `--lang=` first is the same position, since it is a flag and
        // not the event.
        assert!(bish_at("::bish hook add --lang=rust ").contains(&"editor:file:open".to_string()));
    }

    // The namespace is open, so this list can only ever be a starting
    // point -- but `--unset` is the exception: only what is set can be
    // unset, and offering names that would just error is worse than
    // offering none.
    #[test]
    fn bish_hl_completes_known_names_to_set_and_only_live_ones_to_unset() {
        let set = display_names(hl_candidates(&strs(&["--set"]), "", &[]));
        assert!(set.contains(&"string".to_string()) && set.contains(&"keyword".to_string()), "{set:?}");

        let live = strs(&["parameter", "string"]);
        let with_live = display_names(hl_candidates(&strs(&["--set"]), "", &live));
        assert!(with_live.contains(&"parameter".to_string()), "a name only a server ever produced is still offered once set");
        assert_eq!(with_live.iter().filter(|n| *n == "string").count(), 1, "and one that is both is offered once");

        let unset = display_names(hl_candidates(&strs(&["--unset"]), "", &live));
        assert_eq!(unset.len(), 2, "only the two that are set: {unset:?}");
        assert!(display_names(hl_candidates(&strs(&["--unset"]), "", &[])).is_empty(), "nothing set, nothing to unset");

        // The colour itself is free text, but a file name would be
        // wrong, so the position is claimed and answered with nothing.
        assert!(hl_candidates(&strs(&["--set", "string"]), "#ff", &[]).is_empty());
        assert!(bish_candidates(&strs(&["hl", "--set", "string"]), "#ff", &[]).is_some());

        let flags = bish_at("::bish hl --");
        assert!(flags.contains(&"--set".to_string()) && flags.contains(&"--unset".to_string()), "{flags:?}");

        // And the snapshot really does reach the colon line's own
        // provider, which is the one place it had to be threaded rather
        // than read.
        let line = "::bish hl --unset ";
        let colon = BuiltinCompletionProvider { hl_names: strs(&["colonLineOnly"]) };
        let found = colon.complete(CompletionRequest { line, cursor: line.chars().count() });
        assert_eq!(display_names(found.candidates), strs(&["colonLineOnly"]));
    }

    // Ordered by `rank` like every other candidate list here, which for
    // an empty prefix means alphabetically -- so this compares sets, and
    // what it pins is that the list *is* the registry: nothing invented,
    // nothing missed, and a new option completable by existing.
    #[test]
    fn bishopt_completes_option_names_from_the_real_registry() {
        let names = bishopt_at("bishopt ");
        let mut expected = crate::exec::bishopt_names();
        expected.sort();
        assert_eq!(names, expected, "every option, and nothing invented");
        assert!(bishopt_at("bishopt wr").contains(&"wrap".to_string()));
        assert!(bishopt_at("bishopt wr").contains(&"wrap_column".to_string()));
    }

    #[test]
    fn bishopt_completes_a_name_after_every_flag_that_takes_one() {
        for flag in ["--set", "-s", "--unset", "-u", "--quiet", "-q"] {
            assert!(bishopt_at(&format!("bishopt {flag} tab")).contains(&"tabular".to_string()), "{flag}");
        }
    }

    #[test]
    fn bishopt_completes_the_flags_themselves() {
        let mut flags = bishopt_at("bishopt --");
        flags.sort();
        assert_eq!(flags, vec!["--quiet", "--set", "--unset"]);
    }

    // A boolean is the one kind of option with a fixed set of values;
    // anything else is free text, and guessing at it would be inventing.
    #[test]
    fn bishopt_completes_on_and_off_for_a_boolean_value_only() {
        let mut values = bishopt_at("bishopt --set wrap ");
        values.sort();
        assert_eq!(values, vec!["off", "on"]);
        assert_eq!(bishopt_at("bishopt --set showbreak "), Vec::<String>::new());
        assert_eq!(bishopt_at("bishopt --set wrap_column "), Vec::<String>::new());
    }

    // `Some(empty)` rather than `None`: bishopt owns its own completion,
    // so a position with nothing to offer offers nothing, instead of
    // falling through to file names.
    #[test]
    fn bishopt_never_falls_through_to_files() {
        let role = CmdRole::Argument { command: Some("bishopt".to_string()), arg_index: 3 };
        assert_eq!(builtin_argument_candidates(&role, "bishopt --set wrap on ", "", &[]), Some(Vec::new()));
        assert_eq!(bishopt_at("bishopt wrap "), Vec::<String>::new());
    }

    #[test]
    fn the_builtin_provider_says_nothing_about_anything_else() {
        assert_eq!(bishopt_at("ls "), Vec::<String>::new());
        assert_eq!(bishopt_at("bish"), Vec::<String>::new(), "a command name is not this provider's job");
        assert_eq!(bishopt_at(""), Vec::<String>::new());
    }

    // The shell prompt gets the same answers through its own provider.
    #[test]
    fn the_shell_provider_completes_bishopt_options_too() {
        let provider = ShellCompletionProvider {
            cwd: None,
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            hl_names: Vec::new(),
        };
        let line = "bishopt --set wr";
        let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
        let names: Vec<String> = result.candidates.into_iter().map(|c| c.display).collect();
        assert!(names.contains(&"wrap".to_string()));
    }

    #[test]
    fn host_positions_are_where_a_host_can_actually_go() {
        // One host, then a remote command nothing here can know about.
        assert!(host_position(Some("ssh"), 0));
        assert!(!host_position(Some("ssh"), 1));
        assert!(host_position(Some("sftp"), 0));
        // Either end of a copy can be remote, in any position.
        assert!(host_position(Some("scp"), 0));
        assert!(host_position(Some("scp"), 2));
        assert!(host_position(Some("rsync"), 1));
        // Everything else is somebody else's business.
        assert!(!host_position(Some("ls"), 0));
        assert!(!host_position(None, 0));
    }

    // `[user@]host` -- completing the host keeps the user, rather than
    // offering to replace it with one.
    #[test]
    fn a_user_prefix_is_kept_and_the_host_is_what_completes() {
        assert_eq!(host_word("we"), Some(("", "we")));
        assert_eq!(host_word("deploy@we"), Some(("deploy@", "we")));
        assert_eq!(host_word(""), Some(("", "")));
    }

    // Past the `:` this is a *remote* path, and nothing here can
    // enumerate one -- offering local files for it would be offering the
    // wrong machine's.
    #[test]
    fn a_colon_ends_host_completion() {
        assert_eq!(host_word("web:"), None);
        assert_eq!(host_word("web:/var/l"), None);
        assert_eq!(host_word("deploy@web:/etc"), None);
    }

    // The dispatch, not the host list: `ssh web` must not fall through
    // to the file completion it used to get.
    #[test]
    fn ssh_does_not_complete_files() {
        let dir = std::env::temp_dir().join(format!("bish-completion-ssh-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zzz-not-a-host.txt"), "").unwrap();
        let provider = ShellCompletionProvider {
            cwd: Some(&dir),
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            hl_names: Vec::new(),
        };
        let line = "ssh zzz";
        let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
        let names: Vec<String> = result.candidates.into_iter().map(|c| c.display).collect();
        assert!(!names.iter().any(|n| n.contains("zzz-not-a-host")), "a local file is never an ssh target: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ...whereas for scp it is: either end of a copy can be local.
    #[test]
    fn scp_completes_files_as_well_as_hosts() {
        let dir = std::env::temp_dir().join(format!("bish-completion-scp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zzz-local.txt"), "").unwrap();
        let provider = ShellCompletionProvider {
            cwd: Some(&dir),
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            hl_names: Vec::new(),
        };
        let line = "scp zzz";
        let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
        let names: Vec<String> = result.candidates.into_iter().map(|c| c.display).collect();
        assert!(names.iter().any(|n| n.contains("zzz-local")), "got {names:?}");
    }

    // `target/` in a completion list is a list you have to read past.
    #[test]
    fn file_completion_skips_gitignored_entries() {
        let dir = std::env::temp_dir().join(format!("bish-completion-ignore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("tangent.txt"), "").unwrap();

        let names = |honor| {
            let provider = ShellCompletionProvider {
                cwd: Some(&dir),
                known_functions: None,
                completions: None,
                default_completion: None,
                action_ctx: None,
                functions_preamble: None,
                honor_gitignore: honor,
                hl_names: Vec::new(),
            };
            let line = "cat ta";
            provider
                .complete(CompletionRequest { line, cursor: line.chars().count() })
                .candidates
                .into_iter()
                .map(|c| c.display)
                .collect::<Vec<_>>()
        };
        let offered = names(true);
        assert!(offered.contains(&"tangent.txt".to_string()), "got {offered:?}");
        assert!(!offered.iter().any(|n| n.starts_with("target")), "target/ is ignored: {offered:?}");
        // ...and with the option off it is offered like anything else.
        assert!(names(false).iter().any(|n| n.starts_with("target")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
