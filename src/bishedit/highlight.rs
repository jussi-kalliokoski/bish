// Syntax highlighting, built on top of the real lexer rather than a
// parallel implementation (see plan.md / the approved design doc for this
// feature) -- Highlighter is a small trait so a regex-, treesitter-, or
// LSP-backed source can slot in later; BashHighlighter is the only
// implementor today, driven by lexer::tokenize_spanned. Wired into
// editor.rs's redraw() (and, since that function is shared, command
// mode's colon-line and Ctrl-E's line-local normal-mode view get it for
// free too).
//
// #![allow(dead_code)] stays regardless of wiring -- HighlightKind::Number
// is reserved but intentionally unpopulated (see its own doc comment),
// and the Highlighter trait exists for future non-Bash implementors this
// crate doesn't have yet.
#![allow(dead_code)]

use crate::bishedit::manpages;
use crate::lexer::{self, Chunk, SpannedItem, Tok};
use crate::vt100;
use std::ops::Range;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightKind {
    Keyword,
    Operator,
    Redirect,
    String,
    Variable,
    Substitution,
    Comment,
    // Reserved, unpopulated in v1 -- would need its own small scanner over
    // $((...))'s interior (digit runs, $var refs), which has different
    // lexical rules than the shell grammar and isn't attempted here.
    Number,
    // A plain unquoted argument recognized as one of its command's own
    // flags (from a man-page-mined list, see manpages.rs) -- any argument
    // position, exact string match only.
    Flag,
    // A plain unquoted argument recognized as the subcommand immediately
    // following its command name (single level only, e.g. "commit" in
    // "git commit").
    Subcommand,
    // A plain unquoted argument that isn't a recognized Flag/Subcommand
    // but does resolve to a real file/directory against the shell's cwd.
    Link,
    // A refinement *within* a builtin's own argument text (e.g. printf's
    // "%s") -- narrower than, and layered on top of, that argument's own
    // base span (String, typically).
    FormatSpecifier,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub kind: HighlightKind,
    // The file:// URL for a Link-kind span; None for every other kind.
    // Not used for anything yet (no OSC 8 terminal hyperlinks) -- carried
    // purely as data for a future consumer.
    pub link: Option<String>,
}

pub trait Highlighter {
    fn highlight(&self, text: &str, cwd: Option<&Path>) -> Vec<HighlightSpan>;
}

pub struct BashHighlighter;

impl Highlighter for BashHighlighter {
    fn highlight(&self, text: &str, cwd: Option<&Path>) -> Vec<HighlightSpan> {
        let mut out = Vec::new();
        highlight_into(text, 0, cwd, &mut out);
        out
    }
}

// A resolved (start, end, color, attrs) span -- the presentation-layer
// sibling of HighlightSpan, once a HighlightKind has been mapped to an
// actual color. Kept as its own type (rather than just carrying
// HighlightKind through to compose) because it's the seam future
// presentation features (selections, search highlights, inline coverage,
// diffs, completions) plug into: any of those just needs to build its own
// Vec<StyledSpan> and hand it to compose as one more layer, with no
// dependency on HighlightKind or the highlighter at all.
#[derive(Debug, Clone, PartialEq)]
pub struct StyledSpan {
    pub start: usize,
    pub end: usize,
    pub fg: vt100::Color,
    pub attrs: vt100::CellAttrs,
}

// Indexed(0-15), matching prompt.rs's own existing bold+low-8-ANSI
// convention, not Rgb -- there's no light/dark-aware theme system yet to
// make a fixed RGB choice safe.
pub fn default_style(kind: HighlightKind) -> (vt100::Color, vt100::CellAttrs) {
    let bold = vt100::CellAttrs { bold: true, ..vt100::CellAttrs::default() };
    let dim = vt100::CellAttrs { dim: true, ..vt100::CellAttrs::default() };
    let underline = vt100::CellAttrs { underline: true, ..vt100::CellAttrs::default() };
    match kind {
        HighlightKind::Keyword => (vt100::Color::Indexed(3), bold),
        HighlightKind::String => (vt100::Color::Indexed(2), vt100::CellAttrs::default()),
        HighlightKind::Variable => (vt100::Color::Indexed(6), bold),
        HighlightKind::Substitution => (vt100::Color::Indexed(4), bold),
        HighlightKind::Redirect => (vt100::Color::Indexed(5), bold),
        HighlightKind::Operator => (vt100::Color::Indexed(7), vt100::CellAttrs::default()),
        HighlightKind::Comment => (vt100::Color::Indexed(8), dim),
        HighlightKind::Number => (vt100::Color::Indexed(6), vt100::CellAttrs::default()),
        // "Bold for now" per the feature request -- no new color, just
        // weight, so a flag/subcommand match doesn't fight for attention
        // with the actual grammar-level colors above.
        HighlightKind::Flag => (vt100::Color::Default, bold),
        HighlightKind::Subcommand => (vt100::Color::Default, bold),
        HighlightKind::Link => (vt100::Color::Default, underline),
        HighlightKind::FormatSpecifier => (vt100::Color::Indexed(1), bold),
    }
}

// Builds one Cell per char, then paints each layer's spans over it in
// order -- a later layer (or a later span within the same layer, though
// BashHighlighter's own output is always non-overlapping by construction)
// always wins for any char it covers. This is deliberately a plain
// function, not a trait/registry: it has exactly one caller (redraw(), in
// a later stage), which already knows its own layer set at each call
// site -- adding a selection/search-match/diagnostic layer later is "pass
// one more slice," not an interface change.
pub fn compose(chars: &[char], layers: &[&[StyledSpan]]) -> Vec<vt100::Cell> {
    let mut cells: Vec<vt100::Cell> = chars.iter().map(|&ch| vt100::Cell { ch, ..vt100::Cell::default() }).collect();
    for layer in layers {
        for span in layer.iter() {
            let end = span.end.min(cells.len());
            for cell in cells.iter_mut().take(end).skip(span.start) {
                cell.fg = span.fg;
                cell.attrs = span.attrs;
            }
        }
    }
    cells
}

