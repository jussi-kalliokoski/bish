// A linter for bash scripts, built the same way highlight.rs is: on top
// of the real lexer (lexer::tokenize_spanned) rather than a parallel
// implementation or the parser's own AST. The AST (parser.rs's Program/
// Command/Word/...) carries no source spans at all -- threading them
// through every variant (including the lazily-reparsed Subshell/Arith
// raw-text ones) would be a much larger, more invasive change than this
// feature needs. tokenize_spanned already gives a flat, char-offset-
// spanned token/chunk stream (the same one highlight.rs already walks),
// which is enough structure for every rule below: command-name/argument
// position (mirroring highlight.rs's own CmdPos/WordRole), `[[ ... ]]`
// nesting, and each word's own Chunk breakdown (already quote-aware --
// Chunk::Var/VarExpand/etc all carry their own `quoted` flag from the
// lexer, so nothing here has to re-derive that).
//
// `Linter` is a small trait for the same reason `Highlighter` is one:
// `BashLinter` is the only implementor today, but this is meant to be
// the one shared core behind `bish tool check` (tool.rs), and later an
// in-editor "show me the squiggles" feature and `bish tool lsp-server`
// -- all three just want "text in, Diagnostics out," nothing CLI- or
// editor-specific baked in here. `Fix` spans are char offsets into the
// same text, so a caller can apply one with a plain slice-and-splice
// (see tool.rs's own `apply_fixes`), no separate coordinate system to
// convert.
//
// Scope, deliberately: only rules that catch a real behavioral bug or
// risk (unquoted expansions that can word-split/glob, a command
// substitution's exit status getting masked by `local`/`declare`/...) --
// nothing about formatting/style (indentation, brace placement, `[` vs
// `[[`, backticks vs `$()`, ...), which is `bish tool format`'s future
// job, not this one.

use crate::bishedit::highlight::resets_command_position;
use crate::lexer::{self, Chunk, SpannedItem, Tok};
use crate::parser;
use std::borrow::Cow;
use std::ops::Range;

// Declared least-severe-first, and `Ord` is derived for exactly that
// reason: a gutter cell has room for one mark but a line can hold
// several findings, so "which mark" is `.map(|d| d.severity).max()`.
// Nothing here sorts diagnostics for *display* by severity (they stay
// in source order, which is what a list keyed by line wants), so this
// ordering has no other reader to disagree with.
//
// The four are LSP's four, in LSP's own meaning -- bish's own linter
// only ever emits `Warning` (its scope is deliberately "a real
// behavioral bug or risk," see this module's header), but a language
// server speaks all four and its findings land in the same
// `TextBuffer::diagnostics` these are rendered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Hint,
    Info,
    Warning,
    Error,
}

// A single, self-contained text edit: replace `text[start..end]` (char
// offsets, same convention as `Diagnostic`'s own span and highlight.rs's
// `HighlightSpan`) with `replacement`. Deliberately just one range, not a
// list -- a rule that needs to touch more than one place to fix an issue
// (masked-return-value's own two-statement rewrite) still expresses that
// as a single replacement spanning everything in between, rather than a
// multi-edit patch; see `apply_fixes` in tool.rs for why that keeps
// applying a batch of fixes from many diagnostics trivial (sort by
// descending `start`, splice each in turn -- no dependency tracking
// between edits needed).
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
    // A short, stable, kebab-case identifier for the rule that produced
    // this -- what a future suppression comment would key off of, not
    // meant to change once a rule ships.
    //
    // `Cow` rather than the `&'static str` this was, because a language
    // server's diagnostic code is a runtime string off the wire and
    // these share one `Vec` with bish's own findings. Every rule in this
    // file still writes a literal and still allocates nothing for it;
    // only the borrowed-or-owned choice moved into the type.
    pub code: Cow<'static, str>,
    // Who found it -- `None` for bish's own linters (the overwhelming
    // majority, and what a reader assumes when nothing says otherwise),
    // `Some("rustc")` for a finding relayed from a language server. Kept
    // separate from `code` rather than folded into it because the two
    // namespaces are genuinely different: two servers can both emit an
    // `E0308` and mean different things.
    pub source: Option<String>,
    pub message: String,
    // `None` when the finding is real but there's no fix this engine
    // trusts itself to make automatically (see masked_return_value's own
    // doc comment for a concrete case: a fix is only offered when
    // rewriting it can't change the script's behavior any other way).
    pub fix: Option<Fix>,
}

