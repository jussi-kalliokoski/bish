// Tab completion: the candidate and provider types, the walker that decides
// what kind of word is under the cursor (command name, flag, subcommand or
// file), and the shell-backed candidate sources.

use crate::bishedit::fuzzy;
use crate::bishedit::highlight::{self, is_assignment_prefix_word, resets_command_position};
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
    // A char index, as the line editor's cursor is.
    pub cursor: usize,
}

pub struct CompletionResult {
    // Char index. `word_start..cursor` is the replaced range; text after
    // the cursor is never touched.
    pub word_start: usize,
    pub candidates: Vec<CompletionCandidate>,
}

/// One candidate the editor's completion popup can show and insert. Unlike
/// `CompletionCandidate` (the shell prompt's, with fuzzy-match positions),
/// this carries what a language server answers with: a label, a detail, the
/// text to insert (often not the label), and the range to replace.
///
/// The range is already in buffer coordinates: whoever builds these owns
/// the position encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorCompletion {
    pub label: String,
    /// Shown dimmed beside the label: a kind, a signature, a type.
    pub detail: String,
    pub insert: String,
    /// `(row, start_col, end_col)`, or `None` to replace the word being
    /// typed. A server that names a range is completing something that
    /// isn't one word to the editor (a dotted path, an import), so its
    /// range wins.
    pub replace: Option<(usize, usize, usize)>,
    /// `insert` is a snippet (see `bishedit::snippet`): it goes in
    /// tentatively, caret in the first tabstop, Tab moving between them, as
    /// an `abbr` expansion does. Only a language server sets this.
    pub snippet: bool,
}

pub trait CompletionProvider {
    fn complete(&self, req: CompletionRequest) -> CompletionResult;
}

// Whitespace and `| & ; ( ) < >` are boundaries. Quotes, `$` and backslash
// aren't: this scans characters and doesn't understand quoting.
fn is_word_char(c: char) -> bool {
    !(c.is_whitespace() || matches!(c, '|' | '&' | ';' | '(' | ')' | '<' | '>'))
}

// Scans back from `cursor` to the enclosing boundary (or 0), so
// `chars[0..word_start]` is a complete prefix `classify_word_role` can
// re-tokenize on its own.
pub(crate) fn find_word_start(chars: &[char], cursor: usize) -> usize {
    let mut i = cursor.min(chars.len());
    while i > 0 && is_word_char(chars[i - 1]) {
        i -= 1;
    }
    i
}

/// Where the alternative under the cursor starts inside a word with an
/// unclosed `{`, and how much of the word before it every alternative
/// inherits. In `mv somedir/{old,ne`, `ne` means `somedir/ne`: the
/// inherited text decides what to look for, and only the alternative may be
/// replaced.
///
/// Returns `(end of the inherited text, start of this alternative)` as char
/// indices into `word`, or `None` outside a brace list, including one that
/// closed again (`{old,new}ex`).
///
/// Not understood: a `${...}` inside a brace list (its `}` is counted as
/// the list's), and quoting (see `is_word_char`).
fn brace_alternative(word: &[char]) -> Option<(usize, usize)> {
    // Per open brace, where its current alternative starts, so closing a
    // nested list hands the enclosing one back.
    let mut open: Vec<usize> = Vec::new();
    let mut inherited = 0;
    let mut i = 0;
    while i < word.len() {
        match word[i] {
            // Whatever follows a backslash is literal, brace included.
            '\\' => i += 1,
            // `${v}` is a parameter expansion, not a list of one.
            '$' if word.get(i + 1) == Some(&'{') => i += 1,
            '{' => {
                if open.is_empty() {
                    inherited = i;
                }
                open.push(i + 1);
            }
            '}' => {
                open.pop();
            }
            ',' => {
                if let Some(alternative) = open.last_mut() {
                    *alternative = i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    open.last().map(|alternative| (inherited, *alternative))
}

/// Re-roots candidates built for the inherited text plus the alternative
/// onto the alternative alone, fuzzy-match positions included.
fn strip_inherited(candidates: Vec<CompletionCandidate>, inherited: &str) -> Vec<CompletionCandidate> {
    let skipped = inherited.chars().count();
    candidates
        .into_iter()
        .filter_map(|c| {
            let display: String = c.display.chars().skip(skipped).collect();
            // `file_candidates` builds every candidate from the inherited
            // text, so this only drops something a registered spec
            // invented.
            c.display.starts_with(inherited).then(|| CompletionCandidate {
                display,
                matched_positions: c.matched_positions.iter().filter(|p| **p >= skipped).map(|p| p - skipped).collect(),
            })
        })
        .collect()
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

// Re-tokenizes `prefix_text` (a complete prefix, per `find_word_start`) and
// walks it with the same ExpectCommand/InCommand states as highlight.rs's
// walker, using its `resets_command_position` and
// `is_assignment_prefix_word`. The state after the last token is the role
// of the word being completed.
//
// An unclosed `$(...)` and the like degrade to whatever tokens the lexer
// collected (see `SpannedResult::error`): a role that may be wrong, never a
// panic.
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

// Fuzzy-scores and ranks `names` against `prefix`, dropping non-matches.
// Ties break alphabetically so the order is stable across sources.
fn rank(prefix: &str, names: Vec<String>) -> Vec<CompletionCandidate> {
    let mut scored: Vec<(i32, bool, CompletionCandidate)> = names
        .into_iter()
        .filter_map(|name| {
            let hidden = hidden_name(&name) && !asking_for_hidden(prefix);
            fuzzy::fuzzy_match(prefix, &name).map(|m| (m.score, hidden, CompletionCandidate { display: name, matched_positions: m.positions }))
        })
        .collect();
    // Hidden names sort first, ahead of the score: a dotfile is unlikely
    // however well it matches, and the score mostly tracks name length.
    crate::sort::sort_by(&mut scored, |a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)).then_with(|| a.2.display.cmp(&b.2.display)));
    scored.into_iter().map(|(_, _, c)| c).collect()
}

/// Whether the last path segment starts with a dot: `src/.gitignore` is
/// hidden and `./main.rs` isn't.
fn hidden_name(name: &str) -> bool {
    let bare = name.strip_suffix('/').unwrap_or(name);
    bare.rsplit('/').next().is_some_and(|seg| seg.starts_with('.'))
}

/// Whether the word is reaching for a hidden name, in which case they sort
/// like anything else. `.`, `..` and `./` start ordinary relative paths and
/// don't count.
fn asking_for_hidden(prefix: &str) -> bool {
    let last = prefix.rsplit('/').next().unwrap_or(prefix);
    last.starts_with('.') && !matches!(last, "." | "..")
}

// A registered completion spec's candidates, unfiltered and in its own
// order: `-F`/`-C` sources are trusted to have applied their own logic (see
// compgen.rs), and fuzzy filtering could drop a real COMPREPLY entry. The
// prefix is still highlighted where a candidate starts with it.
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

// The command's man-page data if ready; `Pending` and `Missing` both mean
// nothing to offer yet.
fn ready_man_data(command: &str) -> Option<std::sync::Arc<manpages::ManPageData>> {
    match manpages::query(command) {
        manpages::ManStatus::Ready(data) => Some(data),
        manpages::ManStatus::Pending | manpages::ManStatus::Missing => None,
    }
}

// Takes the man data directly, so tests needn't spawn `man`.
fn flag_candidates_core(man: Option<&manpages::ManPageData>, prefix: &str) -> Vec<CompletionCandidate> {
    let Some(man) = man else { return Vec::new() };
    rank(prefix, man.flags.clone())
}

// `None` means no usable subcommand data, and the caller falls through to
// files.
fn subcommand_candidates_core(man: Option<&manpages::ManPageData>, prefix: &str) -> Option<Vec<CompletionCandidate>> {
    let man = man?;
    if man.subcommands.is_empty() {
        return None;
    }
    Some(rank(prefix, man.subcommands.clone()))
}

// Arguments for a builtin whose arguments bish defines (`bishopt`,
// `::bish`). `None` means not one of those; `Some`, even empty, means this
// command owns its completion, as with a registered `complete` spec, so
// `bishopt --set wrap <Tab>` offers nothing rather than files.
//
// `hl` is the currently set `::bish hl` names, since that namespace is
// open. Empty for a caller with no shell (the editor's colon line), which
// falls back to the names bish produces.
pub(crate) fn builtin_argument_candidates(role: &CmdRole, prefix_text: &str, prefix: &str, hl: &[String]) -> Option<Vec<CompletionCandidate>> {
    let CmdRole::Argument { command, arg_index } = role else { return None };
    match command.as_deref()? {
        "bishopt" => Some(bishopt_candidates(&preceding_args(prefix_text, *arg_index), prefix)),
        "::bish" => bish_candidates(&preceding_args(prefix_text, *arg_index), prefix, hl),
        _ => None,
    }
}

// `::bish <sub> ...`. `None` means the position is a command or free text
// (an event handler, a server command line, a CSS colour) and the generic
// fallbacks should have it. `Some(vec![])` would claim it and offer
// nothing.
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
            // this can't know.
            _ => Some(Vec::new()),
        },
        // Only the flags are completed. A directory or capability is the
        // user's to name, and plain `::bish trust` takes a path, which file
        // completion offers.
        "trust" => match rest {
            [] => Some(rank(prefix, subs(sub))),
            _ => Some(Vec::new()),
        },
        _ => Some(Vec::new()),
    }
}