// Turns a resolved cell sequence back into an SGR-coded string, reusing
// vt100::sgr_codes -- the same run-coalescing step repl.rs's render_row
// already does for a live pane's grid, just fed synthesized cells instead
// of ones read off a Screen.
pub fn render_styled(cells: &[vt100::Cell]) -> String {
    let mut out = String::new();
    let mut last: Option<(vt100::Color, vt100::Color, vt100::CellAttrs)> = None;
    for cell in cells {
        let key = (cell.fg, cell.bg, cell.attrs);
        if last != Some(key) {
            out.push_str(&vt100::sgr_codes(cell.fg, cell.bg, cell.attrs));
            last = Some(key);
        }
        out.push(cell.ch);
    }
    out.push_str("\x1b[0m");
    out
}

// Consumes the next raw_capture_spans entry, if any. `.get()` rather than
// direct indexing: raw_capture_spans and the token stream can only desync
// if a heredoc body (whose $VAR/$(...) expansions push spans through
// push_var same as anything else, but whose *source positions* aren't
// reliably tracked -- see raw_capture_spans's own doc comment in
// lexer.rs) sits earlier in the same `text`. That needs an embedded
// newline to reach a non-empty body at all, which never happens for the
// single-line buffer this is actually called on -- but staying
// panic-safe here costs nothing and turns an unreachable-in-practice edge
// case into "a few spans come out wrong" instead of a crash.
fn next_span(raw_spans: &[Range<usize>], cursor: &mut usize) -> Option<Range<usize>> {
    let r = raw_spans.get(*cursor).cloned();
    *cursor += 1;
    r
}

// Re-lexes `text` (a full command line, or -- recursively -- the raw
// interior of a $(...) /`...`/<(...) />(...) ) and appends every span it
// finds to `out`, shifted by `offset` so nested spans land in the outer
// caller's coordinate space. `chars` is rebuilt fresh at each recursion
// level from that level's own `text` (not threaded from the caller) --
// raw_capture_spans positions are always relative to whatever text they
// were captured from, so this stays a purely local computation.
fn highlight_into(text: &str, offset: usize, cwd: Option<&Path>, out: &mut Vec<HighlightSpan>) {
    let chars: Vec<char> = text.chars().collect();
    let res = lexer::tokenize_spanned(text);
    let mut cursor = 0usize;
    // Fresh per call -- every recursion level ($(...), backtick,
    // <(...)/>(...), a (...) subshell) gets its own independent command
    // context, never inherited from the enclosing command (e.g. `git
    // commit $(echo -m)` starts a wholly new command inside the
    // substitution).
    let mut cmd_pos = CmdPos::ExpectCommand;
    for item in &res.items {
        match item {
            SpannedItem::Comment(r) => {
                out.push(HighlightSpan { start: offset + r.start, end: offset + r.end, kind: HighlightKind::Comment, link: None });
            }
            SpannedItem::Tok(tok, span) => {
                highlight_tok(tok, span, offset, &chars, &res.raw_capture_spans, &mut cursor, cwd, &mut cmd_pos, out);
            }
        }
    }
}

// Which command-name/argument-position state a word is in, resolved from
// `CmdPos` right before it's processed.
enum WordRole {
    // A leading NAME=value/NAME+=value word before the real command name
    // -- doesn't itself get classified as a command name or argument.
    AssignmentPrefix,
    CommandName,
    // `command`: the resolved command name, if the command-name word was
    // itself a plain [Chunk::Str(_)] (None for e.g. `$CMD arg` -- no
    // static name to look anything up against). `arg_index` is 0-based,
    // counting only real argument words (not the command name itself).
    Argument { command: Option<String>, arg_index: usize },
}

#[derive(Clone)]
enum CmdPos {
    ExpectCommand,
    InCommand { name: Option<String>, arg_index: usize },
}

// Whether encountering this token means "a new simple command can start
// right after this" -- i.e. resets CmdPos back to ExpectCommand. Closing
// tokens (RBrace, RParen, KwFi, KwDone, KwLBracket2, KwRBracket2) do NOT
// reset -- they end a group/test-expression, not start a new command list
// on their own; a new command still needs its own `;`/`&&`/newline/etc
// after them first. Factored out as its own pure function (rather than
// splitting highlight_tok's existing per-kind match arms) so the reset
// rule itself is directly unit-testable without needing to drive the
// whole highlighter.
fn resets_command_position(tok: &Tok) -> bool {
    matches!(
        tok,
        Tok::Pipe
            | Tok::And
            | Tok::Or
            | Tok::Semi
            | Tok::DSemi
            | Tok::SemiAmp
            | Tok::DSemiAmp
            | Tok::Amp
            | Tok::LBrace
            | Tok::Newline
            | Tok::KwIf
            | Tok::KwThen
            | Tok::KwElif
            | Tok::KwElse
            | Tok::KwDo
            | Tok::KwWhile
            | Tok::KwUntil
            | Tok::KwFor
            | Tok::KwSelect
            | Tok::KwCoproc
            | Tok::KwIn
            | Tok::KwCase
            | Tok::KwEsac
            | Tok::KwFunction
    )
}

fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// `NAME=value` / `NAME+=value` shape -- a hand-duplicated check (rather
// than reusing parser.rs's own word_as_assignment) since that operates on
// a fully-parsed Word/Chunk shape from a different code path; this stays
// consistent with this crate's existing precedent of a small duplicated
// helper over coupling the editor-analysis path to the execution path
// (tokenize_spanned's own relationship to tokenize() is the same idea).
fn is_assignment_prefix_word(chunks: &[Chunk]) -> bool {
    let [Chunk::Str(s)] = chunks else { return false };
    let Some(eq) = s.find('=') else { return false };
    let name = s[..eq].strip_suffix('+').unwrap_or(&s[..eq]);
    is_valid_ident(name)
}

