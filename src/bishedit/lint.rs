// Linters: bash scripts, JSON and TOML.
//
// The bash linter is built on `lexer::tokenize_spanned`, like highlight.rs,
// because the AST carries no source spans. The token stream is enough for
// every rule here: command and argument positions (as highlight.rs's
// CmdPos/WordRole), `[[ ... ]]` nesting, and each word's chunks, which
// already carry the lexer's `quoted` flag.
//
// `Linter` is the core behind `bish tool check`, the editor's diagnostics
// and `bish tool lsp-server`: text in, diagnostics out. `Fix` spans are
// char offsets into the same text, applied by slicing (tool.rs's
// `apply_fixes`).
//
// Only rules that catch a real bug or risk (unquoted expansions that can
// split or glob, a substitution's status masked by `local`). Style is
// `bish tool format`'s job.

use crate::bishedit::highlight::resets_command_position;
use crate::lexer::{self, Chunk, SpannedItem, Tok};
use crate::parser;
use std::borrow::Cow;
use std::ops::Range;

// Least severe first, so a gutter cell showing one mark for several
// findings takes `.max()`. Diagnostics are listed in source order, so
// nothing else depends on this order. LSP's four, in LSP's meaning: bish's
// linter only emits `Warning`, but language-server findings land in the
// same `TextBuffer::diagnostics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Hint,
    Info,
    Warning,
    Error,
}

// One edit: replace `text[start..end]` (char offsets) with
// `replacement`. A rule that touches two places (masked-return-value's
// two-statement rewrite) uses one replacement spanning both, so applying
// many fixes is sorting by descending `start` and splicing (see tool.rs's
// `apply_fixes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub start: usize,
    pub end: usize,
    pub severity: Severity,
    // A short, stable kebab-case name for the rule, which a suppression
    // comment would key on. `Cow` because a language server's code is a
    // runtime string sharing this `Vec`; bish's own rules still use literals.
    pub code: Cow<'static, str>,
    // Who found it: `None` for bish's own linters, `Some("rustc")` for a
    // language server's. Separate from `code` because two servers can both
    // say `E0308` and mean different things.
    pub source: Option<String>,
    pub message: String,
    // `None` when there's no fix that's safe to apply automatically (see
    // `masked_return_value`).
    pub fix: Option<Fix>,
}

impl Diagnostic {
    /// The finding's name in a list: `code` for bish's own, `source:code`
    /// otherwise. Used by `bish tool check` and the editor's diagnostics
    /// pane.
    pub fn label(&self) -> String {
        match &self.source {
            Some(source) => format!("{source}:{}", self.code),
            None => self.code.to_string(),
        }
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn a_label_names_its_source_only_when_something_other_than_bish_found_it() {
        let mut d = Diagnostic {
            start: 0,
            end: 1,
            severity: Severity::Warning,
            code: Cow::Borrowed("unquoted-expansion"),
            source: None,
            message: String::new(),
            fix: None,
        };
        assert_eq!(d.label(), "unquoted-expansion");
        d.code = Cow::Owned("E0308".to_string());
        d.source = Some("rustc".to_string());
        assert_eq!(d.label(), "rustc:E0308");
    }
}

pub trait Linter {
    fn check(&self, text: &str) -> Vec<Diagnostic>;
}

/// JSON, from the tokenizer. Lexical, not structural: `json::tokens` marks
/// each bad piece at its exact offset, which a gutter mark and an underline
/// need. A structural error from `json::parse` has its position only in its
/// message, so a mismatched brace is reported once, against the whole
/// buffer, in the parser's words.
///
/// Not JSONC: the JSON tokenizer can't tell a `//` comment from an error,
/// so every comment in a `tsconfig.json` would be flagged.
pub struct JsonLinter;

impl Linter for JsonLinter {
    fn check(&self, text: &str) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        for token in crate::json::tokens(text) {
            if let crate::json::TokenKind::Invalid(reason) = &token.kind {
                out.push(Diagnostic {
                    start: token.start,
                    end: token.end.max(token.start + 1),
                    severity: Severity::Error,
                    code: Cow::Borrowed("json-syntax"),
                    source: None,
                    message: reason.clone(),
                    fix: None,
                });
            }
        }
        // Only when nothing lexical was found: a stray character reported
        // above is what made the parse fail.
        if out.is_empty()
            && !text.trim().is_empty()
            && let Err(message) = crate::json::parse(text)
        {
            {
                out.push(Diagnostic {
                    start: 0,
                    end: text.chars().count().min(1),
                    severity: Severity::Error,
                    code: Cow::Borrowed("json-parse"),
                    source: None,
                    message,
                    fix: None,
                });
            }
        }
        out
    }
}