impl Diagnostic {
    /// How this finding identifies itself in a list: its bare `code`
    /// when bish found it, `source:code` when something else did. The
    /// one place that formatting is decided, since both the CLI
    /// (`bish tool check`) and the editor's diagnostics pane show it.
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

/// JSON and JSONC, from the tokenizer bish already has.
///
/// Deliberately *lexical* rather than structural: `json::tokens` marks
/// each bad piece with the exact offset it starts at, which is what a
/// gutter marker and an underline need. `json::parse`'s own structural
/// errors carry their position inside a message string rather than as
/// data, so a mismatched brace is reported once, against the whole
/// buffer, with the parser's own words -- which name the position even
/// when this cannot point at it.
///
/// That covers what actually goes wrong in a hand-edited JSON file: an
/// unterminated string, a bad escape, a stray word, a mismatched brace.
///
/// Plain `json` only, deliberately. **JSONC** is a highlighting language
/// here and nothing more -- there is no JSONC parser, and the JSON
/// tokenizer does not know a `//` comment from a syntax error, so
/// linting one would light up every comment in a `tsconfig.json` with a
/// finding that is wrong. **TOML** gets nothing for the same shape of
/// reason: bish has a TOML *tokenizer* and no parser, so there is no
/// failure to report. Neither is an oversight; both want a parser first.
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
        // Only when nothing lexical was found: a stray character already
        // reported once above is what *caused* the parse to fail, and
        // saying so twice helps nobody.
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

/// TOML, from the parser in `toml.rs`.
///
/// Unlike JSON's, this one points at the mistake: `toml::ParseError`
/// carries a char offset as *data* rather than baking a position into
/// its message, so the gutter marker and the underline land on the
/// thing that is wrong rather than on the whole buffer.
///
/// One finding, not a list: the parser stops at the first thing it
/// cannot read, and everything after that is a guess about a document
/// whose shape is already unknown.
pub struct TomlLinter;

impl Linter for TomlLinter {
    fn check(&self, text: &str) -> Vec<Diagnostic> {
        let Err(e) = crate::toml::parse(text) else { return Vec::new() };
        let len = text.chars().count();
        let start = e.at.min(len.saturating_sub(1));
        vec![Diagnostic {
            start,
            // Through to the end of the offending line, so there is
            // something to underline even where the parser can only
            // name a single position.
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

// Consumes the next raw_capture_spans entry, if any -- see highlight.rs's
// own `next_span` (identical shape/reasoning: `.get()` rather than direct
// indexing, since a lex error partway through can leave the token stream
// and this list out of sync; see raw_capture_spans's own doc comment in
// lexer.rs).
fn next_span(raw_spans: &[Range<usize>], cursor: &mut usize) -> Option<Range<usize>> {
    let span = raw_spans.get(*cursor).cloned();
    if span.is_some() {
        *cursor += 1;
    }
    span
}

// raw_capture_spans's own entry for an expansion chunk covers just its
// *content*, excluding whatever delimiters that particular syntax form
// used ($NAME's bare `$`; ${...}'s `${`/`}`; $(...)'s `$(`/`)`; a
// backtick substitution's opening/closing `` ` ``) -- see lexer.rs's own
// push_var/capture_balanced_parens/capture_backtick doc comments. Fine
// for highlighting (which colors the identifier, not the sigil), but a
// wrap-in-quotes fix needs the *whole* expansion, delimiters included, or
// `"foo"` (quoting just the identifier span) would land as `$"foo"` --
// a real, different piece of bash syntax (a locale-translated string),
// not "$foo" quoted.
//
// Chunk::Var is the only chunk kind that can be either form (`$foo` or
// `${foo}`) -- every other expansion chunk this module cares about always
// used braces (`${...}` syntax is required for an operator/index/`!`) or
// is a substitution (`$(...)`/backtick), so which case applies is read
// directly off the characters just outside `content` in the real source
// text, not guessed from the chunk's own shape. Falls back to `content`
// unchanged if surrounding text doesn't match any known delimiter pair
// (shouldn't happen for anything actually produced by push_var/
// capture_balanced_parens/capture_backtick, but degrading to "the fix
// just doesn't add a sigil" is far better than an incorrect splice).
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

// `NAME=value` / `NAME+=value` -- reuses parser::word_as_assignment
// directly (rather than re-deriving the identifier rule by hand) so this
// always agrees with what the real parser treats as an assignment word.
// Returns the name and how many *chars* of the word's own first chunk
// are the "NAME=" / "NAME+=" prefix -- where the value part starts,
// relative to the word's own span.
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

// Peeks forward from `start` (the index in `items` right after a
// local/declare/export/readonly/typeset command-name token) through this
// one command's own argument words, to decide whether
// masked_return_value's own fix can safely rewrite an assignment
// argument into two statements: only when there's exactly one argument
// word for the whole command (no flags, no siblings) and it's
// assignment-shaped. Anything else -- a flag (`local -r x=$(cmd)`, where
// splitting the assignment out would try to assign to an
// already-readonly name), a second assignment, or a plain positional
// argument -- is left as detect-only (`Diagnostic.fix: None`): there's no
// single-edit rewrite that's still guaranteed to preserve the script's
// behavior once siblings are in play. `readonly` itself is never
// fixable, for the same reason as a `-r` flag: the whole point of
// `readonly` is that a following plain assignment to the same name fails.
//
// Read-only over `items`/`chunks` -- never touches the caller's own
// `cursor`/raw_capture_spans (see lint_into's own doc comment for why
// peeking ahead like this doesn't desync them: this only classifies
// word *shapes*, it never needs a precise span, which is the only thing
// raw_capture_spans/`cursor` are for).
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

// The whitespace-only run right after the most recent newline before
// `pos` (or the start of `text` if there isn't one) -- what
// masked_return_value's own fix reuses as the second statement's own
// leading indentation, so the rewritten `NAME\n<indent>NAME=...` lands at
// the same indentation as the `local`/`declare`/... line it split, rather
// than always column 0.
fn line_indent(chars: &[char], pos: usize) -> String {
    let line_start = chars[..pos].iter().rposition(|&c| c == '\n').map(|i| i + 1).unwrap_or(0);
    let indent_end = chars[line_start..pos].iter().position(|&c| !matches!(c, ' ' | '\t')).map(|i| line_start + i).unwrap_or(pos);
    char_slice(chars, line_start..indent_end)
}

// Which command-name/argument-position state a word is in -- mirrors
// highlight.rs's own WordRole/CmdPos exactly (see that module's own doc
// comments for the full reasoning); duplicated rather than shared
// because the two walkers need different things from it (highlight.rs
// wants it for command-validity/argument styling, this module wants it
// to know which words are exempt from unquoted_expansion and which are a
// masked_return_value candidate) and neither's own shape is likely to
// stay identical as each grows its own rules.
enum WordRole {
    AssignmentPrefix,
    CommandName,
    Argument { command: Option<String>, arg_index: usize },
}

#[derive(Clone)]
enum CmdPos {
    ExpectCommand,
    InCommand { name: Option<String>, arg_index: usize },
}

fn resolve_role(cmd_pos: &mut CmdPos, chunks: &[Chunk]) -> WordRole {
    match cmd_pos {
        CmdPos::ExpectCommand if assignment_prefix(chunks).is_some() => WordRole::AssignmentPrefix,
        CmdPos::ExpectCommand => {
            let name = if let [Chunk::Str(s)] = chunks { Some(s.clone()) } else { None };
            *cmd_pos = CmdPos::InCommand { name, arg_index: 0 };
            WordRole::CommandName
        }
        CmdPos::InCommand { name, arg_index } => {
            let role = WordRole::Argument { command: name.clone(), arg_index: *arg_index };
            *arg_index += 1;
            role
        }
    }
}

// The actual walk -- one flat pass over tokenize_spanned's own item
// stream, recursing into $(...)/backtick/<(...)/>(...) raw text the same
// way highlight.rs's highlight_into does (a fresh CmdPos/bracket depth/
// case-subject state per recursion level, offsets translated back to the
// outermost text's own coordinates). Heredoc bodies are deliberately
// *not* recursed into -- same accepted, pre-existing gap highlight.rs's
// own next_span doc comment documents for raw_capture_spans desyncing
// inside a heredoc that itself contains an expansion; not a new problem
// this module introduces, and not worth inheriting a known-desyncing
// feature to chase.
fn lint_into(text: &str, offset: usize, out: &mut Vec<Diagnostic>) {
    let chars: Vec<char> = text.chars().collect();
    let res = lexer::tokenize_spanned(text);
    let mut cursor = 0usize;
    let mut cmd_pos = CmdPos::ExpectCommand;
    let mut bracket2_depth: u32 = 0;
    // `case WORD in` -- WORD is expansion+quote-removed but never
    // word-split (POSIX), so it's exempt from unquoted_expansion the
    // same way [[ ... ]]'s own contents are. Set right after KwCase,
    // consumed by the very next word (there's exactly one between `case`
    // and `in` in valid syntax); KwIn also clears it defensively in case
    // of malformed/in-progress input.
    let mut case_subject = false;
    // Set fresh whenever a command-name word resolves to one of
    // DECLARE_KEYWORDS -- (keyword, whether declare_fixable said this
    // command's own sole argument can be safely split into two
    // statements). Cleared implicitly the next time a command-name word
    // resolves to anything else, same lifetime as CmdPos::InCommand's
    // own `name`.
    let mut declare_ctx: Option<(String, bool)> = None;

    for i in 0..res.items.len() {
        let SpannedItem::Tok(tok, span) = &res.items[i] else { continue };
        if resets_command_position(tok) {
            cmd_pos = CmdPos::ExpectCommand;
        }
        if matches!(tok, Tok::KwFor | Tok::KwSelect | Tok::KwCase | Tok::KwIn) {
            cmd_pos = CmdPos::InCommand { name: None, arg_index: 0 };
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
                // Matches highlight.rs's own Tok::Arith handling: one
                // flat span, no recursion (different lexical rules than
                // the shell grammar) -- consumed here only to keep
                // `cursor` in lockstep with raw_capture_spans.
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
                            // `op_text` (either "=" or "+=", read
                            // straight from the source rather than
                            // assumed) so an append-assignment isn't
                            // silently rewritten into a plain one.
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

// Every expansion chunk in one word: recurses into $(...)/backtick/
// process-substitution raw text regardless of `exempt` (nested content
// always gets linted -- exempt only ever means "don't flag *this* word's
// own splitting/globbing risk", never "skip its interior"), and flags an
// unquoted, splitting-sensitive one when `!exempt`.
fn lint_word_chunks(
    chunks: &[Chunk],
    offset: usize,
    chars: &[char],
    raw_spans: &[Range<usize>],
    cursor: &mut usize,
    exempt: bool,
    out: &mut Vec<Diagnostic>,
) {
    // An all-plain word ([Chunk::Str(_)]) never got a raw_capture_spans
    // entry at all (see that field's own doc comment) and has nothing an
    // expansion-focused rule could ever flag -- skip it outright rather
    // than falling into the chunk loop below and trying to consume an
    // entry that was never pushed for it.
    if let [Chunk::Str(_)] = chunks {
        return;
    }
    for chunk in chunks {
        match chunk {
            Chunk::Str(_) => {}
            Chunk::LiteralStr(_) => {
                next_span(raw_spans, cursor);
            }
            Chunk::ArrayLength { .. } => {
                // ${#name}/${#arr[@]} -- always a plain non-negative
                // integer, never contains whitespace/glob characters, so
                // (like $#/$?/$$/$!/$- below) quoting it is a style
                // choice, not a correctness one -- out of this rule's
                // "non-cosmetic" scope.
                next_span(raw_spans, cursor);
            }
            Chunk::Var { name, quoted } | Chunk::Indirect { name, quoted } | Chunk::ArrayKeys { name, quoted } => {
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
            Chunk::Sub { raw, quoted } => {
                if let Some(content) = next_span(raw_spans, cursor) {
                    let inner_offset = offset + content.start;
                    if !exempt && !quoted {
                        push_unquoted_expansion(chars, offset, content, out);
                    }
                    lint_into(raw, inner_offset, out);
                }
            }
            Chunk::Arith { .. } => {
                // Always safe unquoted -- an arithmetic result is always
                // a plain integer -- and not recursed into, matching
                // Tok::Arith's own treatment (different lexical rules
                // than the shell grammar).
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
        // On the third line, not against the whole buffer -- which is
        // the difference the parser carrying its position as data makes.
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

    // One finding, not a list: everything after the first unreadable
    // thing is a guess about a document whose shape is already unknown.
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
        // Pointing at the bad word, not at the start of the file.
        assert_eq!(&text[found[0].start..found[0].end], "tru");
    }

    #[test]
    fn an_unterminated_string_is_found() {
        let found = check("{\"a\": \"oops}");
        assert_eq!(found.len(), 1);
        assert!(found[0].message.contains("unterminated"), "{}", found[0].message);
    }

    // A structural error has its position inside the parser's own
    // message rather than as data, so it is reported once against the
    // buffer -- with the parser's words, which do name the position.
    #[test]
    fn a_structural_error_is_reported_once_with_the_parsers_own_words() {
        let found = check("{\"a\": 1,}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].code, "json-parse");
        assert!(found[0].message.contains("position"), "{}", found[0].message);
    }

    // A stray character is what *caused* the parse to fail; saying so
    // twice helps nobody.
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
        // `[` is an ordinary command (test), not special syntax -- word
        // splitting inside it is exactly the classic `[: unary operator
        // expected` footgun when the variable is empty.
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
        // `make VAR=$val` -- bash never treats a non-leading, non-
        // declare-argument word as a real assignment, so $val here is an
        // ordinary word-splitting-sensitive expansion, unlike x=$foo.
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
