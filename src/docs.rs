// Godoc-style doc comments for bash scripts -- a `#`-commented block
// immediately above a function definition or a top-level variable
// declaration becomes that symbol's own documentation, the same
// adjacency rule godoc itself uses for `// Comment` above a Go
// declaration (no blank line allowed between the two). This is the
// precursor to an LSP-style hover story bish will eventually want (real
// symbol resolution, go-to-definition, ...) -- built with bish's own
// existing lexer/parser rather than any actual language-server protocol
// machinery, scoped narrowly to what debugger.rs's own `K` hover needs
// right now.
//
// `source PATH`/`. PATH` is followed automatically when PATH is a
// *static* literal (no `$var`/`$(...)`/backticks/etc. -- see
// word_as_literal) so hovering a symbol defined in a sourced library
// works the same as hovering one defined in the entry script itself. A
// dynamically-built source path (`source "$LIB_DIR/util.sh"`, `source
// "$(dirname "$0")/lib.sh"`) can't be resolved without actually running
// the script, so those are simply not followed -- an accepted, sensible
// scope boundary given this is static analysis, not execution.
//
// Deliberately shallow, matching every other "build the practical
// subset" feature in this codebase: only top-level statements are
// scanned (a function/variable declared *inside* an `if`/`while`/`case`
// body is invisible to this, however doc-commented) -- covers the
// overwhelming majority of real scripts, where library functions and
// constants sit at a file's top level.

use crate::lexer::{Chunk, Lexer};
use crate::parser::{AndOr, Command, Parser, Program, Word};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Variable,
}

pub struct SymbolDoc {
    pub kind: SymbolKind,
    // One entry per source comment line, with its leading `#` and (at
    // most) one following space already stripped -- render as separate
    // lines, not rejoined into prose, so an author's own line breaks
    // (e.g. an "Args:" list) survive into the hover popup.
    pub doc: Vec<String>,
    pub file: PathBuf,
    pub line: usize,
}

pub struct DocIndex {
    symbols: HashMap<String, SymbolDoc>,
}

impl DocIndex {
    // Scans `src` (the entry script's own text -- both callers already
    // have this in memory: debugger.rs reads the file once up front,
    // repl.rs's live file editor takes it straight from the buffer, an
    // unsaved edit included) and, recursively, every file it statically
    // `source`s (those genuinely are read fresh off disk -- there's no
    // live buffer for them to prefer) -- see this module's own doc
    // comment. Best-effort throughout: a file that can't be read or
    // fails to parse just contributes nothing rather than aborting the
    // whole scan. `entry_path` is what any `source` the script itself
    // makes gets resolved relative to, and what the entry file's own
    // symbols are attributed to -- it doesn't need to actually exist on
    // disk with matching content for that (a brand new, not-yet-saved
    // buffer). Pre-seeds the visited-set with `entry_path` itself so a
    // `source` that happens to point back at the very file being scanned
    // re-uses `src` (and doesn't re-read what might be stale disk
    // content, if it's actually a live buffer with unsaved changes).
    pub fn build_from_source(src: &str, entry_path: &Path) -> DocIndex {
        let mut symbols = HashMap::new();
        let mut visited = HashSet::new();
        visited.insert(std::fs::canonicalize(entry_path).unwrap_or_else(|_| entry_path.to_path_buf()));
        scan_source(src, entry_path, &mut symbols, &mut visited);
        DocIndex { symbols }
    }

    pub fn lookup(&self, name: &str) -> Option<&SymbolDoc> {
        self.symbols.get(name)
    }
}

fn scan_file(path: &Path, symbols: &mut HashMap<String, SymbolDoc>, visited: &mut HashSet<PathBuf>) {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return; // already scanned, or a source cycle -- either way, stop here.
    }
    let Ok(src) = std::fs::read_to_string(path) else { return };
    scan_source(&src, path, symbols, visited);
}