/// TOML, from toml.rs's parser. `toml::ParseError` carries a char offset,
/// so the mark lands on the mistake. One finding: the parser stops at the
/// first thing it can't read.
pub struct TomlLinter;

impl Linter for TomlLinter {
    fn check(&self, text: &str) -> Vec<Diagnostic> {
        let Err(e) = crate::toml::parse(text) else { return Vec::new() };
        let len = text.chars().count();
        let start = e.at.min(len.saturating_sub(1));
        vec![Diagnostic {
            start,
            // To the end of the line, so there's something to underline.
            end: text.chars().skip(start).position(|c| c == '\n').map_or(len, |n| start + n).max(start + 1).min(len),
            severity: Severity::Error,
            code: Cow::Borrowed("toml-syntax"),
            source: None,
            message: e.message,
            fix: None,
        }]
    }
}

pub struct BashLinter;

impl Linter for BashLinter {
    fn check(&self, text: &str) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        lint_into(text, 0, &mut out);
        out
    }
}

// The next `raw_capture_spans` entry, if any; see highlight.rs's
// `next_span`.
fn next_span(raw_spans: &[Range<usize>], cursor: &mut usize) -> Option<Range<usize>> {
    let span = raw_spans.get(*cursor).cloned();
    if span.is_some() {
        *cursor += 1;
    }
    span
}

// `raw_capture_spans` covers an expansion's content without its
// delimiters (`$`, `${`/`}`, `$(`/`)`, backticks; see lexer.rs). A
// wrap-in-quotes fix needs the whole expansion: quoting only `foo` in
// `$foo` gives `$"foo"`, a locale-translated string.
//
// Only `Chunk::Var` can be either `$foo` or `${foo}`, so the form is read
// off the characters around `content` in the source. If no known delimiter
// pair matches, `content` is returned: a fix without the sigil beats a
// wrong splice.
fn expansion_source_span(chars: &[char], content: Range<usize>) -> Range<usize> {
    let before = |n: usize| content.start.checked_sub(n).and_then(|i| chars.get(i));
    if before(1) == Some(&'{') && before(2) == Some(&'$') {
        let end = if chars.get(content.end) == Some(&'}') { content.end + 1 } else { content.end };
        return content.start - 2..end;
    }
    if before(1) == Some(&'(') && before(2) == Some(&'$') {
        let end = if chars.get(content.end) == Some(&')') { content.end + 1 } else { content.end };
        return content.start - 2..end;
    }
    if before(1) == Some(&'`') {
        let end = if chars.get(content.end) == Some(&'`') { content.end + 1 } else { content.end };
        return content.start - 1..end;
    }
    if before(1) == Some(&'$') {
        return content.start - 1..content.end;
    }
    content
}

fn char_slice(chars: &[char], range: Range<usize>) -> String {
    chars[range].iter().collect()
}

// `NAME=value` or `NAME+=value`, via `parser::word_as_assignment` so it
// agrees with the parser. Returns the name and the length in chars of the
// `NAME=` prefix.
fn assignment_prefix(chunks: &[Chunk]) -> Option<(String, usize)> {
    let w = parser::Word { chunks: chunks.to_vec(), globbable: false };
    let (name, mode, _value) = parser::word_as_assignment(&w)?;
    let op_len = match mode {
        parser::AssignMode::Set => 1,
        parser::AssignMode::Append => 2,
    };
    let prefix_len = name.chars().count() + op_len;
    Some((name, prefix_len))
}

