// Syntax highlighting, built on top of the real lexer rather than a
// parallel implementation (see plan.md / the approved design doc for this
// feature) -- Highlighter is a small trait so a regex-, treesitter-, or
// LSP-backed source can slot in later; BashHighlighter is the only
// implementor today, driven by lexer::tokenize_spanned.
//
// Not yet wired into editor.rs -- lands ahead of its consumer, same
// "build the seam, wire it in later" pattern as several other modules in
// this crate (vt100.rs, pty.rs before the M9 compositor).
#![allow(dead_code)]

use crate::lexer::{self, Chunk, SpannedItem, Tok};
use crate::vt100;
use std::ops::Range;

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
}

#[derive(Debug, Clone, PartialEq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub kind: HighlightKind,
}

pub trait Highlighter {
    fn highlight(&self, text: &str) -> Vec<HighlightSpan>;
}

pub struct BashHighlighter;

impl Highlighter for BashHighlighter {
    fn highlight(&self, text: &str) -> Vec<HighlightSpan> {
        let mut out = Vec::new();
        highlight_into(text, 0, &mut out);
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
    match kind {
        HighlightKind::Keyword => (vt100::Color::Indexed(3), bold),
        HighlightKind::String => (vt100::Color::Indexed(2), vt100::CellAttrs::default()),
        HighlightKind::Variable => (vt100::Color::Indexed(6), bold),
        HighlightKind::Substitution => (vt100::Color::Indexed(4), bold),
        HighlightKind::Redirect => (vt100::Color::Indexed(5), bold),
        HighlightKind::Operator => (vt100::Color::Indexed(7), vt100::CellAttrs::default()),
        HighlightKind::Comment => (vt100::Color::Indexed(8), dim),
        HighlightKind::Number => (vt100::Color::Indexed(6), vt100::CellAttrs::default()),
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
fn highlight_into(text: &str, offset: usize, out: &mut Vec<HighlightSpan>) {
    let chars: Vec<char> = text.chars().collect();
    let res = lexer::tokenize_spanned(text);
    let mut cursor = 0usize;
    for item in &res.items {
        match item {
            SpannedItem::Comment(r) => {
                out.push(HighlightSpan { start: offset + r.start, end: offset + r.end, kind: HighlightKind::Comment });
            }
            SpannedItem::Tok(tok, span) => {
                highlight_tok(tok, span, offset, &chars, &res.raw_capture_spans, &mut cursor, out);
            }
        }
    }
}

fn highlight_tok(
    tok: &Tok,
    span: &Range<usize>,
    offset: usize,
    chars: &[char],
    raw_spans: &[Range<usize>],
    cursor: &mut usize,
    out: &mut Vec<HighlightSpan>,
) {
    let whole = |kind: HighlightKind| HighlightSpan { start: offset + span.start, end: offset + span.end, kind };
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
                highlight_into(raw, offset + inner.start, out);
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

        Tok::Word(chunks, _plain) => highlight_word(chunks, offset, chars, raw_spans, cursor, out),
    }
}

// An all-plain word ([Chunk::Str(_)], the common unquoted case with no
// quoting/escaping/expansion at all) stays uncolored; otherwise each chunk
// is walked in order, consuming one raw_capture_spans entry per non-Str
// chunk (see that field's own doc comment in lexer.rs for the exact
// invariant this relies on).
fn highlight_word(
    chunks: &[Chunk],
    offset: usize,
    chars: &[char],
    raw_spans: &[Range<usize>],
    cursor: &mut usize,
    out: &mut Vec<HighlightSpan>,
) {
    if let [Chunk::Str(_)] = chunks {
        return;
    }
    for chunk in chunks {
        match chunk {
            Chunk::Str(_) => {}

            Chunk::LiteralStr(_) => {
                if let Some(r) = next_span(raw_spans, cursor) {
                    out.push(HighlightSpan { start: offset + r.start, end: offset + r.end, kind: HighlightKind::String });
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
                    out.push(HighlightSpan { start: offset + r.start, end: offset + r.end, kind: HighlightKind::Variable });
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
                    out.push(HighlightSpan { start: offset + delim_start, end: offset + r.start, kind: HighlightKind::Substitution });
                    out.push(HighlightSpan { start: offset + r.end, end: offset + r.end + 1, kind: HighlightKind::Substitution });
                    highlight_into(raw, offset + r.start, out);
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
                    out.push(HighlightSpan { start: offset + full_start, end: offset + full_end, kind: HighlightKind::Substitution });
                }
            }

            // <(cmd) / >(cmd) process substitution -- same delimiter-plus-
            // recurse treatment as Chunk::Sub, but unambiguous (each
            // variant has exactly one possible delimiter pair), so no
            // peeking needed.
            Chunk::ProcSubIn { raw } => push_procsub(raw, offset, raw_spans, cursor, out),
            Chunk::ProcSubOut { raw } => push_procsub(raw, offset, raw_spans, cursor, out),
        }
    }
}

fn push_procsub(raw: &str, offset: usize, raw_spans: &[Range<usize>], cursor: &mut usize, out: &mut Vec<HighlightSpan>) {
    if let Some(r) = next_span(raw_spans, cursor) {
        let delim_start = r.start.saturating_sub(2);
        out.push(HighlightSpan { start: offset + delim_start, end: offset + r.start, kind: HighlightKind::Substitution });
        out.push(HighlightSpan { start: offset + r.end, end: offset + r.end + 1, kind: HighlightKind::Substitution });
        highlight_into(raw, offset + r.start, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(usize, usize, HighlightKind)> {
        let mut spans = BashHighlighter.highlight(text);
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
        let spans = BashHighlighter.highlight(text);
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
        ] {
            let _ = default_style(kind);
        }
    }
}