// The shared "given source text and the path it's attributed to" half
// of scan_file/build_from_source -- the only difference between reading
// a file fresh off disk and scanning an already-in-memory buffer is
// where `src` came from.
fn scan_source(src: &str, path: &Path, symbols: &mut HashMap<String, SymbolDoc>, visited: &mut HashSet<PathBuf>) {
    let Ok(toks) = Lexer::new(src).tokenize() else { return };
    let Ok(program) = Parser::new(toks).parse_program() else { return };
    let lines: Vec<&str> = src.lines().collect();

    // Collected rather than followed inline: keeps the entry file's own
    // top-level symbols recorded (and so entry()-preferred over anything
    // a sourced file might redeclare) before any recursive scan can run.
    let mut sources_to_follow: Vec<PathBuf> = Vec::new();
    scan_program(&program, &lines, path, symbols, &mut sources_to_follow);
    for source_path in sources_to_follow {
        scan_file(&source_path, symbols, visited);
    }
}

fn scan_program(program: &Program, lines: &[&str], file: &Path, symbols: &mut HashMap<String, SymbolDoc>, sources_to_follow: &mut Vec<PathBuf>) {
    for item in program {
        for cmd in pipeline_commands(&item.and_or) {
            match cmd {
                Command::FuncDef { name, .. } => {
                    record_symbol(symbols, name.clone(), SymbolKind::Function, file, item.line, doc_comment_above(lines, item.line));
                }
                Command::Simple(sc) => {
                    if sc.words.is_empty() {
                        // A bare assignment statement -- `NAME=value`,
                        // `arr=(1 2 3)`, `arr[i]=value` -- with no
                        // command word at all.
                        for (name, _, _) in &sc.assigns {
                            record_symbol(symbols, name.clone(), SymbolKind::Variable, file, item.line, doc_comment_above(lines, item.line));
                        }
                        for (name, _, _) in &sc.array_assigns {
                            record_symbol(symbols, name.clone(), SymbolKind::Variable, file, item.line, doc_comment_above(lines, item.line));
                        }
                        for (name, ..) in &sc.index_assigns {
                            record_symbol(symbols, name.clone(), SymbolKind::Variable, file, item.line, doc_comment_above(lines, item.line));
                        }
                        continue;
                    }
                    let Some(first) = word_as_literal(&sc.words[0]) else { continue };
                    if (first == "source" || first == ".") && sc.words.len() > 1 {
                        if let Some(p) = word_as_literal(&sc.words[1])
                            && let Some(resolved) = resolve_source_path(&p, file)
                        {
                            sources_to_follow.push(resolved);
                        }
                    } else if matches!(first.as_str(), "declare" | "local" | "export" | "readonly" | "typeset") {
                        for w in sc.words.iter().skip(1) {
                            let Some(lit) = word_as_literal(w) else { continue };
                            if lit.starts_with('-') {
                                continue;
                            }
                            if let Some(name) = leading_identifier(&lit) {
                                record_symbol(symbols, name, SymbolKind::Variable, file, item.line, doc_comment_above(lines, item.line));
                            }
                        }
                        for (_, name, _, _) in &sc.array_word_assigns {
                            record_symbol(symbols, name.clone(), SymbolKind::Variable, file, item.line, doc_comment_above(lines, item.line));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

// Every command in every stage of every pipeline an AndOr chains
// together (`a && b | c || d`) -- flattened since a doc-commented
// declaration could in principle sit anywhere in such a chain, not just
// as the first, unchained command.
fn pipeline_commands(and_or: &AndOr) -> Vec<&Command> {
    let mut out: Vec<&Command> = and_or.first.commands.iter().collect();
    for (_, pipeline) in &and_or.rest {
        out.extend(pipeline.commands.iter());
    }
    out
}

// Only records a symbol when it actually has an adjacent comment block
// (a name with nothing above it has nothing for this feature to show --
// see debugger.rs's own hover fallthrough to a man-page snippet for that
// case) and only the *first* one seen wins, so the entry script's own
// declarations always take precedence over a same-named one pulled in
// from a `source`d file.
fn record_symbol(symbols: &mut HashMap<String, SymbolDoc>, name: String, kind: SymbolKind, file: &Path, line: usize, doc: Vec<String>) {
    if doc.is_empty() {
        return;
    }
    symbols.entry(name).or_insert(SymbolDoc { kind, doc, file: file.to_path_buf(), line });
}

// Walks upward from `decl_line` (1-based) collecting a contiguous run of
// `#`-commented lines directly above it -- stops at the first blank
// line or real code, exactly godoc's own "immediately adjacent, no gap"
// rule for `// Comment` above a Go declaration. Returned top-to-bottom
// (source order), each entry with its leading `#` and one following
// space (if present) already stripped.
fn doc_comment_above(lines: &[&str], decl_line: usize) -> Vec<String> {
    let mut collected = Vec::new();
    if decl_line < 2 {
        return collected;
    }
    let mut idx = decl_line as isize - 2; // 0-based index of the line just above decl_line
    while idx >= 0 {
        let line = lines[idx as usize];
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix('#') else { break };
        collected.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        idx -= 1;
    }
    collected.reverse();
    collected
}

// `Some` only when every chunk of `word` is plain literal text (no
// `$var`/`$(...)`/arithmetic/etc.) -- the "static" half of "follow
// `source <static_path>` calls automatically": anything with real
// expansion in it can't be resolved without actually running the
// script, so this returns `None` for it instead of guessing.
fn word_as_literal(word: &Word) -> Option<String> {
    let mut s = String::new();
    for chunk in &word.chunks {
        match chunk {
            Chunk::Str(t) | Chunk::LiteralStr(t) => s.push_str(t),
            _ => return None,
        }
    }
    Some(s)
}

// The identifier prefix of a `declare`/`local`/etc. word -- `NAME` from
// bare `NAME`, or from `NAME=value`/`NAME+=value`. `None` if the word
// doesn't start with a valid identifier at all (a stray flag-looking
// word that wasn't caught by the caller's own `starts_with('-')` check,
// or similar).
fn leading_identifier(s: &str) -> Option<String> {
    let end = s.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(s.len());
    let ident = &s[..end];
    match ident.chars().next() {
        Some(c) if c.is_alphabetic() || c == '_' => Some(ident.to_string()),
        _ => None,
    }
}

// Resolves a literal `source`/`.` argument against the file that
// contains it -- real bash resolves a slash-less relative path against
// $PATH if it isn't found relative to the current directory, but this is
// static analysis with no running shell/cwd to consult, so "relative to
// the sourcing file's own directory" (by far the common real-world
// convention for a script's own library files) is tried first, then
// plain relative-to-this-process's-cwd as a fallback. `None` (rather
// than guessing) when neither actually exists, so a path this can't
// really resolve is silently skipped instead of scanning the wrong file.
fn resolve_source_path(raw: &str, from_file: &Path) -> Option<PathBuf> {
    let candidate = PathBuf::from(raw);
    if candidate.is_absolute() {
        return candidate.is_file().then_some(candidate);
    }
    if let Some(dir) = from_file.parent() {
        let joined = dir.join(&candidate);
        if joined.is_file() {
            return Some(joined);
        }
    }
    candidate.is_file().then_some(candidate)
}

// The identifier (`[A-Za-z0-9_]+`) touching column `col` of `chars` --
// `K`-hover's own "what's actually under the cursor" target, shared by
// both `K`'s callers (debugger.rs's own read-only view, repl.rs's real
// file editor). `None` when the cursor isn't sitting on one at all.
pub fn identifier_at(chars: &[char], col: usize) -> Option<String> {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    if col >= chars.len() || !is_ident(chars[col]) {
        return None;
    }
    let start = (0..=col).rev().take_while(|&i| is_ident(chars[i])).last().unwrap_or(col);
    let end = (col..chars.len()).take_while(|&i| is_ident(chars[i])).count() + col;
    Some(chars[start..end].iter().collect())
}

// `K`'s own single entry point: `chars`/`col` locate the cursor on its
// own line (already split out by the caller -- see identifier_at's own
// shape), `line_text` is that same line as a plain string (classify_word
// needs to re-tokenize it), `live_value` is however the caller answers
// "does this identifier have a live value right now" (debugger.rs asks
// `Shell::debug_peek_var`; repl.rs's plain file editor has no running
// script to ask, so it always answers `None`).
//
// A doc-commented symbol or a live value wins regardless of *where* the
// identifier sits (a variable reference is a variable reference whether
// it's argv[0], a flag's own argument, or buried in `${...}`) -- but
// once neither applies, this does NOT fall back to an unconditional
// man-page query the way an earlier version did: `classify_word` decides
// whether the cursor is actually sitting on a command name, a
// recognized-shape subcommand, or a flag before ever trying `man` at
// all. Hovering a plain positional argument -- including, notably, a
// word that only *looks* identifier-shaped because it's sitting inside
// a quoted string ("please deploy the app") -- correctly finds nothing
// rather than spawning a pointless `man` lookup for prose.
pub fn hover_lines_at(chars: &[char], col: usize, line_text: &str, index: &DocIndex, live_value: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let identifier = identifier_at(chars, col);
    if let Some(name) = &identifier {
        if let Some(value) = live_value(name) {
            return hover_lines(name, Some(&value), index);
        }
        if index.lookup(name).is_some() {
            return hover_lines(name, None, index);
        }
    }
    match classify_word(line_text, col) {
        WordRole::Flag { command, flag } => hover_lines_for_flag(&command, &flag),
        WordRole::Command(name) => hover_lines_for_command(&name),
        WordRole::Subcommand { command, subcommand } => hover_lines_for_subcommand(&command, &subcommand),
        WordRole::Other => match identifier {
            Some(name) => vec![name, "no info available".to_string()],
            None => vec!["no identifier under the cursor".to_string()],
        },
    }
}

// The value/doc-comment half of hover_lines_at, split out so a caller
// that's already resolved one of the two (hover_lines_at itself) can
// render it without redoing that work. `live_value` wins outright when
// present; otherwise `index`'s own doc comment, if any. Never reached
// with both `None` from hover_lines_at (that falls to classify_word
// instead) -- kept defensive (a bare name, no second line) rather than
// assuming that, in case something else ever calls this directly.
fn hover_lines(name: &str, live_value: Option<&str>, index: &DocIndex) -> Vec<String> {
    let mut lines = vec![name.to_string()];
    if let Some(value) = live_value {
        lines.push(format!("= {}", value));
    } else if let Some(doc) = index.lookup(name) {
        let kind = match doc.kind {
            SymbolKind::Function => "function",
            SymbolKind::Variable => "variable",
        };
        lines.push(format!("{} -- {}:{}", kind, doc.file.display(), doc.line));
        lines.extend(doc.doc.iter().cloned());
    }
    lines
}

// What role the word touching `col` (0-based, in chars) of `line` plays
// in its own command -- the gate that decides whether a man-page lookup
// even makes sense for a bare word that isn't a known symbol. Built by
// re-tokenizing `line` with the real lexer (`tokenize_spanned`, for its
// byte-range-per-token -- exactly what's needed to tell "is the cursor
// actually on this token" from plain character-class scanning, which is
// what let a quoted string's own prose read as command-shaped before
// this existed) and walking it left to right, tracking which command
// (reset at every `;`/`&&`/`||`/`&`/newline) the cursor's own token
// would belong to and how far into its argument list.
pub enum WordRole {
    // The word IS the command name itself (argv[0], skipping over any
    // leading assignment-prefix words like `FOO=bar`).
    Command(String),
    // The word sits at the shape "the argument right after the command
    // name, and isn't a flag" -- a plausible subcommand *position*,
    // regardless of whether the command actually has a subcommand by
    // this name (hover_lines_for_subcommand's own man-page query is what
    // actually confirms or denies that).
    Subcommand { command: String, subcommand: String },
    // A `-x`/`--long[=value]` word, plus the command it belongs to (if
    // any could be determined at all).
    Flag { command: String, flag: String },
    // Anything else: a later positional argument, a redirect target, a
    // separator itself, or a word this couldn't classify at all (an
    // unparseable line, most often because the cursor's own line is only
    // half of a multi-line construct still being typed).
    Other,
}

pub fn classify_word(line: &str, col: usize) -> WordRole {
    let byte_col: usize = line.chars().take(col).map(|c| c.len_utf8()).sum();
    let spanned = crate::lexer::tokenize_spanned(line);
    let mut command: Option<String> = None;
    // How many non-flag words have been seen since `command` was set --
    // 0 means the *next* one is the subcommand-position candidate.
    let mut arg_index: usize = 0;
    for item in spanned.items {
        let crate::lexer::SpannedItem::Tok(tok, range) = item else { continue };
        let hit = range.contains(&byte_col);
        match tok {
            crate::lexer::Tok::Pipe
            | crate::lexer::Tok::And
            | crate::lexer::Tok::Or
            | crate::lexer::Tok::Semi
            | crate::lexer::Tok::DSemi
            | crate::lexer::Tok::SemiAmp
            | crate::lexer::Tok::DSemiAmp
            | crate::lexer::Tok::Amp
            | crate::lexer::Tok::Newline => {
                if hit {
                    return WordRole::Other;
                }
                command = None;
                arg_index = 0;
            }
            crate::lexer::Tok::Word(chunks, globbable) => {
                let text = word_as_literal(&Word { chunks, globbable });
                let Some(text) = text else {
                    if hit {
                        return WordRole::Other;
                    }
                    if command.is_some() {
                        arg_index += 1;
                    }
                    continue;
                };
                if command.is_none() && is_assignment_word(&text) {
                    if hit {
                        return WordRole::Other;
                    }
                    continue; // an assignment prefix doesn't advance command state at all
                }
                if looks_like_flag(&text) {
                    if hit {
                        return match &command {
                            Some(cmd) => WordRole::Flag { command: cmd.clone(), flag: text },
                            None => WordRole::Other,
                        };
                    }
                    continue; // flags don't consume an arg_index slot
                }
                if command.is_none() {
                    if hit {
                        // `globbable` (Tok::Word's own second field) is
                        // true only for a word with no quoting/escaping/
                        // expansion at all -- a fully bareword command
                        // name. A *quoted* word sitting in this same
                        // position (rare, but `"$0"`-style indirection
                        // exists) isn't a literal command name worth a
                        // man-page lookup for, so it's Other instead --
                        // still becomes `command` for bookkeeping below,
                        // since whatever comes after it still needs a
                        // command context to attribute its own flags to.
                        return if globbable { WordRole::Command(text) } else { WordRole::Other };
                    }
                    command = Some(text);
                    arg_index = 0;
                    continue;
                }
                if hit {
                    // Same reasoning as just above, for the subcommand
                    // position -- this is exactly what caught a quoted
                    // argument (`read -p "Your name: " USER_NAME`'s own
                    // prompt string) being misread as command-shaped
                    // before this existed: it sits at the "first
                    // non-flag word after the command" position, but
                    // it's quoted text, not a bareword subcommand name.
                    return if arg_index == 0 && globbable {
                        WordRole::Subcommand { command: command.clone().unwrap(), subcommand: text }
                    } else {
                        WordRole::Other
                    };
                }
                arg_index += 1;
            }
            _ => {
                if hit {
                    return WordRole::Other;
                }
            }
        }
    }
    WordRole::Other
}

fn looks_like_flag(word: &str) -> bool {
    word.len() > 1 && word.starts_with('-')
}

fn is_assignment_word(word: &str) -> bool {
    match leading_identifier(word) {
        Some(prefix) => word[prefix.len()..].starts_with('='),
        None => false,
    }
}

fn hover_lines_for_command(name: &str) -> Vec<String> {
    use crate::bishedit::manpages::{self, ManStatus};
    let mut lines = vec![name.to_string()];
    match manpages::query(name) {
        ManStatus::Ready(data) => match &data.name_section {
            Some(snippet) => lines.push(snippet.clone()),
            None => lines.push("(found a man page, but no NAME section)".to_string()),
        },
        ManStatus::Pending => lines.push("looking up man page... press K again in a moment".to_string()),
        ManStatus::Missing => lines.push("no info available".to_string()),
    }
    lines
}

// Same idea, for a `command subcommand` pair -- tries the man page
// convention git/apt/ip/... use for their own per-subcommand pages
// (`command-subcommand(N)`, the same naming `bishedit::manpages::
// parse_subcommands` already recognizes for highlight.rs's own
// subcommand completion). A command with no such page for this word
// (most commands, most of the time -- this is a *position*-based guess,
// not a confirmed subcommand list) just reports "no info available",
// same as any other miss.
fn hover_lines_for_subcommand(command: &str, subcommand: &str) -> Vec<String> {
    use crate::bishedit::manpages::{self, ManStatus};
    let full = format!("{command}-{subcommand}");
    let mut lines = vec![format!("{command} {subcommand}")];
    match manpages::query(&full) {
        ManStatus::Ready(data) => match &data.name_section {
            Some(snippet) => lines.push(snippet.clone()),
            None => lines.push(format!("(found {full}, but no NAME section)")),
        },
        ManStatus::Pending => lines.push("looking up man page... press K again in a moment".to_string()),
        ManStatus::Missing => lines.push("no info available".to_string()),
    }
    lines
}

// A flag's own description, from the *enclosing command's* man page --
// `bishedit::manpages::query(command)` already parses every recognized
// flag's own description alongside its existing flags/subcommands scan
// (`ManPageData::flag_descriptions`), so this is just a lookup once
// that's ready, normalized through the same `extract_flag_token` the
// man-page parser itself uses (so a script's own `--block-size=1M`
// looks up the page's own `--block-size` entry, not a literal miss).
fn hover_lines_for_flag(command: &str, flag: &str) -> Vec<String> {
    use crate::bishedit::manpages::{self, ManStatus};
    let key = manpages::extract_flag_token(flag).unwrap_or_else(|| flag.to_string());
    let mut lines = vec![format!("{command} {flag}")];
    match manpages::query(command) {
        ManStatus::Ready(data) => match data.flag_descriptions.get(&key) {
            Some(desc) => lines.push(desc.clone()),
            None => lines.push(format!("(found a man page for {command}, but no description for {key})")),
        },
        ManStatus::Pending => lines.push("looking up man page... press K again in a moment".to_string()),
        ManStatus::Missing => lines.push("no info available".to_string()),
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes one file into `dir`, which owns it: these tests used to
    /// share a single directory named after the process and never
    /// remove it, so every run of the suite left one behind.
    fn write_temp(dir: &crate::tempdir::TempDir, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn a_comment_block_directly_above_a_function_becomes_its_doc() {
        let dir = crate::tempdir::TempDir::new("docs-tests");
        let path = write_temp(&dir, "fn1.sh", "# greet prints a friendly greeting.\n# name: who to greet.\ngreet() {\n    echo \"hi $1\"\n}\n");
        let index = DocIndex::build_from_source(&std::fs::read_to_string(&path).unwrap(), &path);
        let doc = index.lookup("greet").expect("greet should have a doc");
        assert!(matches!(doc.kind, SymbolKind::Function));
        assert_eq!(doc.doc, vec!["greet prints a friendly greeting.".to_string(), "name: who to greet.".to_string()]);
    }

    #[test]
    fn a_blank_line_between_the_comment_and_the_declaration_breaks_the_attachment() {
        let dir = crate::tempdir::TempDir::new("docs-tests");
        let path = write_temp(&dir, "fn2.sh", "# not attached, a blank line separates this from the function\n\ngreet() {\n    echo hi\n}\n");
        let index = DocIndex::build_from_source(&std::fs::read_to_string(&path).unwrap(), &path);
        assert!(index.lookup("greet").is_none());
    }

    #[test]
    fn a_top_level_bare_assignment_can_have_a_doc_too() {
        let dir = crate::tempdir::TempDir::new("docs-tests");
        let path = write_temp(&dir, "var1.sh", "# MAX_RETRIES caps how many times we retry.\nMAX_RETRIES=3\n");
        let index = DocIndex::build_from_source(&std::fs::read_to_string(&path).unwrap(), &path);
        let doc = index.lookup("MAX_RETRIES").expect("MAX_RETRIES should have a doc");
        assert!(matches!(doc.kind, SymbolKind::Variable));
        assert_eq!(doc.doc, vec!["MAX_RETRIES caps how many times we retry.".to_string()]);
    }

    #[test]
    fn a_declare_form_assignment_is_recognized_too() {
        let dir = crate::tempdir::TempDir::new("docs-tests");
        let path = write_temp(&dir, "var2.sh", "# TIMEOUT bounds how long we wait.\ndeclare -i TIMEOUT=30\n");
        let index = DocIndex::build_from_source(&std::fs::read_to_string(&path).unwrap(), &path);
        assert!(index.lookup("TIMEOUT").is_some());
    }

    #[test]
    fn source_with_a_static_path_is_followed_into_the_sourced_file() {
        let dir = crate::tempdir::TempDir::new("docs-tests");
        std::fs::write(dir.join("lib.sh"), "# helper does the real work.\nhelper() {\n    echo helping\n}\n").unwrap();
        let entry = write_temp(&dir, "main1.sh", "source lib.sh\nhelper\n");
        let index = DocIndex::build_from_source(&std::fs::read_to_string(&entry).unwrap(), &entry);
        let doc = index.lookup("helper").expect("helper should be found via source");
        assert_eq!(doc.doc, vec!["helper does the real work.".to_string()]);
    }

    #[test]
    fn source_with_a_dynamic_path_is_not_followed() {
        let dir = crate::tempdir::TempDir::new("docs-tests");
        std::fs::write(dir.join("dynlib.sh"), "# helper2 also does real work.\nhelper2() {\n    echo helping\n}\n").unwrap();
        let entry = write_temp(&dir, "main2.sh", "LIB_DIR=\"$PWD\"\nsource \"$LIB_DIR/dynlib.sh\"\n");
        let index = DocIndex::build_from_source(&std::fs::read_to_string(&entry).unwrap(), &entry);
        assert!(index.lookup("helper2").is_none());
    }

    #[test]
    fn the_entry_files_own_symbol_wins_over_a_same_named_sourced_one() {
        let dir = crate::tempdir::TempDir::new("docs-tests");
        std::fs::write(dir.join("lib3.sh"), "# from the library.\nshared() { :; }\n").unwrap();
        let entry = write_temp(&dir, "main3.sh", "# from the entry script.\nshared() { :; }\nsource lib3.sh\n");
        let index = DocIndex::build_from_source(&std::fs::read_to_string(&entry).unwrap(), &entry);
        assert_eq!(index.lookup("shared").unwrap().doc, vec!["from the entry script.".to_string()]);
    }
}

#[cfg(test)]
mod classify_word_tests {
    use super::*;

    // `col` locates the character to classify -- tests spell it out via
    // the byte index of a marker for readability, then convert to a
    // char index (identical for these all-ASCII fixtures).
    fn role_at(line: &str, marker: char) -> WordRole {
        let col = line.find(marker).expect("marker not found in fixture");
        classify_word(line, col)
    }

    #[test]
    fn the_first_word_is_the_command() {
        match role_at("read -p \"Your name: \" USER_NAME", 'r') {
            WordRole::Command(name) => assert_eq!(name, "read"),
            other => panic!("expected Command, got a different role: line {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn a_dash_prefixed_word_is_a_flag_of_the_enclosing_command() {
        match role_at("read -p \"Your name: \" USER_NAME", 'p') {
            WordRole::Flag { command, flag } => {
                assert_eq!(command, "read");
                assert_eq!(flag, "-p");
            }
            _ => panic!("expected Flag"),
        }
    }

    #[test]
    fn the_word_right_after_the_command_is_a_subcommand_candidate() {
        match role_at("git add file.txt", 'a') {
            WordRole::Subcommand { command, subcommand } => {
                assert_eq!(command, "git");
                assert_eq!(subcommand, "add");
            }
            _ => panic!("expected Subcommand"),
        }
    }

    #[test]
    fn text_inside_a_quoted_argument_is_not_command_shaped() {
        // "Your" sits inside one single quoted shell word ("Your name:
        // "), not as its own bareword -- must not be classified as a
        // command/subcommand/flag at all, so K-hover never spawns a
        // pointless `man Your` lookup for ordinary prose.
        assert!(matches!(role_at("read -p \"Your name: \" USER_NAME", 'Y'), WordRole::Other));
    }

    #[test]
    fn a_later_plain_argument_is_not_a_subcommand() {
        // USER_NAME is the *second* non-flag word after `read` (after
        // the quoted prompt string), not the first -- must not read as
        // a subcommand position.
        assert!(matches!(role_at("read -p \"Your name: \" USER_NAME", 'U'), WordRole::Other));
    }

    #[test]
    fn an_assignment_prefix_word_does_not_count_as_the_command() {
        match role_at("FOO=bar echo hi", 'e') {
            WordRole::Command(name) => assert_eq!(name, "echo"),
            _ => panic!("expected Command"),
        }
    }

    #[test]
    fn each_side_of_a_pipeline_gets_its_own_command() {
        match role_at("cat file.txt | grep foo", 'g') {
            WordRole::Command(name) => assert_eq!(name, "grep"),
            _ => panic!("expected Command"),
        }
    }
}