fn word_contains_command_sub(chunks: &[Chunk]) -> bool {
    chunks.iter().any(|c| matches!(c, Chunk::Sub { .. }))
}

const DECLARE_KEYWORDS: &[&str] = &["local", "declare", "export", "readonly", "typeset"];

// Whether `masked_return_value`'s fix can split this command's argument
// into two statements: only when the command's one argument is an
// assignment. A flag (`local -r x=$(cmd)` would then assign to a readonly
// name), a second assignment or a positional argument leave it
// detect-only. `readonly` is never fixable, for the same reason as `-r`.
//
// Reads `items` without touching `cursor`: it only classifies word shapes
// and needs no spans.
fn declare_fixable(items: &[SpannedItem], start: usize, keyword: &str) -> bool {
    if keyword == "readonly" {
        return false;
    }
    let mut count = 0usize;
    let mut sole_is_assignment = false;
    for item in &items[start..] {
        let SpannedItem::Tok(tok, _) = item else { continue };
        if resets_command_position(tok) {
            break;
        }
        if let Tok::Word(chunks, _) = tok {
            let is_flag = matches!(chunks.as_slice(), [Chunk::Str(s)] if s.starts_with('-') && s.len() > 1);
            if is_flag {
                return false;
            }
            count += 1;
            sole_is_assignment = assignment_prefix(chunks).is_some();
        }
    }
    count == 1 && sole_is_assignment
}

// The indentation of the line containing `pos`, which the split
// statement reuses.
fn line_indent(chars: &[char], pos: usize) -> String {
    let line_start = chars[..pos].iter().rposition(|&c| c == '\n').map(|i| i + 1).unwrap_or(0);
    let indent_end = chars[line_start..pos].iter().position(|&c| !matches!(c, ' ' | '\t')).map(|i| line_start + i).unwrap_or(pos);
    char_slice(chars, line_start..indent_end)
}

// The command or argument position of a word, as in highlight.rs. Kept
// separate because the two walkers need different things from it.
enum WordRole {
    AssignmentPrefix,
    CommandName,
    Argument { arg_index: usize },
}

#[derive(Clone)]
enum CmdPos {
    ExpectCommand,
    InCommand { arg_index: usize },
}

fn resolve_role(cmd_pos: &mut CmdPos, chunks: &[Chunk]) -> WordRole {
    match cmd_pos {
        CmdPos::ExpectCommand if assignment_prefix(chunks).is_some() => WordRole::AssignmentPrefix,
        CmdPos::ExpectCommand => {
            *cmd_pos = CmdPos::InCommand { arg_index: 0 };
            WordRole::CommandName
        }
        CmdPos::InCommand { arg_index } => {
            let role = WordRole::Argument { arg_index: *arg_index };
            *arg_index += 1;
            role
        }
    }
}