// `::bish hl [NAME | --set NAME COLOUR | --unset NAME]`. The namespace is
// open (a server's semantic token types are settable; see
// `repl::semantic_spans`), so this offers what bish draws plus whatever is
// set.
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
        [] => rank(prefix, known()),
        [flag] if flag == "--set" || flag == "-s" => rank(prefix, known()),
        // Only what is set can be unset.
        [flag] if flag == "--unset" || flag == "-u" => rank(prefix, hl.to_vec()),
        // The colour is free text, so nothing is offered, files included.
        _ => Vec::new(),
    }
}

// `::bish lsp <sub>`.
fn lsp_candidates(args: &[String], prefix: &str) -> Option<Vec<CompletionCandidate>> {
    let strings = |vs: &[&str]| vs.iter().map(|v| v.to_string()).collect::<Vec<_>>();
    match args.split_first() {
        None => Some(rank(prefix, strings(crate::exec::bish_sub_subcommands("lsp")))),
        Some((sub, rest)) if sub == "add" => {
            // `--apply-edits scoped`, the value as its own word.
            if rest.last().is_some_and(|w| w == "--apply-edits") {
                return Some(rank(prefix, strings(crate::exec::lsp_apply_edits_values())));
            }
            if prefix.starts_with('-') {
                let mut flags = strings(crate::exec::lsp_add_flags());
                // `--apply-edits=` has a fixed set of values, so the whole
                // `--apply-edits=always` is offered.
                if prefix.starts_with("--apply-edits=") {
                    flags.extend(crate::exec::lsp_apply_edits_values().iter().map(|v| format!("--apply-edits={v}")));
                }
                return Some(rank(prefix, flags));
            }
            // Past the flags, the server's own command line.
            None
        }
        // `rm`, `log` and `restart` take an id from `::bish lsp ls`.
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
            // Before the event come `--lang=GLOB` or `--lang GLOB`; the
            // first other word is the event, and the rest is the handler.
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
                ("add", _) => None,
                _ => Some(Vec::new()),
            }
        }
        Some(_) => Some(Vec::new()),
    }
}

// The `arg_index` words between the command name and the word being
// completed, split on whitespace (see `find_word_start`).
fn preceding_args(prefix_text: &str, arg_index: usize) -> Vec<String> {
    let words: Vec<&str> = prefix_text.split_whitespace().collect();
    words[words.len().saturating_sub(arg_index)..].iter().map(|w| w.to_string()).collect()
}

pub(crate) fn bishopt_candidates(args: &[String], prefix: &str) -> Vec<CompletionCandidate> {
    let takes_a_name = |w: &String| matches!(w.as_str(), "--set" | "-s" | "--unset" | "-u" | "--quiet" | "-q");
    let names = || crate::exec::bishopt_names().into_iter().map(|n| n.to_string()).collect::<Vec<_>>();
    let strings = |vs: &[&str]| vs.iter().map(|v| v.to_string()).collect::<Vec<_>>();
    match args {
        // The long spellings only: a menu should show the name that says
        // what it does.
        [] if prefix.starts_with('-') => rank(prefix, strings(&["--set", "--unset", "--quiet"])),
        [] => rank(prefix, names()),
        [flag] if takes_a_name(flag) => rank(prefix, names()),
        // `bishopt --set wrap <Tab>`: the value, when there's a fixed set.
        [flag, name] if flag == "--set" || flag == "-s" => rank(prefix, strings(crate::exec::bishopt_values(name))),
        // `bishopt NAME` takes nothing more, and `--unset NAME` is
        // complete.
        _ => Vec::new(),
    }
}