// Thin wrapper: resolves `command`'s man-page data (if ready yet -- see
// manpages::query's own doc comment on why this never blocks) and
// delegates the actual matching to classify_plain_argument_core, which
// takes the data directly rather than going through the real cache/thread
// -- that's the seam this module's own tests use to stay deterministic
// without spawning `man`.
fn classify_plain_argument(
    text: &str,
    word_span: &Range<usize>,
    command: &str,
    arg_index: usize,
    cwd: Option<&Path>,
    offset: usize,
) -> Option<HighlightSpan> {
    let man = match manpages::query(command) {
        manpages::ManStatus::Ready(data) => Some(data),
        manpages::ManStatus::Pending | manpages::ManStatus::Missing => None,
    };
    classify_plain_argument_core(text, word_span, arg_index, man.as_deref(), cwd, offset)
}

// Priority order, matching "if the unquoted argument is not a flag or a
// sub-command, we'll check if it's a file": a `-`-prefixed word is
// checked ONLY against flags (any argument position) and never falls
// through to subcommand/file-link matching, even on a miss -- a
// flag-shaped argument is never meaningfully a file path either.
// Subcommand matching is gated to arg_index == 0 (the word immediately
// after the command name) -- single-level only, per this feature's v1
// scope. The file/dir Link fallback lands in a later stage; for now a
// non-flag, non-subcommand argument just stays uncolored, same as today.
fn classify_plain_argument_core(
    text: &str,
    word_span: &Range<usize>,
    arg_index: usize,
    man: Option<&manpages::ManPageData>,
    cwd: Option<&Path>,
    offset: usize,
) -> Option<HighlightSpan> {
    let out_span = |kind: HighlightKind, link: Option<String>| HighlightSpan {
        start: offset + word_span.start,
        end: offset + word_span.end,
        kind,
        link,
    };

    if text.starts_with('-') {
        let flag_text = strip_flag_suffix(text);
        if let Some(man) = man {
            if man.flags.iter().any(|f| f == flag_text) {
                return Some(out_span(HighlightKind::Flag, None));
            }
        }
        return None;
    }

    if arg_index == 0 {
        if let Some(man) = man {
            if man.subcommands.iter().any(|s| s == text) {
                return Some(out_span(HighlightKind::Subcommand, None));
            }
        }
    }

    resolve_file_link(text, cwd).map(|url| out_span(HighlightKind::Link, Some(url)))
}

// Resolves `text` (a fully-plain, unquoted argument -- never `~`-prefixed:
// the lexer always rewrites a leading `~` into a Chunk::Var, so such a
// word never reaches this function as a single Chunk::Str) against `cwd`
// if relative, and checks whether it's a real file/directory. `cwd` being
// None (command mode's own colon-line, see its call site's own doc
// comment) means "don't know, don't guess" -- skip Link detection
// entirely rather than resolving against this process's own cwd, which
// can legitimately differ from the session's Shell.cwd.
//
// A fresh Path::exists() syscall every call, not cached -- filesystem
// state can change moment to moment, unlike man-page content.
fn resolve_file_link(text: &str, cwd: Option<&Path>) -> Option<String> {
    let cwd = cwd?;
    let candidate = if Path::new(text).is_absolute() { std::path::PathBuf::from(text) } else { cwd.join(text) };
    if candidate.exists() {
        Some(format!("file://{}", candidate.display()))
    } else {
        None
    }
}

// Strips a trailing "=value" or " <placeholder>" suffix from a `-`-
// prefixed argument, per this feature's v1 exact-match scope (no bundled
// short-flag decomposition, e.g. "-la" is never split into "-l"/"-a").
// Only ever called after the caller has already confirmed `text` starts
// with '-'.
fn strip_flag_suffix(text: &str) -> &str {
    let end = text.find(|c: char| c == '=' || c == ' ').unwrap_or(text.len());
    &text[..end]
}

// One match arm today (printf) -- a plain function rather than a trait/
// registry, since the extensibility axis here is "add a match arm," which
// this handles cleanly with no dynamic registration/discovery needed.
// Revisit only if a second, structurally different consumer of refiners
// shows up.
fn builtin_refine(command: &str, arg_index: usize, raw_text: &[char]) -> Vec<(Range<usize>, HighlightKind)> {
    match command {
        // printf's format string is its first argument in the common
        // case -- `printf -v NAME FORMAT ...` shifts it to arg_index 2,
        // which this doesn't detect (a documented v1 gap: degrades to no
        // highlighting for that call shape, never a wrong one, since
        // arg_index 0 there is just "-v" with no '%' chars to match).
        "printf" if arg_index == 0 => scan_printf_directives(raw_text),
        _ => Vec::new(),
    }
}

// Mirrors exec.rs's own printf_format_once grammar exactly, not full C
// printf: flags are only '-' and '0' (no '+', space, '#'); width is a
// digit run; precision is '.' plus a digit run; conversion is exactly one
// of s b c q d i u o x X, or a literal "%%". An unrecognized conversion
// char is skipped without emitting a span (matches printf_format_once's
// own "emitted literally, nothing consumed" behavior) -- `i` has already
// advanced past the '%' by then, so this can't loop forever.
fn scan_printf_directives(text: &[char]) -> Vec<(Range<usize>, HighlightKind)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text[i] != '%' {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        if i < text.len() && text[i] == '%' {
            i += 1;
            out.push((start..i, HighlightKind::FormatSpecifier));
            continue;
        }
        while i < text.len() && matches!(text[i], '-' | '0') {
            i += 1;
        }
        while i < text.len() && text[i].is_ascii_digit() {
            i += 1;
        }
        if i < text.len() && text[i] == '.' {
            i += 1;
            while i < text.len() && text[i].is_ascii_digit() {
                i += 1;
            }
        }
        if i < text.len() && matches!(text[i], 's' | 'b' | 'c' | 'q' | 'd' | 'i' | 'u' | 'o' | 'x' | 'X') {
            i += 1;
            out.push((start..i, HighlightKind::FormatSpecifier));
        }
    }
    out
}

