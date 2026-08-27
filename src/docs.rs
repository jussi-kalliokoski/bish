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
    // Scans `entry_path` and, recursively, every file it statically
    // `source`s -- see this module's own doc comment. Best-effort: a
    // file that can't be read or fails to parse just contributes nothing
    // rather than aborting the whole scan (matching debugger.rs's own
    // "never let a documentation feature crash the debugger" posture).
    pub fn build(entry_path: &Path) -> DocIndex {
        let mut symbols = HashMap::new();
        let mut visited = HashSet::new();
        scan_file(entry_path, &mut symbols, &mut visited);
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
    let Ok(toks) = Lexer::new(&src).tokenize() else { return };
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
                        for (name, _, _) in &sc.index_assigns {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bish-docs-tests-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn a_comment_block_directly_above_a_function_becomes_its_doc() {
        let path = write_temp("fn1.sh", "# greet prints a friendly greeting.\n# name: who to greet.\ngreet() {\n    echo \"hi $1\"\n}\n");
        let index = DocIndex::build(&path);
        let doc = index.lookup("greet").expect("greet should have a doc");
        assert!(matches!(doc.kind, SymbolKind::Function));
        assert_eq!(doc.doc, vec!["greet prints a friendly greeting.".to_string(), "name: who to greet.".to_string()]);
    }

    #[test]
    fn a_blank_line_between_the_comment_and_the_declaration_breaks_the_attachment() {
        let path = write_temp("fn2.sh", "# not attached, a blank line separates this from the function\n\ngreet() {\n    echo hi\n}\n");
        let index = DocIndex::build(&path);
        assert!(index.lookup("greet").is_none());
    }

    #[test]
    fn a_top_level_bare_assignment_can_have_a_doc_too() {
        let path = write_temp("var1.sh", "# MAX_RETRIES caps how many times we retry.\nMAX_RETRIES=3\n");
        let index = DocIndex::build(&path);
        let doc = index.lookup("MAX_RETRIES").expect("MAX_RETRIES should have a doc");
        assert!(matches!(doc.kind, SymbolKind::Variable));
        assert_eq!(doc.doc, vec!["MAX_RETRIES caps how many times we retry.".to_string()]);
    }

    #[test]
    fn a_declare_form_assignment_is_recognized_too() {
        let path = write_temp("var2.sh", "# TIMEOUT bounds how long we wait.\ndeclare -i TIMEOUT=30\n");
        let index = DocIndex::build(&path);
        assert!(index.lookup("TIMEOUT").is_some());
    }

    #[test]
    fn source_with_a_static_path_is_followed_into_the_sourced_file() {
        let dir = std::env::temp_dir().join(format!("bish-docs-tests-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("lib.sh"), "# helper does the real work.\nhelper() {\n    echo helping\n}\n").unwrap();
        let entry = write_temp("main1.sh", "source lib.sh\nhelper\n");
        let index = DocIndex::build(&entry);
        let doc = index.lookup("helper").expect("helper should be found via source");
        assert_eq!(doc.doc, vec!["helper does the real work.".to_string()]);
    }

    #[test]
    fn source_with_a_dynamic_path_is_not_followed() {
        let dir = std::env::temp_dir().join(format!("bish-docs-tests-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("dynlib.sh"), "# helper2 also does real work.\nhelper2() {\n    echo helping\n}\n").unwrap();
        let entry = write_temp("main2.sh", "LIB_DIR=\"$PWD\"\nsource \"$LIB_DIR/dynlib.sh\"\n");
        let index = DocIndex::build(&entry);
        assert!(index.lookup("helper2").is_none());
    }

    #[test]
    fn the_entry_files_own_symbol_wins_over_a_same_named_sourced_one() {
        let dir = std::env::temp_dir().join(format!("bish-docs-tests-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("lib3.sh"), "# from the library.\nshared() { :; }\n").unwrap();
        let entry = write_temp("main3.sh", "# from the entry script.\nshared() { :; }\nsource lib3.sh\n");
        let index = DocIndex::build(&entry);
        assert_eq!(index.lookup("shared").unwrap().doc, vec!["from the entry script.".to_string()]);
    }
}