// One pass over the token stream, recursing into `$(...)`, backticks,
// `<(...)` and `>(...)` like highlight.rs's `highlight_into`: fresh state
// per level, offsets translated back to the outer text. Heredoc bodies
// aren't recursed into, since `raw_capture_spans` can desync there (see
// highlight.rs's `next_span`).
fn lint_into(text: &str, offset: usize, out: &mut Vec<Diagnostic>) {
    let chars: Vec<char> = text.chars().collect();
    let res = lexer::tokenize_spanned(text);
    let mut cursor = 0usize;
    let mut cmd_pos = CmdPos::ExpectCommand;
    let mut bracket2_depth: u32 = 0;
    // `case WORD in`: WORD is never word-split (POSIX), so it's exempt like
    // `[[ ... ]]`'s contents. Set after `case`, consumed by the next word,
    // and cleared by `in` in case of half-typed input.
    let mut case_subject = false;
    // Set when a command name is one of DECLARE_KEYWORDS: the keyword, and
    // whether `declare_fixable` allows the split. Lives as long as the
    // command.
    let mut declare_ctx: Option<(String, bool)> = None;

    for i in 0..res.items.len() {
        let SpannedItem::Tok(tok, span) = &res.items[i] else { continue };
        if resets_command_position(tok) {
            cmd_pos = CmdPos::ExpectCommand;
        }
        if matches!(tok, Tok::KwFor | Tok::KwSelect | Tok::KwCase | Tok::KwIn) {
            cmd_pos = CmdPos::InCommand { arg_index: 0 };
        }
        match tok {
            Tok::KwLBracket2 => bracket2_depth += 1,
            Tok::KwRBracket2 => bracket2_depth = bracket2_depth.saturating_sub(1),
            Tok::KwCase => case_subject = true,
            Tok::KwIn => case_subject = false,
            Tok::Subshell { raw, .. } => {
                if let Some(inner) = next_span(&res.raw_capture_spans, &mut cursor) {
                    lint_into(raw, offset + inner.start, out);
                }
            }
            Tok::Arith(_raw) => {
                // Like highlight.rs's `Tok::Arith`: no recursion, consumed
                // only to keep `cursor` in step.
                next_span(&res.raw_capture_spans, &mut cursor);
            }
            Tok::Word(chunks, _) => {
                let this_case_subject = case_subject;
                case_subject = false;
                let role = resolve_role(&mut cmd_pos, chunks);

                if let WordRole::CommandName = role {
                    declare_ctx = match chunks.as_slice() {
                        [Chunk::Str(s)] if DECLARE_KEYWORDS.contains(&s.as_str()) => Some((s.clone(), declare_fixable(&res.items, i + 1, s))),
                        _ => None,
                    };
                }

                let mut exempt = matches!(role, WordRole::AssignmentPrefix) || this_case_subject || bracket2_depth > 0;

                if let WordRole::Argument { arg_index, .. } = &role
                    && let (Some((keyword, fixable)), Some((name, prefix_len))) = (&declare_ctx, assignment_prefix(chunks))
                {
                    exempt = true;
                    if word_contains_command_sub(chunks) {
                        let word_start = offset + span.start;
                        let word_end = offset + span.end;
                        let fix = (*fixable && *arg_index == 0).then(|| {
                            let indent = line_indent(&chars, span.start);
                            // `=` or `+=`, read from the source, so an
                            // append isn't rewritten into an assignment.
                            let op_text = char_slice(&chars, span.start + name.chars().count()..span.start + prefix_len);
                            let value_text = char_slice(&chars, span.start + prefix_len..span.end);
                            Fix { start: word_start, end: word_end, replacement: format!("{name}\n{indent}{name}{op_text}{value_text}") }
                        });
                        out.push(Diagnostic {
                            start: word_start,
                            end: word_end,
                            severity: Severity::Warning,
                            code: Cow::Borrowed("masked-return-value"),
                            source: None,
                            message: format!(
                                "`{keyword}` masks `{name}`'s command substitution's own exit status -- assign it in a separate statement to check it"
                            ),
                            fix,
                        });
                    }
                }

                lint_word_chunks(chunks, offset, &chars, &res.raw_capture_spans, &mut cursor, exempt, out);
            }
            _ => {}
        }
    }
}