// What the editor's colon line gets. There's no shell context there for a
// full `ShellCompletionProvider`, but the builtins whose arguments bish
// defines need none.
pub struct BuiltinCompletionProvider {
    /// The same `::bish hl` snapshot `ShellCompletionProvider` takes: a
    /// colour is changed while looking at the buffer it changes.
    pub hl_names: Vec<String>,
    /// The line's own command names (`repl::COLON_COMMANDS`), which no PATH
    /// scan finds. Empty when the line has none beyond the shell's.
    pub commands: &'static [&'static str],
}

impl CompletionProvider for BuiltinCompletionProvider {
    fn complete(&self, req: CompletionRequest) -> CompletionResult {
        let chars: Vec<char> = req.line.chars().collect();
        let cursor = req.cursor.min(chars.len());
        let word_start = find_word_start(&chars, cursor);
        let prefix: String = chars[word_start..cursor].iter().collect();
        let prefix_text: String = chars[..word_start].iter().collect();
        let role = classify_word_role(&prefix_text);
        let candidates = match role {
            CmdRole::Command => rank(&prefix, self.commands.iter().map(|c| c.to_string()).collect()),
            CmdRole::Argument { .. } => builtin_argument_candidates(&role, &prefix_text, &prefix, &self.hl_names).unwrap_or_default(),
        };
        CompletionResult { word_start, candidates }
    }
}

/// Commands whose arguments are directories rather than any file. Written
/// down because there's no general way to know: man-page data is fetched in
/// the background and still `Pending` on the first Tab, which is the one
/// that needs it. Anything else is `complete -d -c name`, which this
/// doesn't override.
///
/// Every argument, not a position: none of these takes anything else
/// afterwards.
fn directory_position(command: Option<&str>) -> bool {
    matches!(
        command,
        Some("cd" | "pushd" | "chdir" | "rmdir")
        // `mkdir a/b` names a directory that doesn't exist yet; its parent
        // is still completed as one.
        | Some("mkdir")
    )
}

// Commands whose arguments are hosts, and where. `ssh`, `sftp`, `mosh` and
// `ssh-copy-id` take one host, then (for ssh) a remote command, so only the
// first argument is a host. `scp` and `rsync` take `[user@]host:path`
// anywhere and either side can be local, so those offer hosts and files
// together.
fn host_position(command: Option<&str>, arg_index: usize) -> bool {
    match command {
        Some("ssh" | "sftp" | "mosh" | "ssh-copy-id") => arg_index == 0,
        Some("scp" | "rsync") => true,
        _ => false,
    }
}

// `[user@]host[:path]`: only the host part is completed, so
// `ssh deploy@we<Tab>` keeps the user.
fn host_word(prefix: &str) -> Option<(&str, &str)> {
    // A `:` means the host is done and a remote path, which can't be
    // listed, has started.
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
    // Registered `complete` specs (a per-prompt snapshot of
    // `Shell::completions` and `default_completion`) and what
    // `compgen::resolve_spec` needs to evaluate one, including a snapshot
    // of the caller's functions and variables for `-F`/`-C` specs, which
    // run in a subprocess. `None` is a caller with no completion specs at
    // all; an empty map still consults `default_completion`.
    pub completions: Option<&'a HashMap<String, compgen::CompgenSpec>>,
    pub default_completion: Option<&'a compgen::CompgenSpec>,
    pub action_ctx: Option<&'a compgen::ActionContext>,
    pub functions_preamble: Option<&'a str>,
    /// The `gitignore` bishopt. `false` for a caller with no shell, which
    /// offers everything.
    pub honor_gitignore: bool,
    /// `FIGNORE`, split on `:`: filename suffixes to leave out (`.o`, `~`).
    pub fignore: Vec<String>,
    /// The `force_fignore` shopt, on by default as in bash. Off, the
    /// suffixes are dropped only while something else is left to offer.
    pub force_fignore: bool,
    /// The `::bish hl` names currently set, for `::bish hl --unset <Tab>`.
    /// A snapshot because the namespace is open. Empty for a caller with no
    /// shell.
    pub hl_names: Vec<String>,
}

impl<'a> CompletionProvider for ShellCompletionProvider<'a> {
    fn complete(&self, req: CompletionRequest) -> CompletionResult {
        let chars: Vec<char> = req.line.chars().collect();
        let cursor = req.cursor.min(chars.len());
        let word_start = find_word_start(&chars, cursor);
        let word = &chars[word_start..cursor];
        let prefix_text: String = chars[..word_start].iter().collect();

        // The role is the whole word's, so a brace list doesn't make
        // `mv somedir/{a,b` the second argument.
        let role = classify_word_role(&prefix_text);

        // Inside an unclosed brace only the alternative is completed, and
        // it inherits the text before the brace. Below works on the two
        // joined: what the shell will expand.
        let (replacing_from, prefix, inherited) = match brace_alternative(word) {
            Some((inherited_end, alternative)) => {
                let inherited: String = word[..inherited_end].iter().collect();
                let typed: String = word[alternative..].iter().collect();
                (word_start + alternative, format!("{inherited}{typed}"), inherited)
            }
            None => (word_start, word.iter().collect(), String::new()),
        };
        let word_start = replacing_from;
        if let CmdRole::Argument { command, .. } = &role
            && let Some(candidates) = self.registered_spec_candidates(command.as_deref(), &prefix)
        {
            return CompletionResult { word_start, candidates: strip_inherited(candidates, &inherited) };
        }
        // After a user's `complete` spec, which owns a command outright,
        // but before the generic fallbacks, which know nothing of bish's
        // builtins.
        if let Some(candidates) = builtin_argument_candidates(&role, &prefix_text, &prefix, &self.hl_names) {
            return CompletionResult { word_start, candidates: strip_inherited(candidates, &inherited) };
        }

        let candidates = match role {
            CmdRole::Command => self.command_name_candidates(&prefix),
            CmdRole::Argument { command, .. } if prefix.starts_with('-') => self.flag_candidates(command.as_deref(), &prefix),
            // Before the subcommand and file fallbacks: `ssh web` isn't a
            // path, and a man page doesn't know this machine's hosts.
            CmdRole::Argument { command, arg_index } if host_position(command.as_deref(), arg_index) => {
                self.host_candidates(command.as_deref(), &prefix)
            }
            CmdRole::Argument { command, .. } if directory_position(command.as_deref()) => self.directory_candidates(&prefix),
            CmdRole::Argument { command, arg_index: 0 } => self.subcommand_or_file_candidates(command.as_deref(), &prefix),
            CmdRole::Argument { .. } => self.file_candidates(&prefix),
        };

        CompletionResult { word_start, candidates: strip_inherited(candidates, &inherited) }
    }
}