fn highlight_tok(
    tok: &Tok,
    span: &Range<usize>,
    offset: usize,
    chars: &[char],
    raw_spans: &[Range<usize>],
    cursor: &mut usize,
    cwd: Option<&Path>,
    cmd_pos: &mut CmdPos,
    out: &mut Vec<HighlightSpan>,
) {
    if resets_command_position(tok) {
        *cmd_pos = CmdPos::ExpectCommand;
    }
    let whole = |kind: HighlightKind| HighlightSpan { start: offset + span.start, end: offset + span.end, kind, link: None };
    match tok {
        Tok::KwIf
        | Tok::KwThen
        | Tok::KwElif
        | Tok::KwElse
        | Tok::KwFi
        | Tok::KwWhile
        | Tok::KwUntil
        | Tok::KwDo
        | Tok::KwDone
        | Tok::KwFor
        | Tok::KwSelect
        | Tok::KwCoproc
        | Tok::KwIn
        | Tok::KwCase
        | Tok::KwEsac
        | Tok::KwFunction
        | Tok::KwLBracket2
        | Tok::KwRBracket2 => out.push(whole(HighlightKind::Keyword)),

        Tok::Pipe
        | Tok::And
        | Tok::Or
        | Tok::Semi
        | Tok::DSemi
        | Tok::SemiAmp
        | Tok::DSemiAmp
        | Tok::Amp
        | Tok::LBrace
        | Tok::RBrace
        | Tok::RParen => out.push(whole(HighlightKind::Operator)),

        Tok::RedirOut { .. }
        | Tok::RedirIn
        | Tok::RedirErr { .. }
        | Tok::RedirBoth { .. }
        | Tok::DupErrToOut
        | Tok::RedirFdOut { .. }
        | Tok::RedirFdIn { .. }
        | Tok::RedirFdDup { .. }
        | Tok::RedirDupWord { .. }
        | Tok::RedirFdClose { .. }
        | Tok::HereString
        | Tok::HereDoc(_) => out.push(whole(HighlightKind::Redirect)),

        Tok::Newline => {}

        // A (...) subshell command grouping -- the parens themselves stay
        // uncolored (unlike Chunk::Sub below, which does mark its
        // delimiters); only the interior recursively highlights.
        Tok::Subshell(raw) => {
            if let Some(inner) = next_span(raw_spans, cursor) {
                highlight_into(raw, offset + inner.start, cwd, out);
            }
        }

        // A bare ((...)) arithmetic command -- one flat Substitution span
        // covering the whole thing (this token's own span already
        // includes both paren pairs), no recursion: arithmetic has
        // different lexical rules than the shell grammar (`<`/`>` are
        // comparisons here, not redirects).
        Tok::Arith(_raw) => {
            next_span(raw_spans, cursor); // capture_double_paren's own capture_balanced_parens push
            out.push(whole(HighlightKind::Substitution));
        }

        Tok::Word(chunks, _plain) => highlight_word(chunks, span, offset, chars, raw_spans, cursor, cwd, cmd_pos, out),
    }
}

// An all-plain word ([Chunk::Str(_)], the common unquoted case with no
// quoting/escaping/expansion at all) is checked against classify_plain_
// argument (flags/subcommands/file-links -- a later stage) instead of
// just staying uncolored; otherwise each chunk is walked in order,
// consuming one raw_capture_spans entry per non-Str chunk (see that
// field's own doc comment in lexer.rs for the exact invariant this relies
// on), with LiteralStr chunks additionally offered to builtin_refine
// (printf's %s/%d/etc -- also a later stage).
//
// `cmd_pos` is resolved into this word's own WordRole *before* the
// all-plain fast path, since both branches need to know it: is this word
// a leading NAME=value assignment (doesn't advance past ExpectCommand),
// the command name itself (advances to InCommand), or the Nth argument
// of an already-known command.
fn highlight_word(
    chunks: &[Chunk],
    word_span: &Range<usize>,
    offset: usize,
    chars: &[char],
    raw_spans: &[Range<usize>],
    cursor: &mut usize,
    cwd: Option<&Path>,
    cmd_pos: &mut CmdPos,
    out: &mut Vec<HighlightSpan>,
) {
    let role = match cmd_pos {
        CmdPos::ExpectCommand if is_assignment_prefix_word(chunks) => WordRole::AssignmentPrefix,
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
    };

    if let [Chunk::Str(s)] = chunks {
        if let WordRole::Argument { command: Some(cmd), arg_index } = &role {
            if let Some(span) = classify_plain_argument(s, word_span, cmd, *arg_index, cwd, offset) {
                out.push(span);
            }
        }
        return;
    }
    for chunk in chunks {
        match chunk {
            Chunk::Str(_) => {}

            Chunk::LiteralStr(_) => {
                if let Some(r) = next_span(raw_spans, cursor) {
                    out.push(HighlightSpan { start: offset + r.start, end: offset + r.end, kind: HighlightKind::String, link: None });
                    if let WordRole::Argument { command: Some(cmd), arg_index } = &role {
                        let raw_slice = &chars[r.start..r.end];
                        for (sub, kind) in builtin_refine(cmd, *arg_index, raw_slice) {
                            out.push(HighlightSpan { start: offset + r.start + sub.start, end: offset + r.start + sub.end, kind, link: None });
                        }
                    }
                }
            }

            // Terminal, no recursion. The span covers just the
            // name/index/op text itself -- not the leading `$` (or the
            // `${`/`}` braces, for the braced forms) -- a deliberate v1
            // simplification: the identifier is colored, the sigil/braces
            // stay neutral, a fairly common convention in editors, and it
            // avoids needing the same delimiter-disambiguation this
            // module does for Chunk::Sub below (bare `$NAME` vs `${NAME}`
            // isn't distinguishable from the chunk alone, only from the
            // source text).
            Chunk::Var { .. }
            | Chunk::VarExpand { .. }
            | Chunk::ArrayVar { .. }
            | Chunk::ArrayLength { .. }
            | Chunk::ArrayVarExpand { .. }
            | Chunk::Indirect { .. }
            | Chunk::ArrayKeys { .. } => {
                if let Some(r) = next_span(raw_spans, cursor) {
                    out.push(HighlightSpan { start: offset + r.start, end: offset + r.end, kind: HighlightKind::Variable, link: None });
                }
            }

            // $(...) command substitution or `...` backtick substitution
            // -- Chunk::Sub doesn't record which delimiter style produced
            // it (one variant covers both), so the char immediately
            // before the captured interior disambiguates: a backtick
            // means a 1-char delimiter on each side, anything else means
            // the grammar's only other route here, "$(", a 2-char/1-char
            // pair.
            Chunk::Sub { raw, .. } => {
                if let Some(r) = next_span(raw_spans, cursor) {
                    let is_backtick = r.start >= 1 && chars.get(r.start - 1) == Some(&'`');
                    let delim_start = if is_backtick { r.start - 1 } else { r.start.saturating_sub(2) };
                    out.push(HighlightSpan { start: offset + delim_start, end: offset + r.start, kind: HighlightKind::Substitution, link: None });
                    out.push(HighlightSpan { start: offset + r.end, end: offset + r.end + 1, kind: HighlightKind::Substitution, link: None });
                    highlight_into(raw, offset + r.start, cwd, out);
                }
            }

            // $((...)) arithmetic expansion within a word -- same flat,
            // non-recursive treatment as the bare ((...)) command above,
            // just needing the surrounding "$((" / "))" computed manually
            // since (unlike Tok::Arith) there's no wrapping token span
            // that already includes them.
            Chunk::Arith { .. } => {
                if let Some(r) = next_span(raw_spans, cursor) {
                    let full_start = r.start.saturating_sub(3);
                    let full_end = r.end + 2;
                    out.push(HighlightSpan { start: offset + full_start, end: offset + full_end, kind: HighlightKind::Substitution, link: None });
                }
            }

            // <(cmd) / >(cmd) process substitution -- same delimiter-plus-
            // recurse treatment as Chunk::Sub, but unambiguous (each
            // variant has exactly one possible delimiter pair), so no
            // peeking needed.
            Chunk::ProcSubIn { raw } => push_procsub(raw, offset, raw_spans, cursor, cwd, out),
            Chunk::ProcSubOut { raw } => push_procsub(raw, offset, raw_spans, cursor, cwd, out),
        }
    }
}

