// Doc comments for bash scripts: a `#` comment block directly above a
// function or top-level variable, with no blank line between, documents
// it, as godoc does for Go. Used by `K` hover and the outline, with bish's
// own parser.
//
// `source PATH` is followed when PATH is a static literal (see
// `word_as_literal`), so a symbol from a sourced library hovers the same.
// A computed path (`source "$LIB_DIR/util.sh"`) can't be resolved without
// running the script, so it isn't followed.
//
// Only top-level statements are scanned: a declaration inside an `if` or
// loop body isn't seen, while library functions usually sit at the top
// level.

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
    // One entry per comment line, `#` and one following space stripped,
    // kept as separate lines so the author's line breaks (an "Args:" list)
    // survive into the popup.
    pub doc: Vec<String>,
    pub file: PathBuf,
    pub line: usize,
}

pub struct DocIndex {
    symbols: HashMap<String, SymbolDoc>,
}

impl DocIndex {
    // Scans `src`, the entry script's text (debugger.rs reads it once;
    // the file editor passes its buffer, unsaved edits included), and
    // recursively every file it statically sources, read from disk. A file
    // that can't be read or parsed contributes nothing. `entry_path` is what
    // the script's own `source`s resolve against and what its symbols are
    // attributed to; it needn't exist on disk yet. It's marked visited up
    // front, so a `source` of itself reuses `src` rather than stale disk
    // content.
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

// The shared half of `scan_file` and `build_from_source`, which differ
// only in where `src` came from.
fn scan_source(src: &str, path: &Path, symbols: &mut HashMap<String, SymbolDoc>, visited: &mut HashSet<PathBuf>) {
    let Ok(toks) = Lexer::new(src).tokenize() else { return };
    let Ok(program) = Parser::new(toks).parse_program() else { return };
    let lines: Vec<&str> = src.lines().collect();

    // Collected rather than followed now, so the entry file's symbols are
    // recorded first and win over a sourced file's redeclaration.
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
                        // A bare assignment (`NAME=value`, `arr=(1 2 3)`,
                        // `arr[i]=value`), with no command word.
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

// Every command in every stage of an AndOr chain (`a && b | c || d`),
// since a documented declaration could sit anywhere in it.
fn pipeline_commands(and_or: &AndOr) -> Vec<&Command> {
    let mut out: Vec<&Command> = and_or.first.commands.iter().collect();
    for (_, pipeline) in &and_or.rest {
        out.extend(pipeline.commands.iter());
    }
    out
}

/// Every function `src` defines at the top level, in order, with its
/// 1-based start line and doc comment if any. Not `DocIndex`, which only
/// records documented symbols: an outline wants every function.
pub fn functions_in(src: &str) -> Vec<(String, usize, Vec<String>)> {
    let Ok(toks) = Lexer::new(src).tokenize() else { return Vec::new() };
    let Ok(program) = Parser::new(toks).parse_program() else { return Vec::new() };
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    for item in &program {
        for cmd in pipeline_commands(&item.and_or) {
            if let Command::FuncDef { name, .. } = cmd {
                out.push((name.clone(), item.line, doc_comment_above(&lines, item.line)));
            }
        }
    }
    out
}

// Records a symbol only if it has a doc comment (an undocumented name
// falls through to the man-page fallback in hover), and only the first
// seen, so the entry script's declarations beat a sourced file's.
fn record_symbol(symbols: &mut HashMap<String, SymbolDoc>, name: String, kind: SymbolKind, file: &Path, line: usize, doc: Vec<String>) {
    if doc.is_empty() {
        return;
    }
    symbols.entry(name).or_insert(SymbolDoc { kind, doc, file: file.to_path_buf(), line });
}

// Collects the run of `#` lines directly above `decl_line` (1-based),
// stopping at a blank line or code, as godoc does. Top to bottom, `#` and
// one space stripped.
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

// `Some` when every chunk of `word` is plain literal text: a path with an
// expansion can't be resolved without running the script.
fn word_as_literal(word: &Word) -> Option<String> {
    let mut s = String::new();
    for chunk in &word.chunks {
        match chunk {
            Chunk::Str(t) | Chunk::LiteralStr(t, _) => s.push_str(t),
            _ => return None,
        }
    }
    Some(s)
}

// The identifier at the start of a `declare`/`local` word: `NAME` from
// `NAME`, `NAME=value` or `NAME+=value`. `None` without a valid
// identifier.
fn leading_identifier(s: &str) -> Option<String> {
    let end = s.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(s.len());
    let ident = &s[..end];
    match ident.chars().next() {
        Some(c) if c.is_alphabetic() || c == '_' => Some(ident.to_string()),
        _ => None,
    }
}

// Resolves a literal `source` argument. bash searches $PATH for a
// relative name not found in the current directory; with no running
// shell, this tries the sourcing file's directory (the usual convention
// for a script's libraries), then this process's cwd. `None` if neither
// exists, so the wrong file is never scanned.
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

// The identifier (`[A-Za-z0-9_]+`) touching column `col`: what `K` looks
// up, in the debugger and the editor. `None` when the cursor isn't on one.
pub fn identifier_at(chars: &[char], col: usize) -> Option<String> {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    if col >= chars.len() || !is_ident(chars[col]) {
        return None;
    }
    let start = (0..=col).rev().take_while(|&i| is_ident(chars[i])).last().unwrap_or(col);
    let end = (col..chars.len()).take_while(|&i| is_ident(chars[i])).count() + col;
    Some(chars[start..end].iter().collect())
}

/// What `K` says when `hover_lines_at` found nothing, shared by the
/// editor's and the debugger's hover.
pub const NOTHING_UNDER_THE_CURSOR: &str = "no identifier under the cursor";

/// What a hover says when it looked and found nothing to tell you.
pub const NO_INFO: &str = "no info available";

/// Shown while a man page is fetched in the background: advice about a
/// key, right for `K` and wrong for anything not pressed.
pub const STILL_LOOKING: &str = "looking up man page... press K again in a moment";

/// Whether these lines are a placeholder for a lookup still in flight.
/// `K` shows them anyway; a popup that appears on its own should wait for
/// something worth showing.
pub fn is_still_looking(lines: &[String]) -> bool {
    lines.iter().any(|l| l == STILL_LOOKING)
}

/// Whether these lines say "nothing known": a recognized word with no
/// documentation anywhere.
pub fn says_nothing(lines: &[String]) -> bool {
    lines.is_empty() || lines.iter().all(|l| l == NO_INFO || l == NOTHING_UNDER_THE_CURSOR) || lines.last().is_some_and(|l| l == NO_INFO)
}

// `K`'s entry point. `chars` and `col` locate the cursor on its line,
// `line_text` is the same line for `classify_word` to tokenize, and
// `live_value` answers whether an identifier has a live value
// (`Shell::debug_peek_var` in the debugger; always `None` in the editor).
//
// A documented symbol or a live value wins wherever the identifier sits.
// Otherwise `classify_word` decides whether the cursor is on a command
// name, subcommand or flag before `man` is asked, so a positional
// argument, or prose in a quoted string, finds nothing.
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
        // Empty means nothing is here, unlike a word about which nothing is
        // known. `K` turns it into `NOTHING_UNDER_THE_CURSOR`, since a key
        // press deserves an answer; a mouse-over hover and
        // `bish tool lsp-server` want silence.
        WordRole::Other => match identifier {
            Some(name) => vec![name, NO_INFO.to_string()],
            None => Vec::new(),
        },
    }
}