impl<'a> ShellCompletionProvider<'a> {
    // `None` means no registered spec applies; `Some`, even empty, means
    // one matched and owns this completion, flags included, as in bash. The
    // command's own spec wins, and the `-D` default spec covers every other
    // command, including one behind a variable.
    fn registered_spec_candidates(&self, command: Option<&str>, prefix: &str) -> Option<Vec<CompletionCandidate>> {
        let completions = self.completions?;
        let spec = command.and_then(|c| completions.get(c)).or(self.default_completion)?;
        let default_ctx = compgen::ActionContext::default();
        let ctx = self.action_ctx.unwrap_or(&default_ctx);
        let cwd = self.cwd.unwrap_or_else(|| Path::new("."));
        let preamble = self.functions_preamble.unwrap_or("");
        let mut names = compgen::resolve_spec(spec, prefix, ctx, cwd, preamble);
        // Sorted alphabetically unless the spec says `-o nosort`, as in
        // bash. Only sorted: the entries are left as the spec produced them
        // (see `as_unranked_candidates`).
        if !spec.opts.iter().any(|o| o == "nosort") {
            crate::sort::sort(&mut names);
        }
        Some(as_unranked_candidates(prefix, names))
    }

    // Builtins, functions and PATH commands. Aliases are left out; see
    // `HighlightContext::known_functions` for why.
    fn command_name_candidates(&self, prefix: &str) -> Vec<CompletionCandidate> {
        let mut names: HashSet<String> = highlight::prompt_builtins().map(str::to_string).collect();
        if let Some(functions) = self.known_functions {
            names.extend(functions.iter().cloned());
        }
        // The caller's PATH scan (`Shell::action_context`, once per prompt,
        // also behind `compgen -c`), which used the session's PATH: the one
        // a typed command resolves against. Only this module's tests pass
        // no context; they get the real environment's.
        match self.action_ctx {
            Some(ctx) => names.extend(ctx.path_commands.iter().filter(|n| n.starts_with(prefix)).cloned()),
            None => names.extend(highlight::enumerate_path_matches(prefix, None)),
        }
        rank(prefix, names.into_iter().collect())
    }

    // Hosts this machine knows (`hosts::known`), plus local files for the
    // commands where an argument can be either end of a copy.
    fn host_candidates(&self, command: Option<&str>, prefix: &str) -> Vec<CompletionCandidate> {
        let also_files = matches!(command, Some("scp" | "rsync"));
        let Some((keep, _)) = host_word(prefix) else {
            // Past the `:`, a remote path: local files would be the wrong
            // machine's.
            return Vec::new();
        };
        let mut names: Vec<String> = crate::hosts::known().into_iter().map(|h| format!("{keep}{h}")).collect();
        // `scp host:` wants the colon, since the path follows on the same
        // word; `ssh host` is finished as it is.
        if also_files {
            names = names.into_iter().map(|n| format!("{n}:")).collect();
            names.extend(self.file_candidates(prefix).into_iter().map(|c| c.display));
        }
        rank(prefix, names)
    }

    // Flags only, never falling through on a miss, as in highlight.rs's
    // `classify_plain_argument_core`. A `-` word is never a file path
    // either.
    fn flag_candidates(&self, command: Option<&str>, prefix: &str) -> Vec<CompletionCandidate> {
        let Some(cmd) = command else { return Vec::new() };
        flag_candidates_core(ready_man_data(cmd).as_deref(), prefix)
    }

    // First argument and not a flag: subcommands if the man page has them,
    // else files. The `git co` case.
    fn subcommand_or_file_candidates(&self, command: Option<&str>, prefix: &str) -> Vec<CompletionCandidate> {
        if let Some(cmd) = command {
            if let Some(candidates) = subcommand_candidates_core(ready_man_data(cmd).as_deref(), prefix) {
                return candidates;
            }
        }
        self.file_candidates(prefix)
    }

    /// `file_candidates` without the non-directories. See
    /// `directory_position`.
    fn directory_candidates(&self, prefix: &str) -> Vec<CompletionCandidate> {
        // `file_candidates` marks directories with a trailing `/`, so this
        // needs no second trip to the filesystem.
        self.file_candidates(prefix).into_iter().filter(|c| c.display.ends_with('/')).collect()
    }

    // Candidates for `prefix` from a single `read_dir` of its directory
    // part, resolved against `cwd`. Each candidate is the whole replacement
    // word (`src/fuzzy.rs`, not `fuzzy.rs`), and directories get a trailing
    // `/`, which invites Tab for the next level.
    fn file_candidates(&self, prefix: &str) -> Vec<CompletionCandidate> {
        let Some(cwd) = self.cwd else { return Vec::new() };
        let dir_part = match prefix.rfind('/') {
            Some(idx) => &prefix[..idx + 1],
            None => "",
        };
        let dir_path = if let Some(rest) = dir_part.strip_prefix('~') {
            // `~/...`: only the lookup is expanded. `dir_part` stays
            // literal, so candidates start with the `~/` that was typed.
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
        // Built once for the directory, and only when Tab is pressed.
        let ignore = self.honor_gitignore.then(|| crate::gitignore::Stack::for_directory(&dir_path));
        let mut names = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            // Recorded rather than dropped; see the fallback below.
            let ignored = ignore.as_ref().is_some_and(|i| i.matched(&entry.path(), is_dir).is_ignored());
            names.push((format!("{dir_part}{name}{}", if is_dir { "/" } else { "" }), ignored));
        }
        let visible: Vec<String> = names.iter().filter(|(_, ig)| !ig).map(|(n, _)| n.clone()).collect();
        let ranked = self.rank_files(prefix, visible);
        // An ignored file can still be asked for by name: `.gitignore`
        // means "don't clutter the listing", not "can't be typed". So when
        // hiding them leaves nothing to complete to, they're offered.
        if !ranked.is_empty() || !names.iter().any(|(_, ig)| *ig) {
            return ranked;
        }
        self.rank_files(prefix, names.into_iter().map(|(n, _)| n).collect())
    }