// Every expansion chunk in one word. Substitutions are recursed into
// whether or not the word is `exempt`, which only spares this word's own
// splitting risk; an unquoted, splitting-sensitive expansion is flagged
// unless `exempt`.
fn lint_word_chunks(
    chunks: &[Chunk],
    offset: usize,
    chars: &[char],
    raw_spans: &[Range<usize>],
    cursor: &mut usize,
    exempt: bool,
    out: &mut Vec<Diagnostic>,
) {
    // A wholly plain word has no `raw_capture_spans` entry and nothing to
    // flag.
    if let [Chunk::Str(_)] = chunks {
        return;
    }
    for chunk in chunks {
        match chunk {
            Chunk::Str(_) | Chunk::Tilde { .. } => {}
            Chunk::LiteralStr(_, _) => {
                next_span(raw_spans, cursor);
            }
            Chunk::ArrayLength { .. } => {
                // `${#name}` is always a non-negative integer, so quoting
                // it is style, as for `$#`, `$?`, `$$`, `$!` and `$-`
                // below.
                next_span(raw_spans, cursor);
            }
            Chunk::Var { name, quoted, .. } | Chunk::Indirect { name, quoted } | Chunk::ArrayKeys { name, quoted } => {
                if let Some(content) = next_span(raw_spans, cursor) {
                    let always_safe = matches!(chunk, Chunk::Var { .. }) && matches!(name.as_str(), "?" | "$" | "!" | "#" | "-");
                    if !exempt && !quoted && !always_safe {
                        push_unquoted_expansion(chars, offset, content, out);
                    }
                }
            }
            Chunk::VarExpand { quoted, .. }
            | Chunk::ArrayVar { quoted, .. }
            | Chunk::ArrayVarExpand { quoted, .. }
            | Chunk::VarNamesMatchingPrefix { quoted, .. } => {
                if let Some(content) = next_span(raw_spans, cursor)
                    && !exempt
                    && !quoted
                {
                    push_unquoted_expansion(chars, offset, content, out);
                }
            }
            Chunk::Sub { raw, quoted, .. } => {
                if let Some(content) = next_span(raw_spans, cursor) {
                    let inner_offset = offset + content.start;
                    if !exempt && !quoted {
                        push_unquoted_expansion(chars, offset, content, out);
                    }
                    lint_into(raw, inner_offset, out);
                }
            }
            Chunk::Arith { .. } => {
                // An arithmetic result is always an integer, so safe
                // unquoted, and not recursed into, like `Tok::Arith`.
                next_span(raw_spans, cursor);
            }
            Chunk::ProcSubIn { raw } | Chunk::ProcSubOut { raw } => {
                if let Some(content) = next_span(raw_spans, cursor) {
                    lint_into(raw, offset + content.start, out);
                }
            }
        }
    }
}

fn push_unquoted_expansion(chars: &[char], offset: usize, content: Range<usize>, out: &mut Vec<Diagnostic>) {
    let full = expansion_source_span(chars, content);
    let start = offset + full.start;
    let end = offset + full.end;
    out.push(Diagnostic {
        start,
        end,
        severity: Severity::Warning,
        code: Cow::Borrowed("unquoted-expansion"),
        source: None,
        message: "Unquoted expansion may be word-split or glob-expanded here -- wrap it in double quotes".to_string(),
        fix: Some(Fix { start, end, replacement: format!("\"{}\"", char_slice(chars, full)) }),
    });
}

#[cfg(test)]
mod toml_linter_tests {
    use super::*;

    fn check(text: &str) -> Vec<Diagnostic> {
        TomlLinter.check(text)
    }

    #[test]
    fn a_broken_cargo_toml_is_pointed_at() {
        let text = "[package]\nname = \"bish\"\nversion = \n";
        let found = check(text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].code, "toml-syntax");
        // On the third line, not the whole buffer.
        let line = text.chars().take(found[0].start).filter(|c| *c == '\n').count();
        assert_eq!(line, 2);
    }

    #[test]
    fn the_underline_covers_something() {
        let found = check("a = 1\nb = @\n");
        assert!(found[0].end > found[0].start, "an empty span would draw nothing");
    }

    #[test]
    fn a_valid_document_is_quiet() {
        assert!(check("[a]\nb = 1\nc = [1, 2]\n").is_empty());
        assert!(check("").is_empty());
    }

    // One finding: after the first unreadable thing the shape is unknown.
    #[test]
    fn only_the_first_problem_is_reported() {
        assert_eq!(check("a = \nb = \nc = \n").len(), 1);
    }
}