// The value or doc-comment half of `hover_lines_at`: a live value wins,
// then the index's doc comment. Called with neither only from elsewhere,
// giving a bare name.
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

// The role of the word at `col` (0-based chars) in its command, which
// decides whether a man lookup makes sense. Re-tokenizes `line` with
// `tokenize_spanned` for per-token byte ranges, so the cursor is known to
// be on a token (a quoted string's prose isn't command-shaped), tracking
// which command (reset at `;`, `&&`, `||`, `&`, newline) and argument
// position the token falls in.
pub enum WordRole {
    // The command name itself, after any `FOO=bar` prefix words.
    Command(String),
    // The first non-flag argument: a possible subcommand position, which
    // `hover_lines_for_subcommand`'s man lookup confirms or not.
    Subcommand { command: String, subcommand: String },
    // A `-x` or `--long[=value]` word, with its command if known.
    Flag { command: String, flag: String },
    // Anything else: a later argument, a redirect target, a separator, or an
    // unclassifiable word (often half of a multi-line construct being typed).
    Other,
}

pub fn classify_word(line: &str, col: usize) -> WordRole {
    let byte_col: usize = line.chars().take(col).map(|c| c.len_utf8()).sum();
    let spanned = crate::lexer::tokenize_spanned(line);
    let mut command: Option<String> = None;
    // Non-flag words seen since `command` was set; 0 means the next one is
    // in subcommand position.
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
                        // `globbable` (Tok::Word's second field) is true
                        // only for a bare word with no quoting or
                        // expansion. A quoted word here (`"$0"`
                        // indirection) isn't worth a man lookup, so it's
                        // Other, but still becomes `command` so following
                        // flags have a command to belong to.
                        return if globbable { WordRole::Command(text) } else { WordRole::Other };
                    }
                    command = Some(text);
                    arg_index = 0;
                    continue;
                }
                if hit {
                    // The same for the subcommand position: a quoted
                    // argument there, like `read -p "Your name: "`'s
                    // prompt, isn't a subcommand.
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
        ManStatus::Pending => lines.push(STILL_LOOKING.to_string()),
        ManStatus::Missing => lines.push(NO_INFO.to_string()),
    }
    lines
}