    // `FIGNORE` last, on the names the listing produced. Directories keep
    // their trailing `/`, so the suffix test looks past it: a directory
    // called `x.o` isn't a `.o` file.
    fn rank_files(&self, prefix: &str, mut names: Vec<String>) -> Vec<CompletionCandidate> {
        if !self.fignore.is_empty() {
            let ignored = |n: &String| {
                let bare = n.strip_suffix('/').unwrap_or(n);
                self.fignore.iter().any(|suffix| bare.len() > suffix.len() && bare.ends_with(suffix.as_str()))
            };
            let kept: Vec<String> = names.iter().filter(|n| !ignored(n)).cloned().collect();
            // With `force_fignore` off, a filter that took everything is
            // ignored.
            if !kept.is_empty() || self.force_fignore {
                names = kept;
            }
        }
        rank(prefix, names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(prefix: &str, names: &[&str]) -> Vec<String> {
        rank(prefix, names.iter().map(|n| n.to_string()).collect()).into_iter().map(|c| c.display).collect()
    }

    const LISTING: &[&str] = &["Makefile", "README.md", "main.rs", "zebra.txt", ".env", ".gitignore", "src/", "node_modules/", ".git/"];

    // With nothing typed every candidate scores the same, and the
    // alphabetical tiebreak would put dotfiles first.
    #[test]
    fn tabbing_at_nothing_does_not_lead_with_dotfiles() {
        let order = ranked("", LISTING);
        let first_hidden = order.iter().position(|n| n.starts_with('.')).expect("they are still offered");
        assert_eq!(first_hidden, 6, "every visible name comes first: {order:?}");
        assert_eq!(&order[6..], &[".env".to_string(), ".git/".to_string(), ".gitignore".to_string()]);
    }

    // Asked for by name, they stop being unlikely.
    #[test]
    fn a_leading_dot_is_a_request_for_them() {
        let order = ranked(".g", LISTING);
        // Which of `.git/` and `.gitignore` leads is the score's business;
        // a hidden name just isn't pushed behind the rest.
        assert!(order.first().is_some_and(|n| n.starts_with(".git")), "{order:?}");
        assert!(order.iter().take(2).all(|n| n.starts_with('.')), "{order:?}");
    }

    // `./` starts a relative path, not a request for hidden names. Every
    // candidate matches `./` at the same place, so only name length is left
    // to score, which is why hidden has to outrank the score.
    #[test]
    fn an_explicit_relative_path_is_not_a_request_for_them() {
        let names: Vec<String> = LISTING.iter().map(|n| format!("./{n}")).collect();
        let order: Vec<String> = rank("./", names).into_iter().map(|c| c.display).collect();
        let first_hidden = order.iter().position(|n| n.starts_with("./.")).expect("still offered");
        assert_eq!(first_hidden, 6, "{order:?}");
    }

    #[test]
    fn hidden_is_a_property_of_the_last_segment() {
        assert!(hidden_name(".env") && hidden_name("src/.env") && hidden_name(".git/"));
        assert!(!hidden_name("./main.rs") && !hidden_name("src/") && !hidden_name("a.b"));
    }

    #[test]
    fn dot_and_dotdot_are_paths_rather_than_a_request() {
        assert!(!asking_for_hidden("."), "`.` is how you write the current directory");
        assert!(!asking_for_hidden(".."));
        assert!(!asking_for_hidden("src/"));
        assert!(asking_for_hidden(".git"));
        assert!(asking_for_hidden("src/.env"), "the segment being typed is what counts");
    }

    // There's no deriving this (see `directory_position`); the test says
    // what's on the list.
    #[test]
    fn the_commands_that_can_only_take_a_directory_are_written_down() {
        for cmd in ["cd", "pushd", "chdir", "rmdir", "mkdir"] {
            assert!(directory_position(Some(cmd)), "{cmd}");
        }
        for cmd in ["cat", "ls", "grep", "vim", "git"] {
            assert!(!directory_position(Some(cmd)), "{cmd} takes files too");
        }
        assert!(!directory_position(None), "a command this could not resolve gets the general answer");
    }

    #[test]
    fn fignore_leaves_out_the_suffixes_it_names() {
        let dir = std::env::temp_dir().join(format!("bish-fignore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["main.c", "main.o", "util.c", "util.o"] {
            std::fs::write(dir.join(n), "").unwrap();
        }
        let offer = |fignore: Vec<String>, force: bool| {
            let provider = ShellCompletionProvider {
                cwd: Some(&dir),
                known_functions: None,
                completions: None,
                default_completion: None,
                action_ctx: None,
                functions_preamble: None,
                honor_gitignore: false,
                fignore,
                force_fignore: force,
                hl_names: Vec::new(),
            };
            let mut got: Vec<String> =
                provider.complete(CompletionRequest { line: "cat ", cursor: 4 }).candidates.into_iter().map(|c| c.display).collect();
            got.sort();
            got
        };
        let all = vec!["main.c".to_string(), "main.o".to_string(), "util.c".to_string(), "util.o".to_string()];
        assert_eq!(offer(Vec::new(), true), all, "no FIGNORE offers everything");
        assert_eq!(offer(vec![".o".to_string()], true), vec!["main.c".to_string(), "util.c".to_string()]);
        assert!(offer(vec![".o".to_string(), ".c".to_string()], true).is_empty(), "force_fignore on means gone is gone");
        // Off, the filter applies only while it leaves something.
        assert_eq!(offer(vec![".o".to_string(), ".c".to_string()], false), all);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_ignored_file_still_completes_when_it_is_the_only_answer() {
        let dir = std::env::temp_dir().join(format!("bish-completion-gitignore-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".gitignore"), "plan.md\ntarget\n").unwrap();
        for n in ["plan.md", "notes.md"] {
            std::fs::write(dir.join(n), "").unwrap();
        }
        std::fs::create_dir_all(dir.join("target")).unwrap();
        let offer = |word: &str| {
            let provider = ShellCompletionProvider {
                cwd: Some(&dir),
                known_functions: None,
                completions: None,
                default_completion: None,
                action_ctx: None,
                functions_preamble: None,
                honor_gitignore: true,
                fignore: Vec::new(),
                force_fignore: false,
                hl_names: Vec::new(),
            };
            let line = format!("cat {word}");
            let mut got: Vec<String> =
                provider.complete(CompletionRequest { line: &line, cursor: line.len() }).candidates.into_iter().map(|c| c.display).collect();
            got.sort();
            got
        };
        assert_eq!(
            offer(""),
            vec![".git/".to_string(), ".gitignore".to_string(), "notes.md".to_string()],
            "an empty word gets the uncluttered listing -- `target/` and `plan.md` are the ones .gitignore names"
        );
        assert_eq!(offer("plan"), vec!["plan.md".to_string()], "but naming one is not a mistake");
        assert_eq!(offer("targ"), vec!["target/".to_string()]);
        assert_eq!(offer("notes"), vec!["notes.md".to_string()], "a word the filter can answer never reaches the fallback");
        assert!(offer("zzz").is_empty(), "and the fallback invents nothing");
        std::fs::remove_dir_all(&dir).ok();
    }

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
        // `word_start` is 4, where `co` starts, and the prefix is `git `.
        let line = "git co";
        let chars: Vec<char> = line.chars().collect();
        let cursor = chars.len();
        let word_start = find_word_start(&chars, cursor);
        assert_eq!(word_start, 4);
        let prefix_text: String = chars[..word_start].iter().collect();
        assert_eq!(prefix_text, "git ");
        assert_eq!(classify_word_role(&prefix_text), CmdRole::Argument { command: Some("git".to_string()), arg_index: 0 });
    }

    #[test]
    fn classify_word_role_resets_to_command_after_a_pipe() {
        assert_eq!(classify_word_role("true | "), CmdRole::Command);
    }

    #[test]
    fn classify_word_role_skips_an_assignment_prefix() {
        // `FOO=bar ` still expects the command name.
        assert_eq!(classify_word_role("FOO=bar "), CmdRole::Command);
    }

    #[test]
    fn classify_word_role_degrades_gracefully_inside_an_unclosed_substitution() {
        // Must not panic. The role isn't asserted: an unclosed `$(...)`
        // gets no guaranteed answer.
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
        let provider = ShellCompletionProvider {
            cwd: None,
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
        let names = display_names(provider.command_name_candidates("ech"));
        assert!(names.iter().any(|n| n == "echo"), "{names:?}");
    }

    #[test]
    fn command_name_candidates_includes_a_known_function() {
        // An unusual prefix, so no unrelated executable on PATH can match.
        let mut functions = HashSet::new();
        functions.insert("zz_bish_test_func".to_string());
        let provider = ShellCompletionProvider {
            cwd: None,
            known_functions: Some(&functions),
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
        let names = display_names(provider.command_name_candidates("zz_bish_test"));
        assert_eq!(names, vec!["zz_bish_test_func".to_string()]);
    }

    #[test]
    fn command_name_candidates_includes_a_real_path_executable() {
        // coreutils, as highlight.rs's `is_in_path` tests assume.
        let provider = ShellCompletionProvider {
            cwd: None,
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
        let names = display_names(provider.command_name_candidates("tru"));
        assert!(names.iter().any(|n| n == "true"), "{names:?}");
    }

    // Command names come from the session's PATH, which can differ from the
    // process environment's after a `.bishrc` or a `cd` hook changes it.
    #[test]
    fn command_name_candidates_come_from_the_callers_own_path_scan() {
        let ctx = compgen::ActionContext {
            path_commands: vec!["zz-bish-hookbin".to_string(), "zz-bish-elsewhere".to_string()],
            ..compgen::ActionContext::default()
        };
        let provider = ShellCompletionProvider {
            cwd: None,
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: Some(&ctx),
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
        let names = display_names(provider.command_name_candidates("zz-bish-hook"));
        assert!(names.iter().any(|n| n == "zz-bish-hookbin"), "{names:?}");
        // Prefix-filtered.
        assert!(!names.iter().any(|n| n == "zz-bish-elsewhere"), "{names:?}");
        // And the real environment's PATH isn't consulted as well.
        let names = display_names(provider.command_name_candidates("sor"));
        assert!(!names.iter().any(|n| n == "sort"), "{names:?}");
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
        let man = manpages::ManPageData {
            flags: vec!["-l".to_string()],
            subcommands: vec![],
            name_section: None,
            flag_descriptions: std::collections::HashMap::new(),
        };
        assert_eq!(subcommand_candidates_core(Some(&man), "co"), None);
        assert_eq!(subcommand_candidates_core(None, "co"), None);
    }

    // Canned man-page data, no real `man`: commit, config and count-objects
    // rank above checkout for `co`.
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

    // `mv somedir/{old,new}`: the word under the cursor is
    // `somedir/{old,ne`, which isn't a path, so the brace has to be
    // understood.
    #[test]
    fn a_brace_alternative_completes_against_the_path_the_brace_sits_in() {
        let dir = crate::tempdir::TempDir::new("completion-brace");
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("somedir")).unwrap();
        std::fs::write(dir.join("somedir/old-notes.txt"), b"hi").unwrap();
        std::fs::write(dir.join("somedir/new-notes.txt"), b"hi").unwrap();
        std::fs::write(dir.join("elsewhere.txt"), b"hi").unwrap();
        let provider = ShellCompletionProvider {
            cwd: Some(dir),
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
        let at = |line: &str| {
            let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
            (result.word_start, display_names(result.candidates))
        };

        // After the brace and after the comma, both completed against
        // `somedir/`. The candidate is only the alternative, since the
        // editor replaces `ol`, not the whole word.
        assert_eq!(at("mv somedir/{ol"), ("mv somedir/{".chars().count(), vec!["old-notes.txt".to_string()]));
        // Fuzzy, as everywhere here: `somedir/ne` matches the old file too.
        // Both are re-rooted onto the alternative, best first.
        let (from, names) = at("mv somedir/{old-notes.txt,ne");
        assert_eq!(from, "mv somedir/{old-notes.txt,".chars().count());
        assert_eq!(names.first().map(String::as_str), Some("new-notes.txt"), "{names:?}");
        assert!(names.iter().all(|n| !n.contains('/')), "every candidate replaces the alternative alone: {names:?}");

        // The text before the brace needn't end in a slash.
        std::fs::write(dir.join("prefix-one"), b"hi").unwrap();
        assert_eq!(at("cat prefix{-o"), ("cat prefix{".chars().count(), vec!["-one".to_string()]));

        // A closed brace isn't a brace context.
        assert_eq!(at("cat else"), (4, vec!["elsewhere.txt".to_string()]));
        assert_eq!(at("cat {a,b}else"), (4, Vec::<String>::new()), "nothing is named `{{a,b}}else`");
    }

    // `${` opens a parameter expansion, not a list: `cat ${HOM` must not
    // offer files.
    #[test]
    fn a_parameter_expansion_is_not_a_brace_list() {
        assert_eq!(super::brace_alternative(&"somedir/{old,ne".chars().collect::<Vec<_>>()), Some((8, 13)));
        assert_eq!(super::brace_alternative(&"${HOM".chars().collect::<Vec<_>>()), None);
        assert_eq!(super::brace_alternative(&"${HOME}/pro".chars().collect::<Vec<_>>()), None);
        // Escaped, so not an opening brace.
        assert_eq!(super::brace_alternative(&r"a\{b".chars().collect::<Vec<_>>()), None);
        // Nested: the alternative is the innermost list's, and the
        // inherited text is still everything before the outer one.
        assert_eq!(super::brace_alternative(&"p{a,{b,c".chars().collect::<Vec<_>>()), Some((1, 7)), "the `c` after the inner comma");
    }

    #[test]
    fn file_candidates_lists_a_real_directory_with_prefix_filter_and_dir_slash() {
        let dir = std::env::temp_dir().join(format!("bish-completion-files-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("widgets")).unwrap();
        std::fs::write(dir.join("widget-notes.txt"), b"hi").unwrap();
        std::fs::write(dir.join("unrelated.txt"), b"hi").unwrap();

        let provider = ShellCompletionProvider {
            cwd: Some(&dir),
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
        let names = display_names(provider.file_candidates("widg"));

        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().any(|n| n == "widgets/"), "{names:?}");
        assert!(names.iter().any(|n| n == "widget-notes.txt"), "{names:?}");
        assert!(!names.iter().any(|n| n == "unrelated.txt"), "{names:?}");
    }

    // `~/` has to be expanded for the lookup: joined onto `cwd` literally
    // it names a directory that doesn't exist.
    #[test]
    fn file_candidates_expands_a_leading_tilde_slash_for_the_lookup() {
        let dir = std::env::temp_dir().join(format!("bish-completion-tilde-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".config")).unwrap();

        let original_home = std::env::var("HOME").unwrap_or_default();
        let names = {
            // SAFETY: nothing else in this test binary depends on HOME
            // during this window, and it's restored before returning.
            unsafe { std::env::set_var("HOME", &dir) };
            // A cwd unrelated to HOME, so the lookup has to go to $HOME.
            let provider = ShellCompletionProvider {
                cwd: Some(Path::new("/")),
                known_functions: None,
                completions: None,
                default_completion: None,
                action_ctx: None,
                functions_preamble: None,
                honor_gitignore: false,
                fignore: Vec::new(),
                force_fignore: true,
                hl_names: Vec::new(),
            };
            let names = display_names(provider.file_candidates("~/.co"));
            unsafe { std::env::set_var("HOME", &original_home) };
            names
        };

        std::fs::remove_dir_all(&dir).ok();

        // The candidate keeps the typed `~/`: it's spliced back into the
        // buffer.
        assert_eq!(names, vec!["~/.config/".to_string()]);
    }

    #[test]
    fn file_candidates_yields_nothing_without_a_cwd() {
        let provider = ShellCompletionProvider {
            cwd: None,
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
        assert_eq!(provider.file_candidates("anything"), Vec::new());
    }

    #[test]
    fn complete_dispatches_bare_prefix_to_command_names() {
        let provider = ShellCompletionProvider {
            cwd: None,
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
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

        let provider = ShellCompletionProvider {
            cwd: Some(&dir),
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
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
        let provider = ShellCompletionProvider {
            cwd: None,
            known_functions: None,
            completions: Some(&completions),
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
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
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
        let line = "unknowncmd12345 def";
        let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
        assert_eq!(display_names(result.candidates), vec!["defaultA".to_string(), "defaultB".to_string()]);
    }

    #[test]
    fn complete_never_consults_a_registered_spec_when_none_is_supplied() {
        // `completions: None`, a caller with no specs at all, falls through
        // to command names.
        let provider = ShellCompletionProvider {
            cwd: None,
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
        let line = "ech";
        let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
        assert!(display_names(result.candidates).iter().any(|n| n == "echo"));
    }

    /// A registered spec's candidates come out sorted unless it asks for
    /// `-o nosort`, as in bash.
    #[test]
    fn a_registered_specs_candidates_are_sorted_unless_it_says_nosort() {
        let ordered = |spec: compgen::CompgenSpec| {
            let mut completions = HashMap::new();
            completions.insert("cmd".to_string(), spec);
            let provider = ShellCompletionProvider {
                cwd: None,
                known_functions: None,
                completions: Some(&completions),
                default_completion: None,
                action_ctx: None,
                functions_preamble: None,
                honor_gitignore: false,
                fignore: Vec::new(),
                force_fignore: true,
                hl_names: Vec::new(),
            };
            let line = "cmd abc";
            display_names(provider.complete(CompletionRequest { line, cursor: line.chars().count() }).candidates)
        };
        let words = "abcdef abcd abcde abc";
        assert_eq!(
            ordered(compgen::CompgenSpec { wordlist: Some(words.to_string()), ..Default::default() }),
            vec!["abc".to_string(), "abcd".to_string(), "abcde".to_string(), "abcdef".to_string()],
            "the shorter completion is the one reached first"
        );
        // ...and the spec that asks to keep its own order keeps it.
        assert_eq!(
            ordered(compgen::CompgenSpec { wordlist: Some(words.to_string()), opts: vec!["nosort".to_string()], ..Default::default() }),
            vec!["abcdef".to_string(), "abcd".to_string(), "abcde".to_string(), "abc".to_string()],
        );
    }

    #[test]
    fn as_unranked_candidates_keeps_every_entry_even_ones_not_starting_with_the_prefix() {
        // A `-F`/`-C` source is trusted to have filtered already, so an
        // entry not starting with `prefix` still comes through.
        let out = as_unranked_candidates("xyz", vec!["foo".to_string(), "xyzzy".to_string()]);
        assert_eq!(display_names(out.clone()), vec!["foo".to_string(), "xyzzy".to_string()]);
        assert_eq!(out[0].matched_positions, Vec::<usize>::new(), "no highlight for a non-matching entry");
        assert_eq!(out[1].matched_positions, vec![0, 1, 2], "highlights the matched prefix span");
    }

    fn bishopt_at(line: &str) -> Vec<String> {
        let cursor = line.chars().count();
        BuiltinCompletionProvider { hl_names: Vec::new(), commands: &[] }
            .complete(CompletionRequest { line, cursor })
            .candidates
            .into_iter()
            .map(|c| c.display)
            .collect()
    }

    // `::bish` is mostly typed on the editor's colon line.
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

    // The server's command line is an ordinary command line, so this
    // position is given up rather than claimed and left empty.
    #[test]
    fn bish_lsp_add_hands_the_server_command_back_to_the_ordinary_completions() {
        assert!(bish_candidates(&strs(&["lsp", "add"]), "rust-ana", &[]).is_none());
        assert!(bish_candidates(&strs(&["lsp", "add", "--lang=rust"]), "rust-ana", &[]).is_none());
        // Likewise a hook's handler, but only after its event, which is a
        // fixed list.
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
        // `--lang=` first is the same position: it's a flag, not the event.
        assert!(bish_at("::bish hook add --lang=rust ").contains(&"editor:file:open".to_string()));
    }

    // Only what is set can be unset.
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

        // The colour is free text, but a file name would be wrong, so the
        // position is claimed and left empty.
        assert!(hl_candidates(&strs(&["--set", "string"]), "#ff", &[]).is_empty());
        assert!(bish_candidates(&strs(&["hl", "--set", "string"]), "#ff", &[]).is_some());

        let flags = bish_at("::bish hl --");
        assert!(flags.contains(&"--set".to_string()) && flags.contains(&"--unset".to_string()), "{flags:?}");

        // The snapshot reaches the colon line's provider, where it had to
        // be threaded through.
        let line = "::bish hl --unset ";
        let colon = BuiltinCompletionProvider { hl_names: strs(&["colonLineOnly"]), commands: &[] };
        let found = colon.complete(CompletionRequest { line, cursor: line.chars().count() });
        assert_eq!(display_names(found.candidates), strs(&["colonLineOnly"]));
    }

    // A line's own vocabulary completes in command position only.
    #[test]
    fn the_lines_own_commands_complete_in_command_position() {
        let provider = BuiltinCompletionProvider { hl_names: Vec::new(), commands: &["preview", "prev", "diag"] };
        let at = |line: &str| display_names(provider.complete(CompletionRequest { line, cursor: line.chars().count() }).candidates);
        assert_eq!(at("prev"), strs(&["prev", "preview"]));
        assert_eq!(at("di"), strs(&["diag"]));
        assert_eq!(at("diag prev"), Vec::<String>::new());
    }

    // Compares sets: the list is the registry, nothing invented or missed.
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

    // A boolean has a fixed set of values; anything else is free text.
    #[test]
    fn bishopt_completes_on_and_off_for_a_boolean_value_only() {
        let mut values = bishopt_at("bishopt --set wrap ");
        values.sort();
        assert_eq!(values, vec!["off", "on"]);
        assert_eq!(bishopt_at("bishopt --set showbreak "), Vec::<String>::new());
        assert_eq!(bishopt_at("bishopt --set wrap_column "), Vec::<String>::new());
    }

    // `Some(empty)`: bishopt owns its completion, so nothing to offer means
    // no file names either.
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
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
        let line = "bishopt --set wr";
        let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
        let names: Vec<String> = result.candidates.into_iter().map(|c| c.display).collect();
        assert!(names.contains(&"wrap".to_string()));
    }

    #[test]
    fn host_positions_are_where_a_host_can_actually_go() {
        // One host, then a remote command.
        assert!(host_position(Some("ssh"), 0));
        assert!(!host_position(Some("ssh"), 1));
        assert!(host_position(Some("sftp"), 0));
        // Either end of a copy can be remote, in any position.
        assert!(host_position(Some("scp"), 0));
        assert!(host_position(Some("scp"), 2));
        assert!(host_position(Some("rsync"), 1));
        assert!(!host_position(Some("ls"), 0));
        assert!(!host_position(None, 0));
    }

    #[test]
    fn a_user_prefix_is_kept_and_the_host_is_what_completes() {
        assert_eq!(host_word("we"), Some(("", "we")));
        assert_eq!(host_word("deploy@we"), Some(("deploy@", "we")));
        assert_eq!(host_word(""), Some(("", "")));
    }

    #[test]
    fn a_colon_ends_host_completion() {
        assert_eq!(host_word("web:"), None);
        assert_eq!(host_word("web:/var/l"), None);
        assert_eq!(host_word("deploy@web:/etc"), None);
    }

    // `ssh web` must not fall through to file completion.
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
            fignore: Vec::new(),
            force_fignore: true,
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
        let guard = crate::tempdir::TempDir::new("completion-scp-test");
        std::fs::write(guard.join("zzz-local.txt"), "").unwrap();
        let dir = guard.path().to_path_buf();
        let provider = ShellCompletionProvider {
            cwd: Some(&dir),
            known_functions: None,
            completions: None,
            default_completion: None,
            action_ctx: None,
            functions_preamble: None,
            honor_gitignore: false,
            fignore: Vec::new(),
            force_fignore: true,
            hl_names: Vec::new(),
        };
        let line = "scp zzz";
        let result = provider.complete(CompletionRequest { line, cursor: line.chars().count() });
        let names: Vec<String> = result.candidates.into_iter().map(|c| c.display).collect();
        assert!(names.iter().any(|n| n.contains("zzz-local")), "got {names:?}");
    }

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
                fignore: Vec::new(),
                force_fignore: true,
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
            provider.complete(CompletionRequest { line, cursor: line.chars().count() }).candidates.into_iter().map(|c| c.display).collect::<Vec<_>>()
        };
        let offered = names(true);
        assert!(offered.contains(&"tangent.txt".to_string()), "got {offered:?}");
        assert!(!offered.iter().any(|n| n.starts_with("target")), "target/ is ignored: {offered:?}");
        assert!(names(false).iter().any(|n| n.starts_with("target")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