fn push_procsub(raw: &str, offset: usize, raw_spans: &[Range<usize>], cursor: &mut usize, cwd: Option<&Path>, out: &mut Vec<HighlightSpan>) {
    if let Some(r) = next_span(raw_spans, cursor) {
        let delim_start = r.start.saturating_sub(2);
        out.push(HighlightSpan { start: offset + delim_start, end: offset + r.start, kind: HighlightKind::Substitution, link: None });
        out.push(HighlightSpan { start: offset + r.end, end: offset + r.end + 1, kind: HighlightKind::Substitution, link: None });
        highlight_into(raw, offset + r.start, cwd, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(usize, usize, HighlightKind)> {
        let mut spans = BashHighlighter.highlight(text, None);
        spans.sort_by_key(|s| (s.start, s.end));
        spans.into_iter().map(|s| (s.start, s.end, s.kind)).collect()
    }

    #[test]
    fn plain_word_command_has_no_spans() {
        assert_eq!(kinds("ls -la"), vec![]);
    }

    #[test]
    fn keywords_and_operators() {
        let text = "if true; then echo hi; fi";
        let spans = kinds(text);
        // "if" .. "then" .. ";" .. ";" .. "fi"
        assert!(spans.contains(&(0, 2, HighlightKind::Keyword))); // if
        assert!(spans.contains(&(9, 13, HighlightKind::Keyword))); // then
        assert!(spans.contains(&(23, 25, HighlightKind::Keyword))); // fi
        assert!(spans.iter().any(|s| s.2 == HighlightKind::Operator));
    }

    #[test]
    fn redirect_gets_its_own_span() {
        let text = "ls > out.txt";
        let spans = kinds(text);
        assert!(spans.contains(&(3, 4, HighlightKind::Redirect)));
    }

    #[test]
    fn single_quoted_string_excludes_the_quote_marks() {
        let text = "echo 'hello world'";
        let spans = kinds(text);
        assert!(spans.contains(&(6, 17, HighlightKind::String)));
        assert_eq!(&text[6..17], "hello world");
    }

    #[test]
    fn bare_variable_gets_the_name_only() {
        let text = "echo $HOME";
        let spans = kinds(text);
        assert!(spans.contains(&(6, 10, HighlightKind::Variable)));
        assert_eq!(&text[6..10], "HOME");
    }

    #[test]
    fn braced_variable_gets_the_inner_content_only() {
        let text = "echo ${HOME}";
        let spans = kinds(text);
        assert!(spans.contains(&(7, 11, HighlightKind::Variable)));
        assert_eq!(&text[7..11], "HOME");
    }

    #[test]
    fn bare_arithmetic_command_is_one_flat_span() {
        let text = "((1 + 2))";
        let spans = kinds(text);
        assert_eq!(spans, vec![(0, 9, HighlightKind::Substitution)]);
    }

    #[test]
    fn arithmetic_expansion_within_a_word_is_one_flat_span() {
        let text = "echo $((1 + 2))";
        let spans = kinds(text);
        assert!(spans.contains(&(5, 15, HighlightKind::Substitution)));
        assert_eq!(&text[5..15], "$((1 + 2))");
        // no separate spans for anything inside the parens
        assert_eq!(spans.iter().filter(|s| s.0 >= 5 && s.1 <= 15).count(), 1);
    }

    #[test]
    fn subshell_parens_are_uncolored_but_interior_recurses() {
        let text = "(echo 'hi')";
        let spans = kinds(text);
        // No span at all covers either paren (index 0 or 10); the nested
        // single-quoted string recurses correctly, offset back into the
        // outer text's own coordinates.
        assert_eq!(spans, vec![(7, 9, HighlightKind::String)]);
        assert_eq!(&text[7..9], "hi");
    }

    #[test]
    fn dollar_paren_substitution_delimiters_and_interior() {
        let text = "echo $(echo hi)";
        let spans = kinds(text);
        // "$(" at 5..7, ")" at 14..15
        assert!(spans.contains(&(5, 7, HighlightKind::Substitution)));
        assert!(spans.contains(&(14, 15, HighlightKind::Substitution)));
    }

    #[test]
    fn backtick_substitution_delimiters_are_one_char_each() {
        let text = "echo `echo hi`";
        let spans = kinds(text);
        assert!(spans.contains(&(5, 6, HighlightKind::Substitution)));
        assert!(spans.contains(&(13, 14, HighlightKind::Substitution)));
    }

    // The user's own motivating example: the single-quoted string nested
    // inside $(...) must independently highlight as its own String span,
    // not flatten into the outer Substitution color.
    #[test]
    fn nested_single_quote_inside_substitution_recurses() {
        let text = "echo \"yooo, $(printf 'hello %' world)\"";
        let spans = kinds(text);
        let quote_start = text.find("'hello %'").unwrap() + 1;
        let quote_end = quote_start + "hello %".len();
        assert!(
            spans.contains(&(quote_start, quote_end, HighlightKind::String)),
            "expected a String span for the nested quote at {}..{}, got {:?}",
            quote_start,
            quote_end,
            spans
        );
        // The outer "yooo, " literal run also gets its own String span.
        let outer_start = text.find("yooo, ").unwrap();
        assert!(spans.contains(&(outer_start, outer_start + "yooo, ".len(), HighlightKind::String)));
    }

    #[test]
    fn comment_gets_its_own_span() {
        let text = "echo hi # a comment";
        let spans = kinds(text);
        let start = text.find('#').unwrap();
        assert!(spans.contains(&(start, text.len(), HighlightKind::Comment)));
    }

    #[test]
    fn process_substitution_delimiters_and_interior() {
        let text = "diff <(sort a) <(sort b)";
        let spans = kinds(text);
        assert!(spans.contains(&(5, 7, HighlightKind::Substitution))); // "<("
        assert!(spans.contains(&(13, 14, HighlightKind::Substitution))); // ")"
    }

    #[test]
    fn incomplete_line_highlights_up_to_the_error_point() {
        // An unclosed single quote -- tokenize_spanned stops with an
        // error, but "echo" before it must still highlight normally
        // (i.e. produce no spans, being a plain word) rather than the
        // whole line going uncolored.
        let text = "echo 'unterminated";
        let spans = BashHighlighter.highlight(text, None);
        // No panic, and no spans at all is the correct result here since
        // "echo" alone is a plain, uncolored word and the unterminated
        // quote never got far enough to produce a LiteralStr chunk.
        assert_eq!(spans, vec![]);
    }

    #[test]
    fn compose_paints_default_uncolored_cells_for_an_empty_layer_set() {
        let chars: Vec<char> = "abc".chars().collect();
        let cells = compose(&chars, &[]);
        assert_eq!(cells.len(), 3);
        for (cell, expected) in cells.iter().zip("abc".chars()) {
            assert_eq!(cell.ch, expected);
            assert_eq!(cell.fg, vt100::Color::Default);
        }
    }

    #[test]
    fn compose_later_layer_overrides_earlier_layer_for_overlapping_chars() {
        let chars: Vec<char> = "abc".chars().collect();
        let base = [StyledSpan { start: 0, end: 3, fg: vt100::Color::Indexed(2), attrs: vt100::CellAttrs::default() }];
        let overlay = [StyledSpan { start: 1, end: 2, fg: vt100::Color::Indexed(5), attrs: vt100::CellAttrs::default() }];
        let cells = compose(&chars, &[&base, &overlay]);
        assert_eq!(cells[0].fg, vt100::Color::Indexed(2));
        assert_eq!(cells[1].fg, vt100::Color::Indexed(5)); // overlay wins here
        assert_eq!(cells[2].fg, vt100::Color::Indexed(2));
    }

    #[test]
    fn compose_span_end_past_char_count_is_clamped_not_a_panic() {
        let chars: Vec<char> = "ab".chars().collect();
        let layer = [StyledSpan { start: 0, end: 100, fg: vt100::Color::Indexed(1), attrs: vt100::CellAttrs::default() }];
        let cells = compose(&chars, &[&layer]);
        assert_eq!(cells.len(), 2);
        assert!(cells.iter().all(|c| c.fg == vt100::Color::Indexed(1)));
    }

    #[test]
    fn render_styled_coalesces_runs_of_identical_style_into_one_sgr_code() {
        let chars: Vec<char> = "abcd".chars().collect();
        // "ab" plain, "cd" colored -- two distinct runs, so exactly two
        // SGR escapes should appear (one per run), not four (one per
        // char).
        let layer = [StyledSpan { start: 2, end: 4, fg: vt100::Color::Indexed(3), attrs: vt100::CellAttrs::default() }];
        let cells = compose(&chars, &[&layer]);
        let rendered = render_styled(&cells);
        assert_eq!(rendered.matches('\x1b').count(), 3); // 2 style changes + 1 trailing reset
        assert_eq!(rendered, format!("{}ab{}cd\x1b[0m", vt100::sgr_codes(vt100::Color::Default, vt100::Color::Default, vt100::CellAttrs::default()), vt100::sgr_codes(vt100::Color::Indexed(3), vt100::Color::Default, vt100::CellAttrs::default())));
    }

    #[test]
    fn render_styled_uniform_style_emits_a_single_run() {
        let chars: Vec<char> = "abc".chars().collect();
        let layer = [StyledSpan { start: 0, end: 3, fg: vt100::Color::Indexed(2), attrs: vt100::CellAttrs::default() }];
        let cells = compose(&chars, &[&layer]);
        let rendered = render_styled(&cells);
        // One SGR to enter the style, one to reset -- no per-char churn.
        assert_eq!(rendered.matches('\x1b').count(), 2);
    }

    #[test]
    fn default_style_covers_every_highlight_kind_without_panicking() {
        for kind in [
            HighlightKind::Keyword,
            HighlightKind::Operator,
            HighlightKind::Redirect,
            HighlightKind::String,
            HighlightKind::Variable,
            HighlightKind::Substitution,
            HighlightKind::Comment,
            HighlightKind::Number,
            HighlightKind::Flag,
            HighlightKind::Subcommand,
            HighlightKind::Link,
            HighlightKind::FormatSpecifier,
        ] {
            let _ = default_style(kind);
        }
    }

    fn tok(word: &str) -> Tok {
        // Small helper for resets_command_position tests -- lexes just
        // enough to get a real Tok of the right variant without hardcoding
        // every enum's exact field shape by hand.
        match lexer::Lexer::new(word).tokenize() {
            Ok(mut toks) if toks.len() == 1 => toks.pop().unwrap(),
            other => panic!("expected exactly one token from {word:?}, got {other:?}"),
        }
    }

    #[test]
    fn resets_command_position_on_separators_and_command_list_keywords() {
        for src in ["|", "||", "&&", ";", ";;", "&", "{", "if", "then", "do", "while", "for"] {
            assert!(resets_command_position(&tok(src)), "expected {src:?} to reset command position");
        }
        assert!(resets_command_position(&Tok::Newline));
    }

    #[test]
    fn does_not_reset_command_position_on_closing_tokens() {
        for src in ["}", ")", "fi", "done"] {
            assert!(!resets_command_position(&tok(src)), "expected {src:?} to NOT reset command position");
        }
    }

    #[test]
    fn does_not_reset_command_position_on_an_ordinary_word() {
        assert!(!resets_command_position(&tok("echo")));
    }

    #[test]
    fn is_valid_ident_accepts_typical_shell_variable_names() {
        assert!(is_valid_ident("FOO"));
        assert!(is_valid_ident("_foo123"));
        assert!(!is_valid_ident(""));
        assert!(!is_valid_ident("1FOO")); // leading digit
        assert!(!is_valid_ident("FOO-BAR")); // hyphen not allowed
    }

    fn plain_chunks(s: &str) -> Vec<Chunk> {
        vec![Chunk::Str(s.to_string())]
    }

    #[test]
    fn is_assignment_prefix_word_recognizes_name_equals_and_name_plus_equals() {
        assert!(is_assignment_prefix_word(&plain_chunks("FOO=bar")));
        assert!(is_assignment_prefix_word(&plain_chunks("FOO+=bar")));
        assert!(is_assignment_prefix_word(&plain_chunks("FOO=")));
    }

    #[test]
    fn is_assignment_prefix_word_rejects_invalid_shapes() {
        assert!(!is_assignment_prefix_word(&plain_chunks("1FOO=bar"))); // invalid ident
        assert!(!is_assignment_prefix_word(&plain_chunks("echo"))); // no '='
        assert!(!is_assignment_prefix_word(&plain_chunks("=bar"))); // empty name
        // Not a single plain Chunk::Str -- e.g. a word containing an
        // expansion -- never counts as an assignment prefix here.
        assert!(!is_assignment_prefix_word(&[Chunk::Var { name: "FOO".to_string(), quoted: false }]));
    }

    #[test]
    fn command_name_and_argument_positions_produce_no_spurious_spans() {
        // "one"/"two" are never going to match "echo"'s real flags or
        // subcommands (assuming man is even present and this is the
        // first-ever query for "echo" in this test binary run, the real
        // manpages::query call this now makes returns Pending
        // synchronously -- no data yet, no spans -- regardless, since
        // classify_plain_argument only ever emits a span on an *exact*
        // match). This is really exercising the state-machine wiring
        // itself: an assignment prefix, a command name, and two
        // arguments, none of which should produce spurious output.
        assert_eq!(kinds("FOO=bar echo one two"), vec![]);
    }

    fn man(flags: &[&str], subcommands: &[&str]) -> manpages::ManPageData {
        manpages::ManPageData {
            flags: flags.iter().map(|s| s.to_string()).collect(),
            subcommands: subcommands.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn classify_plain_argument_core_matches_an_exact_flag_at_any_arg_index() {
        let data = man(&["-a", "--all"], &[]);
        let span = 5..7;
        let got = classify_plain_argument_core("-a", &span, 3, Some(&data), None, 0);
        assert_eq!(got, Some(HighlightSpan { start: 5, end: 7, kind: HighlightKind::Flag, link: None }));
    }

    #[test]
    fn classify_plain_argument_core_strips_a_value_suffix_before_matching() {
        let data = man(&["--color"], &[]);
        let span = 0..13;
        let got = classify_plain_argument_core("--color=auto", &span, 1, Some(&data), None, 0);
        assert_eq!(got, Some(HighlightSpan { start: 0, end: 13, kind: HighlightKind::Flag, link: None }));
    }

    #[test]
    fn classify_plain_argument_core_flag_miss_never_falls_through() {
        let data = man(&["-a"], &["commit"]);
        // "-x" doesn't match any known flag, and starting with '-' means
        // it must never be checked against subcommands either, even at
        // arg_index 0.
        let got = classify_plain_argument_core("-x", &(0..2), 0, Some(&data), None, 0);
        assert_eq!(got, None);
    }

    #[test]
    fn classify_plain_argument_core_no_man_data_yields_no_match() {
        let got = classify_plain_argument_core("-a", &(0..2), 0, None, None, 0);
        assert_eq!(got, None);
    }

    #[test]
    fn classify_plain_argument_core_matches_subcommand_only_at_arg_index_zero() {
        let data = man(&[], &["commit", "push"]);
        let span = 4..10;
        assert_eq!(
            classify_plain_argument_core("commit", &span, 0, Some(&data), None, 0),
            Some(HighlightSpan { start: 4, end: 10, kind: HighlightKind::Subcommand, link: None })
        );
        // Same text, later argument position -- single-level subcommand
        // support only, per this feature's v1 scope.
        assert_eq!(classify_plain_argument_core("commit", &span, 1, Some(&data), None, 0), None);
    }

    #[test]
    fn classify_plain_argument_core_offset_shifts_the_returned_span() {
        let data = man(&["-a"], &[]);
        let got = classify_plain_argument_core("-a", &(2..4), 0, Some(&data), None, 100);
        assert_eq!(got, Some(HighlightSpan { start: 102, end: 104, kind: HighlightKind::Flag, link: None }));
    }

    #[test]
    fn strip_flag_suffix_stops_at_equals_or_space() {
        assert_eq!(strip_flag_suffix("--color=auto"), "--color");
        assert_eq!(strip_flag_suffix("-C <path>"), "-C");
        assert_eq!(strip_flag_suffix("-a"), "-a");
    }

    #[test]
    fn resolve_file_link_finds_a_real_relative_path() {
        let cwd = std::env::current_dir().unwrap();
        let url = resolve_file_link("Cargo.toml", Some(&cwd));
        assert!(url.as_deref().is_some_and(|u| u.starts_with("file://")), "{url:?}");
    }

    #[test]
    fn resolve_file_link_returns_none_for_a_nonexistent_path() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(resolve_file_link("definitely-does-not-exist-xyz", Some(&cwd)), None);
    }

    #[test]
    fn resolve_file_link_returns_none_without_a_cwd() {
        // Command mode's own colon-line passes None -- "don't know, don't
        // guess" rather than resolving against this process's own cwd,
        // which can legitimately differ from the session's Shell.cwd.
        assert_eq!(resolve_file_link("Cargo.toml", None), None);
    }

    #[test]
    fn resolve_file_link_handles_an_absolute_path_directly() {
        let cwd = std::env::current_dir().unwrap();
        let abs = cwd.join("Cargo.toml");
        let url = resolve_file_link(abs.to_str().unwrap(), Some(&cwd));
        assert!(url.as_deref().is_some_and(|u| u.starts_with("file://")), "{url:?}");
    }

    #[test]
    fn classify_plain_argument_core_falls_back_to_link_when_no_flag_or_subcommand_matches() {
        let cwd = std::env::current_dir().unwrap();
        let data = man(&[], &[]);
        let span = 0..10;
        match classify_plain_argument_core("Cargo.toml", &span, 1, Some(&data), Some(&cwd), 0) {
            Some(HighlightSpan { start: 0, end: 10, kind: HighlightKind::Link, link: Some(url) }) => {
                assert!(url.starts_with("file://"), "{url}");
            }
            other => panic!("expected a Link span, got {other:?}"),
        }
    }

    #[test]
    fn classify_plain_argument_core_no_link_when_cwd_is_none() {
        let data = man(&[], &[]);
        assert_eq!(classify_plain_argument_core("Cargo.toml", &(0..10), 1, Some(&data), None, 0), None);
    }

    fn chars_of(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn scan_printf_directives_recognizes_every_supported_conversion() {
        for conv in ["s", "b", "c", "q", "d", "i", "u", "o", "x", "X"] {
            let text = format!("%{conv}");
            let chars = chars_of(&text);
            assert_eq!(scan_printf_directives(&chars), vec![(0..2, HighlightKind::FormatSpecifier)], "conv={conv}");
        }
    }

    #[test]
    fn scan_printf_directives_handles_flags_width_and_precision() {
        assert_eq!(scan_printf_directives(&chars_of("%-10s")), vec![(0..5, HighlightKind::FormatSpecifier)]);
        assert_eq!(scan_printf_directives(&chars_of("%05d")), vec![(0..4, HighlightKind::FormatSpecifier)]);
        assert_eq!(scan_printf_directives(&chars_of("%5.2f")), vec![]); // 'f' isn't supported by this shell's printf
        assert_eq!(scan_printf_directives(&chars_of("%.3s")), vec![(0..4, HighlightKind::FormatSpecifier)]);
    }

    #[test]
    fn scan_printf_directives_treats_literal_percent_as_its_own_directive() {
        assert_eq!(scan_printf_directives(&chars_of("100%% done")), vec![(3..5, HighlightKind::FormatSpecifier)]);
    }

    #[test]
    fn scan_printf_directives_skips_unrecognized_conversions_without_a_span() {
        assert_eq!(scan_printf_directives(&chars_of("%z")), vec![]);
    }

    #[test]
    fn scan_printf_directives_does_not_panic_on_a_trailing_percent() {
        assert_eq!(scan_printf_directives(&chars_of("done%")), vec![]);
    }

    #[test]
    fn scan_printf_directives_finds_multiple_directives_in_order() {
        assert_eq!(
            scan_printf_directives(&chars_of("%s is %d years old")),
            vec![(0..2, HighlightKind::FormatSpecifier), (6..8, HighlightKind::FormatSpecifier)]
        );
    }

    #[test]
    fn builtin_refine_only_fires_for_printf_at_arg_index_zero() {
        let text = chars_of("%s");
        assert_eq!(builtin_refine("printf", 0, &text), vec![(0..2, HighlightKind::FormatSpecifier)]);
        // `-v NAME FORMAT` shifts the real format string to arg_index 2 --
        // a documented v1 gap, degrading to no highlighting rather than a
        // wrong one.
        assert_eq!(builtin_refine("printf", 2, &text), vec![]);
        assert_eq!(builtin_refine("echo", 0, &text), vec![]);
    }

    // The user's own example from the feature request.
    #[test]
    fn printf_format_specifier_is_highlighted_within_the_quoted_string() {
        let text = "printf 'hello %s' world";
        let spans = kinds(text);
        let quote_start = text.find("'hello %s'").unwrap() + 1;
        let quote_end = quote_start + "hello %s".len();
        assert!(spans.contains(&(quote_start, quote_end, HighlightKind::String)), "{spans:?}");
        let pct_start = text.find("%s").unwrap();
        assert!(spans.contains(&(pct_start, pct_start + 2, HighlightKind::FormatSpecifier)), "{spans:?}");
    }
}