#[cfg(test)]
mod json_linter_tests {
    use super::*;

    fn check(text: &str) -> Vec<Diagnostic> {
        JsonLinter.check(text)
    }

    #[test]
    fn a_lexical_error_is_reported_where_it_actually_is() {
        let text = "{\"a\": 1, \"b\": tru}";
        let found = check(text);
        assert_eq!(found.len(), 1);
        assert!(found[0].message.contains("tru"), "{}", found[0].message);
        assert_eq!(&text[found[0].start..found[0].end], "tru");
    }

    #[test]
    fn an_unterminated_string_is_found() {
        let found = check("{\"a\": \"oops}");
        assert_eq!(found.len(), 1);
        assert!(found[0].message.contains("unterminated"), "{}", found[0].message);
    }

    // A structural error's position is only in the parser's message, so it's
    // reported against the buffer, in the parser's words.
    #[test]
    fn a_structural_error_is_reported_once_with_the_parsers_own_words() {
        let found = check("{\"a\": 1,}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].code, "json-parse");
        assert!(found[0].message.contains("position"), "{}", found[0].message);
    }

    #[test]
    fn a_lexical_error_suppresses_the_structural_one() {
        assert!(check("{\"a\": @}").iter().all(|d| d.code == "json-syntax"));
    }

    #[test]
    fn valid_json_and_an_empty_buffer_are_both_quiet() {
        assert!(check("{\"a\": [1, 2, {\"b\": null}]}").is_empty());
        assert!(check("").is_empty());
        assert!(check("   \n  ").is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    fn check(src: &str) -> Vec<Diagnostic> {
        BashLinter.check(src)
    }

    #[test]
    fn unquoted_bare_var_in_argument_position_is_flagged_and_fixed() {
        let diags = check("echo $foo");
        assert_eq!(codes(&diags), ["unquoted-expansion"]);
        let fix = diags[0].fix.as_ref().unwrap();
        assert_eq!(&"echo $foo"[fix.start..fix.end], "$foo");
        assert_eq!(fix.replacement, "\"$foo\"");
    }

    #[test]
    fn unquoted_braced_var_fix_includes_the_braces() {
        let diags = check("echo ${foo}");
        let fix = diags[0].fix.as_ref().unwrap();
        assert_eq!(&"echo ${foo}"[fix.start..fix.end], "${foo}");
        assert_eq!(fix.replacement, "\"${foo}\"");
    }

    #[test]
    fn unquoted_var_expand_fix_includes_the_whole_operator_expression() {
        let diags = check("echo ${foo:-bar}");
        let fix = diags[0].fix.as_ref().unwrap();
        assert_eq!(fix.replacement, "\"${foo:-bar}\"");
    }

    #[test]
    fn unquoted_command_substitution_is_flagged_and_recursed_into() {
        let diags = check("echo $(cat $x)");
        assert_eq!(codes(&diags), ["unquoted-expansion", "unquoted-expansion"]);
        assert_eq!(diags[0].fix.as_ref().unwrap().replacement, "\"$(cat $x)\"");
        assert_eq!(diags[1].fix.as_ref().unwrap().replacement, "\"$x\"");
    }

    #[test]
    fn unquoted_backtick_substitution_fix_includes_the_backticks() {
        let diags = check("echo `cat foo`");
        assert_eq!(diags[0].fix.as_ref().unwrap().replacement, "\"`cat foo`\"");
    }

    #[test]
    fn already_quoted_expansion_is_not_flagged() {
        assert!(check("echo \"$foo\"").is_empty());
    }

    #[test]
    fn plain_word_command_name_is_not_flagged() {
        assert!(check("echo hello world").is_empty());
    }

    #[test]
    fn unquoted_var_in_bracket2_test_is_exempt() {
        assert!(check("[[ $foo == bar ]]").is_empty());
    }

    #[test]
    fn unquoted_var_in_single_bracket_test_is_still_flagged() {
        // `[` is a command, not syntax, so an empty `$foo` gives `[: unary
        // operator expected`.
        let diags = check("[ $foo = bar ]");
        assert_eq!(codes(&diags), ["unquoted-expansion"]);
    }

    #[test]
    fn unquoted_var_in_case_subject_is_exempt() {
        assert!(check("case $foo in a) ;; esac").is_empty());
    }

    #[test]
    fn unquoted_var_in_case_pattern_is_still_flagged() {
        let diags = check("case x in $foo) ;; esac");
        assert_eq!(codes(&diags), ["unquoted-expansion"]);
    }

    #[test]
    fn assignment_rhs_is_exempt_regardless_of_position() {
        assert!(check("x=$foo").is_empty());
        assert!(check("FOO=$bar cmd").is_empty());
    }

    #[test]
    fn equals_shaped_word_as_an_ordinary_argument_is_still_flagged() {
        // `make VAR=$val`: a non-leading word is never an assignment, so
        // `$val` splits, unlike in `x=$foo`.
        let diags = check("make VAR=$val");
        assert_eq!(codes(&diags), ["unquoted-expansion"]);
    }

    #[test]
    fn special_single_char_parameters_are_never_flagged() {
        assert!(check("echo $? $$ $! $# $-").is_empty());
    }

    #[test]
    fn positional_and_at_and_array_length_and_arith_are_not_special_cased() {
        assert_eq!(check("echo $1").len(), 1);
        assert_eq!(check("echo $@").len(), 1);
        assert!(check("echo ${#arr[@]}").is_empty());
        assert!(check("echo $((1 + 2))").is_empty());
    }

    #[test]
    fn masked_return_value_bare_local_is_flagged_with_a_fix() {
        let diags = check("local x=$(cmd)");
        assert_eq!(codes(&diags), ["masked-return-value"]);
        let fix = diags[0].fix.as_ref().unwrap();
        assert_eq!(fix.replacement, "x\nx=$(cmd)");
        assert_eq!(&"local x=$(cmd)"[fix.start..fix.end], "x=$(cmd)");
    }

    #[test]
    fn masked_return_value_declare_export_typeset_are_all_recognized() {
        for kw in ["declare", "export", "typeset"] {
            let diags = check(&format!("{kw} x=$(cmd)"));
            assert_eq!(codes(&diags), ["masked-return-value"], "keyword {kw}");
            assert!(diags[0].fix.is_some(), "keyword {kw} should be fixable");
        }
    }

    #[test]
    fn masked_return_value_preserves_line_indentation_in_its_fix() {
        let src = "f() {\n    local x=$(cmd)\n}";
        let diags = check(src);
        let fix = diags[0].fix.as_ref().unwrap();
        assert_eq!(fix.replacement, "x\n    x=$(cmd)");
    }

    #[test]
    fn masked_return_value_readonly_is_flagged_but_never_fixed() {
        let diags = check("readonly x=$(cmd)");
        assert_eq!(codes(&diags), ["masked-return-value"]);
        assert!(diags[0].fix.is_none());
    }

    #[test]
    fn masked_return_value_with_a_flag_is_flagged_but_not_fixed() {
        let diags = check("local -r x=$(cmd)");
        assert_eq!(codes(&diags), ["masked-return-value"]);
        assert!(diags[0].fix.is_none());
    }

    #[test]
    fn masked_return_value_with_a_sibling_assignment_is_flagged_but_not_fixed() {
        let diags = check("local x=$(cmd) y=2");
        assert_eq!(codes(&diags), ["masked-return-value"]);
        assert!(diags[0].fix.is_none());
    }

    #[test]
    fn masked_return_value_plain_value_is_not_flagged() {
        assert!(check("local x=plain").is_empty());
        assert!(check("local x=$plain_var").is_empty());
    }

    #[test]
    fn masked_return_value_only_applies_to_declare_keyword_commands() {
        assert!(check("x=$(cmd)").is_empty());
    }
}