// A `command subcommand` pair: tries the `command-subcommand(N)` page
// convention git, apt and ip use (as `manpages::parse_subcommands`
// recognizes). This guesses by position, so most words find no page and
// get "no info available".
fn hover_lines_for_subcommand(command: &str, subcommand: &str) -> Vec<String> {
    use crate::bishedit::manpages::{self, ManStatus};
    let full = format!("{command}-{subcommand}");
    let mut lines = vec![format!("{command} {subcommand}")];
    match manpages::query(&full) {
        ManStatus::Ready(data) => match &data.name_section {
            Some(snippet) => lines.push(snippet.clone()),
            None => lines.push(format!("(found {full}, but no NAME section)")),
        },
        ManStatus::Pending => lines.push(STILL_LOOKING.to_string()),
        ManStatus::Missing => lines.push(NO_INFO.to_string()),
    }
    lines
}

// A flag's description from the command's man page
// (`ManPageData::flag_descriptions`), normalized with
// `extract_flag_token`, so `--block-size=1M` finds `--block-size`.
fn hover_lines_for_flag(command: &str, flag: &str) -> Vec<String> {
    use crate::bishedit::manpages::{self, ManStatus};
    let key = manpages::extract_flag_token(flag).unwrap_or_else(|| flag.to_string());
    let mut lines = vec![format!("{command} {flag}")];
    match manpages::query(command) {
        ManStatus::Ready(data) => match data.flag_descriptions.get(&key) {
            Some(desc) => lines.push(desc.clone()),
            None => lines.push(format!("(found a man page for {command}, but no description for {key})")),
        },
        ManStatus::Pending => lines.push(STILL_LOOKING.to_string()),
        ManStatus::Missing => lines.push(NO_INFO.to_string()),
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes one file into `dir`, which owns it and cleans up.
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

    // `col` is given by the position of a marker character, for readability
    // (byte and char indices agree for these ASCII fixtures).
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
        // `Your` is inside a quoted word, so it's not a command, subcommand
        // or flag, and `K` doesn't look up `man Your`.
        assert!(matches!(role_at("read -p \"Your name: \" USER_NAME", 'Y'), WordRole::Other));
    }

    #[test]
    fn a_later_plain_argument_is_not_a_subcommand() {
        // `USER_NAME` is the second non-flag word after `read`, not a
        // subcommand position.
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

#[cfg(test)]
mod hover_quietness_tests {
    use super::*;

    /// `K` answers with whatever it has; a popup that appears on its own
    /// stays quiet unless it has something to say.
    #[test]
    fn a_placeholder_is_told_apart_from_an_answer() {
        assert!(is_still_looking(&["grep".to_string(), STILL_LOOKING.to_string()]));
        assert!(!is_still_looking(&["grep".to_string(), "grep - print lines that match".to_string()]));

        assert!(says_nothing(&[]));
        assert!(says_nothing(&["deploy".to_string(), NO_INFO.to_string()]));
        assert!(says_nothing(&[NOTHING_UNDER_THE_CURSOR.to_string()]));
        assert!(!says_nothing(&["greet".to_string(), "Greets somebody.".to_string()]));
    }

    // A real answer whose last line is about something else isn't silence:
    // the marker is the whole line, not a substring.
    #[test]
    fn an_answer_mentioning_the_marker_in_passing_still_counts() {
        assert!(!says_nothing(&["x".to_string(), format!("see also: {NO_INFO}")]));
        assert!(!is_still_looking(&["x".to_string(), format!("almost: {STILL_LOOKING}!")]));
    }
}
