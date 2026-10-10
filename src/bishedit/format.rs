// A formatter for bish scripts, built like lint.rs on
// `lexer::tokenize_spanned`'s spanned token stream (the AST has no source
// positions; see lint.rs).
//
// A token's text is copied through verbatim, and what changes is the gap
// between one item's end and the next one's start. Indentation is the gap
// before a line's first token; joining `then`/`do`/`in`/`{` onto their
// header line is choosing `; ` or ` ` as that gap instead of a newline. Two
// rules touch a token: the `function` keyword is deleted (see
// `Tok::KwFunction` below), and a wholly literal word is requoted to its
// plainest form (`canonical_quoting`).
//
// `$(...)`, backticks, `(...)` and `((...))` are each one token, newlines
// included, so their interiors are never reformatted. `[[ ... ]]` holds
// ordinary items and follows the general rules.
//
// `Tok::Newline` and `Tok::Semi` are separators, not items: they're part of
// the gap around them, which is how `if cond; then` doesn't get a second
// `; `.
//
// A heredoc's body has no span, so it ends up inside a gap. A gap holding
// anything but whitespace and `;` is never generated here, so such a gap is
// left untouched, which covers heredoc bodies and anything else unexpected
// (a `\` line continuation, say).

use crate::bishedit::lint::{Diagnostic, Fix, Severity};
use crate::lexer::{self, SpannedItem, Tok};
use crate::parser::Parser;
use std::borrow::Cow;

pub struct BashFormatter;

impl BashFormatter {
    // Err if the script doesn't parse: nothing invalid is reformatted. Not
    // the `Linter` trait, whose `check` can't fail.
    pub fn check(&self, source: &str) -> Result<Vec<Diagnostic>, String> {
        let toks = lexer::Lexer::new(source).tokenize()?;
        Parser::new(toks).parse_program()?;

        let res = lexer::tokenize_spanned(source);
        if let Some(e) = res.error {
            return Err(e);
        }
        let chars: Vec<char> = source.chars().collect();
        let real: Vec<&SpannedItem> =
            res.items.iter().filter(|it| !matches!(it, SpannedItem::Tok(Tok::Newline, _) | SpannedItem::Tok(Tok::Semi, _))).collect();
        Ok(format_gaps(&chars, &real))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    If,
    ForLoop,
    Case,
    CaseArm,
    Brace,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Joiner {
    Then,
    Do,
    In,
}

struct Block {
    kind: BlockKind,
    // The depth of the block's header line (the opening keyword or `{`, or
    // for a CaseArm its `pattern)` line). The body is one deeper; a closer
    // goes back to this depth.
    header_depth: usize,
    awaiting: Option<Joiner>,
}

fn current_depth(stack: &[Block]) -> usize {
    match stack.last() {
        None => 0,
        Some(b) if b.awaiting.is_some() => b.header_depth,
        Some(b) => b.header_depth + 1,
    }
}

// One level under the innermost open block's header, even while it's still
// awaiting `then`/`do`/`in`: the indent for a line break preserved inside a
// header, such as a multi-line `[[ ... ]]` before `then`. Unlike
// `current_depth`, which places a nested block's header: a condition that
// is itself an `if` stays flush with the outer header.
fn content_depth(stack: &[Block]) -> usize {
    stack.last().map(|b| b.header_depth + 1).unwrap_or(0)
}

// Whether the parentheses at `real[i]` belong to the word before them
// rather than opening a subshell. The only two ways a Word is legally
// followed by a Subshell:
//
//     NAME()      a function definition: the parens are empty, and a
//                 subshell command never is
//     NAME=(...)  an array assignment, or `NAME+=(...)`
//
// Neither takes a gap; `arr= (a b)` is a syntax error in bash.
fn parens_belong_to_previous_word(chars: &[char], real: &[&SpannedItem], i: usize, inner: &str) -> bool {
    let Some(SpannedItem::Tok(Tok::Word(..), span)) = i.checked_sub(1).and_then(|j| real.get(j)).copied() else {
        return false;
    };
    inner.is_empty() || span.end.checked_sub(1).and_then(|last| chars.get(last)) == Some(&'=')
}

// Whether `real[lbrace_idx]` opens a function body (`foo() {`,
// `function foo {`, `function foo() {`) rather than a `{ ... }` group,
// following `Parser::looks_like_func_def` and `parse_function_kw`. `real`
// has no separators, so the preceding items are exactly the ones that
// matter.
fn is_function_brace(real: &[&SpannedItem], lbrace_idx: usize) -> bool {
    let at = |off: usize| -> Option<&Tok> {
        let idx = lbrace_idx.checked_sub(off)?;
        match real.get(idx)? {
            SpannedItem::Tok(t, _) => Some(t),
            SpannedItem::Comment(_) => None,
        }
    };
    match (at(1), at(2)) {
        (Some(Tok::Subshell { raw: s, .. }), Some(Tok::Word(..))) if s.is_empty() => true,
        (Some(Tok::Word(..)), Some(Tok::KwFunction)) => true,
        _ => false,
    }
}

// `function foo {`, with no `()`: normalizing to `foo() {` has to add them.
fn function_brace_omits_parens(real: &[&SpannedItem], lbrace_idx: usize) -> bool {
    let at = |off: usize| -> Option<&Tok> {
        let idx = lbrace_idx.checked_sub(off)?;
        match real.get(idx)? {
            SpannedItem::Tok(t, _) => Some(t),
            SpannedItem::Comment(_) => None,
        }
    };
    matches!((at(1), at(2)), (Some(Tok::Word(..)), Some(Tok::KwFunction)))
}

fn indent(depth: usize) -> String {
    "\t".repeat(depth)
}

fn newline_at(depth: usize) -> String {
    format!("\n{}", indent(depth))
}

fn describe(replacement: &str) -> String {
    match replacement {
        "" => "no space expected here".to_string(),
        " " => "expected a single space here".to_string(),
        "  " => "expected two spaces before this trailing comment".to_string(),
        "; " => "expected `;` joining this onto the previous line".to_string(),
        "() " => "expected `()` before this function body's `{`".to_string(),
        s if s.starts_with("\n\n") => format!("expected exactly one blank line, then indentation of {} tab(s)", s.matches('\t').count()),
        s if s.starts_with('\n') => format!("expected a line break here, indented {} tab(s)", s.matches('\t').count()),
        _ => "this doesn't match bish tool format's expected layout".to_string(),
    }
}

fn span_of(item: &SpannedItem) -> std::ops::Range<usize> {
    match item {
        SpannedItem::Tok(_, span) | SpannedItem::Comment(span) => span.clone(),
    }
}

// Quoting a word that doesn't need it is noise. Three answers, in order:
//
// - Nothing in the text needs quoting: drop the quotes.
// - Something does, but nothing needs double quotes (no expansion or
//   substitution): single-quote it.
// - Otherwise leave it.
//
// Only for a word that is one wholly literal run. A word built from several
// pieces (`"$(foo)"'\n'a"$(bar)"`) is left as written: each piece is quoted
// for its own reason.
fn canonical_quoting(text: &str) -> Option<String> {
    if bare_is_safe(text) {
        return Some(text.to_string());
    }
    match text.contains('\'') {
        // A `'` inside would end the quote, and `'\''` is no improvement.
        true => None,
        false => Some(format!("'{text}'")),
    }
}

// Whether `text` means itself with no quoting at all. A short allow-list of
// characters nothing gives a meaning to. No glob metacharacter is on it, so
// this is safe in a `case` pattern and beside `[[ x == y ]]` too.
fn bare_is_safe(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    // `#` and `~` matter only where a word begins, which is where bash's
    // `printf %q` escapes them.
    let safe_anywhere = |c: char| c.is_alphanumeric() || "%+-./:@_=".contains(c);
    if !text.chars().enumerate().all(|(i, c)| safe_anywhere(c) || (i > 0 && (c == '#' || c == '~'))) {
        return false;
    }
    // `2>file` redirects fd 2, where `"2">file` writes the word.
    if text.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    // `"a=b" cmd` runs a command called `a=b`; `a=b cmd` sets a variable. A
    // name can't start with `-`, so `--flag=value` still loses its quotes.
    let name = text.split('=').next().unwrap_or_default();
    let assignment_shaped = text.contains('=')
        && !name.is_empty()
        && name.starts_with(|c: char| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    !assignment_shaped
}

// The text of a word that is one wholly literal run; `None` for an
// expansion, several pieces, or a word that was never quoted.
fn sole_literal(chunks: &[lexer::Chunk], plain: bool) -> Option<&str> {
    if plain {
        return None;
    }
    match chunks {
        // `''` and `""` both arrive as one empty `Str`.
        [lexer::Chunk::Str(text)] if text.is_empty() => Some(text),
        [lexer::Chunk::LiteralStr(text, _)] => Some(text),
        _ => None,
    }
}

// One pass over `real`, deciding for every adjacent pair (and before the
// first and after the last item) what the gap should be, and comparing it
// with what's there. `forced_next` carries a requirement set by the item
// just processed (`then` wants a line break after it) to the next
// iteration. A closer's own requirement (`fi` dedenting) takes priority
// over it; see the empty `if ...; then fi` case.
fn format_gaps(chars: &[char], real: &[&SpannedItem]) -> Vec<Diagnostic> {
    let mut stack: Vec<Block> = Vec::new();
    let mut diagnostics = Vec::new();
    let mut prev_end = 0usize;
    let mut forced_next: Option<String> = None;
    // Set when the `Tok::KwFunction` arm has deleted `function`: the next
    // item (the name) then skips its gap check, since that gap is part of
    // the deletion and two fixes over one span would conflict.
    let mut suppress_next_gap = false;

    for i in 0..=real.len() {
        let prev_forced = forced_next.take();
        let this_suppressed = std::mem::replace(&mut suppress_next_gap, false);
        let item = real.get(i).copied();
        let this_start = item.map(|it| span_of(it).start).unwrap_or(chars.len());

        // In a case's pattern list (`in` consumed, no arm open), `|` and
        // the closing `)` take no spaces: `foo|bar)`.
        let in_case_patterns = matches!(stack.last(), Some(b) if b.kind == BlockKind::Case && b.awaiting.is_none());
        let comment_precedes = i > 0 && matches!(real.get(i - 1), Some(SpannedItem::Comment(_)));
        // Before this item's arm can push a block for its own body, which
        // mustn't move where the opener itself lands.
        let depth_before = content_depth(&stack);

        let mut leading_override: Option<String> = None;
        if let Some(SpannedItem::Tok(tok, _)) = item {
            match tok {
                Tok::KwThen => {
                    if matches!(stack.last(), Some(b) if b.awaiting == Some(Joiner::Then)) && !comment_precedes {
                        stack.last_mut().unwrap().awaiting = None;
                        leading_override = Some("; ".to_string());
                        forced_next = Some(newline_at(current_depth(&stack)));
                    }
                }
                Tok::KwDo => {
                    if matches!(stack.last(), Some(b) if b.kind == BlockKind::ForLoop && b.awaiting == Some(Joiner::Do)) && !comment_precedes {
                        stack.last_mut().unwrap().awaiting = None;
                        leading_override = Some("; ".to_string());
                        forced_next = Some(newline_at(current_depth(&stack)));
                    }
                }
                Tok::KwIn => {
                    if matches!(stack.last(), Some(b) if b.kind == BlockKind::Case && b.awaiting == Some(Joiner::In)) && !comment_precedes {
                        stack.last_mut().unwrap().awaiting = None;
                        leading_override = Some(" ".to_string());
                        forced_next = Some(newline_at(current_depth(&stack)));
                    }
                }
                Tok::KwElif => {
                    if matches!(stack.last(), Some(b) if b.kind == BlockKind::If) {
                        let hd = stack.last().unwrap().header_depth;
                        leading_override = Some(newline_at(hd));
                        stack.last_mut().unwrap().awaiting = Some(Joiner::Then);
                    }
                }
                Tok::KwElse => {
                    if matches!(stack.last(), Some(b) if b.kind == BlockKind::If) {
                        let hd = stack.last().unwrap().header_depth;
                        leading_override = Some(newline_at(hd));
                        stack.last_mut().unwrap().awaiting = None;
                        forced_next = Some(newline_at(current_depth(&stack)));
                    }
                }
                Tok::KwFi => {
                    if matches!(stack.last(), Some(b) if b.kind == BlockKind::If) {
                        leading_override = Some(newline_at(stack.last().unwrap().header_depth));
                    }
                    stack.pop();
                }
                Tok::KwDone => {
                    if matches!(stack.last(), Some(b) if b.kind == BlockKind::ForLoop) {
                        leading_override = Some(newline_at(stack.last().unwrap().header_depth));
                    }
                    stack.pop();
                }
                Tok::KwEsac => {
                    if matches!(stack.last(), Some(b) if b.kind == BlockKind::CaseArm) {
                        stack.pop();
                    }
                    if matches!(stack.last(), Some(b) if b.kind == BlockKind::Case) {
                        leading_override = Some(newline_at(stack.last().unwrap().header_depth));
                    }
                    stack.pop();
                }
                Tok::RBrace => {
                    if matches!(stack.last(), Some(b) if b.kind == BlockKind::Brace) {
                        leading_override = Some(newline_at(stack.last().unwrap().header_depth));
                    }
                    stack.pop();
                }
                Tok::DSemi | Tok::SemiAmp | Tok::DSemiAmp => {
                    if matches!(stack.last(), Some(b) if b.kind == BlockKind::CaseArm) {
                        leading_override = Some(newline_at(stack.last().unwrap().header_depth));
                        stack.pop();
                    }
                }
                Tok::RParen if in_case_patterns => {
                    leading_override = Some(String::new());
                    let case_depth = stack.last().unwrap().header_depth;
                    stack.push(Block { kind: BlockKind::CaseArm, header_depth: case_depth + 1, awaiting: None });
                    forced_next = Some(newline_at(current_depth(&stack)));
                }
                Tok::Pipe if in_case_patterns => {
                    leading_override = Some(String::new());
                    forced_next = Some(String::new());
                }
                // Parentheses that belong to the word before them; see
                // `parens_belong_to_previous_word`.
                Tok::Subshell { raw: inner, .. } if parens_belong_to_previous_word(chars, real, i, inner) => {
                    leading_override = Some(String::new());
                }
                Tok::LBrace => {
                    let is_func = is_function_brace(real, i);
                    if is_func && !comment_precedes {
                        // `function foo {`: the `KwFunction` arm deleted
                        // `function` and suppressed its gap check, so
                        // adding `()` in this gap's fix is a single edit.
                        let parens = if function_brace_omits_parens(real, i) { "()" } else { "" };
                        leading_override = Some(format!("{parens} "));
                    }
                    let hd = current_depth(&stack);
                    stack.push(Block { kind: BlockKind::Brace, header_depth: hd, awaiting: None });
                    forced_next = Some(newline_at(current_depth(&stack)));
                }
                // Quoting a word that doesn't need it; see
                // `canonical_quoting`. Never touches a word with an
                // expansion in it.
                Tok::Word(chunks, plain) => {
                    let span = span_of(item.unwrap());
                    if let Some(text) = sole_literal(chunks, *plain)
                        && let Some(canonical) = canonical_quoting(text)
                    {
                        let source: String = chars[span.start..span.end].iter().collect();
                        if canonical != source {
                            diagnostics.push(Diagnostic {
                                start: span.start,
                                end: span.end,
                                severity: Severity::Warning,
                                code: Cow::Borrowed("format"),
                                source: None,
                                message: format!("expected `{canonical}` here"),
                                fix: Some(Fix { start: span.start, end: span.end, replacement: canonical }),
                            });
                        }
                    }
                }
                // `function foo { ... }` and `function foo() { ... }`
                // become `foo() { ... }`. Only for a brace body, checked
                // through to the `{`; `function foo` followed by any other
                // body (valid, rare) is left alone.
                //
                // Deletes `function` and the gap after it, up to the name.
                // The gap before `function` is resolved normally below.
                // Sets `suppress_next_gap` so the name's gap isn't fixed
                // twice (see `apply_fixes` in tool.rs).
                Tok::KwFunction => {
                    let at = |off: usize| -> Option<&Tok> {
                        match real.get(i + off)? {
                            SpannedItem::Tok(t, _) => Some(t),
                            SpannedItem::Comment(_) => None,
                        }
                    };
                    if matches!(at(1), Some(Tok::Word(..))) {
                        let brace_off = if matches!(at(2), Some(Tok::Subshell { raw: s, .. }) if s.is_empty()) { 3 } else { 2 };
                        if matches!(at(brace_off), Some(Tok::LBrace)) {
                            let word_start = span_of(real[i + 1]).start;
                            let kw_span = span_of(item.unwrap());
                            diagnostics.push(Diagnostic {
                                start: kw_span.start,
                                end: word_start,
                                severity: Severity::Warning,
                                code: Cow::Borrowed("format"),
                                source: None,
                                message: "expected no `function` keyword here -- use `NAME() { ... }`".to_string(),
                                fix: Some(Fix { start: kw_span.start, end: word_start, replacement: String::new() }),
                            });
                            suppress_next_gap = true;
                        }
                    }
                }
                Tok::KwIf => {
                    stack.push(Block { kind: BlockKind::If, header_depth: current_depth(&stack), awaiting: Some(Joiner::Then) });
                }
                Tok::KwFor | Tok::KwWhile | Tok::KwUntil | Tok::KwSelect => {
                    stack.push(Block { kind: BlockKind::ForLoop, header_depth: current_depth(&stack), awaiting: Some(Joiner::Do) });
                }
                Tok::KwCase => {
                    stack.push(Block { kind: BlockKind::Case, header_depth: current_depth(&stack), awaiting: Some(Joiner::In) });
                }
                _ => {}
            }
        }

        // Resolve the gap text for [prev_end, this_start).
        let actual: String = chars[prev_end..this_start].iter().collect();
        let safe_to_format = actual.chars().all(|c| c.is_whitespace() || c == ';');
        if safe_to_format && !this_suppressed {
            let intended = if item.is_none() {
                // EOF: exactly one trailing newline.
                "\n".to_string()
            } else if let Some(o) = leading_override {
                o
            } else if let Some(f) = prev_forced {
                f
            } else {
                let has_semi = actual.contains(';');
                let newlines = actual.matches('\n').count();
                if prev_end == 0 {
                    String::new()
                } else if has_semi && newlines == 0 {
                    "; ".to_string()
                } else if has_semi {
                    // A `;` together with a line break in one gap is left
                    // as it is.
                    actual.clone()
                } else if newlines == 0 {
                    if matches!(item, Some(SpannedItem::Comment(_))) { "  ".to_string() } else { " ".to_string() }
                } else {
                    let blank = newlines >= 2;
                    format!("{}{}", if blank { "\n\n" } else { "\n" }, indent(depth_before))
                }
            };
            if actual != intended {
                diagnostics.push(Diagnostic {
                    start: prev_end,
                    end: this_start,
                    severity: Severity::Warning,
                    code: Cow::Borrowed("format"),
                    source: None,
                    message: describe(&intended),
                    fix: Some(Fix { start: prev_end, end: this_start, replacement: intended }),
                });
            }
        }

        prev_end = item.map(|it| span_of(it).end).unwrap_or(chars.len());
    }

    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format_text(source: &str) -> String {
        let diagnostics = BashFormatter.check(source).unwrap();
        let mut chars: Vec<char> = source.chars().collect();
        let mut fixes: Vec<&Fix> = diagnostics.iter().filter_map(|d| d.fix.as_ref()).collect();
        fixes.sort_by_key(|f| std::cmp::Reverse(f.start));
        for f in fixes {
            chars.splice(f.start..f.end, f.replacement.chars());
        }
        chars.into_iter().collect()
    }

    #[test]
    fn a_word_that_needs_no_quoting_loses_it() {
        assert_eq!(format_text("echo \"hello\"\n"), "echo hello\n");
        assert_eq!(format_text("echo 'hello'\n"), "echo hello\n");
        assert_eq!(format_text("echo \"--flag=value\"\n"), "echo --flag=value\n");
        assert_eq!(format_text("echo 'a.b/c:d@e'\n"), "echo a.b/c:d@e\n");
        assert_eq!(format_text("echo 'x#y'\n"), "echo x#y\n");
        assert_eq!(format_text("echo 'a:~/b'\n"), "echo a:~/b\n");
        assert_eq!(format_text("echo '#x'\n"), "echo '#x'\n");
        assert_eq!(format_text("echo '~/x'\n"), "echo '~/x'\n");
    }

    #[test]
    fn a_word_with_nothing_to_expand_gets_single_quotes() {
        assert_eq!(format_text("echo \"hello world\"\n"), "echo 'hello world'\n");
        assert_eq!(format_text("echo \"a\\$b\"\n"), "echo 'a$b'\n");
        assert_eq!(format_text("echo \"\"\n"), "echo ''\n");
        // Glob and brace metacharacters keep their quotes; single quotes
        // hold them just as literally.
        assert_eq!(format_text("echo \"*\"\n"), "echo '*'\n");
        assert_eq!(format_text("echo \"{a,b}\"\n"), "echo '{a,b}'\n");
        assert_eq!(format_text("case $x in \"a b\") echo \"y\";; esac\n"), "case $x in\n\t'a b')\n\t\techo y\n\t;;\nesac\n");
    }

    #[test]
    fn quoting_that_is_doing_something_is_left_alone() {
        // An expansion needs its double quotes.
        assert_eq!(format_text("echo \"$x\"\n"), "echo \"$x\"\n");
        assert_eq!(format_text("echo \"$(foo)\"\n"), "echo \"$(foo)\"\n");
        let mixed = "echo \"$(foo)\"'\\n'a\"$(bar)\"\n";
        assert_eq!(format_text(mixed), mixed);
        assert_eq!(format_text("echo a\"b\"c\n"), "echo a\"b\"c\n");
        assert_eq!(format_text("a=\"b\"\n"), "a=\"b\"\n");
        assert_eq!(format_text("echo \"it's\"\n"), "echo \"it's\"\n");
        assert_eq!(format_text("echo \"a=b\"\n"), "echo 'a=b'\n");
        assert_eq!(format_text("\"a=b\" cmd\n"), "'a=b' cmd\n");
        assert_eq!(format_text("echo \"2\"\n"), "echo '2'\n");
    }

    #[test]
    fn requoting_settles_in_one_pass() {
        let src = "echo \"a\" 'b' \"c d\" \"$x\" \"e=f\" \"2\" '#g' 'h#i'\n";
        let once = format_text(&src);
        assert_eq!(once, "echo a b 'c d' \"$x\" 'e=f' '2' '#g' h#i\n");
        assert_eq!(format_text(&once), once, "formatting a formatted file changes nothing");
    }

    // `arr= (a b)` is a syntax error in bash, so the parentheses of an
    // array assignment take no gap.
    #[test]
    fn an_array_assignment_keeps_its_parentheses_attached() {
        assert_eq!(format_text("arr=(a b)\n"), "arr=(a b)\n");
        assert_eq!(format_text("arr+=(\"$y\")\n"), "arr+=(\"$y\")\n");
        assert_eq!(format_text("declare -a x=(1 2)\n"), "declare -a x=(1 2)\n");
        // The parser rejects `arr= (a b)`, so it can't be formatted at all.
        assert!(BashFormatter.check("arr= (a b)\n").is_err());
        assert_eq!(format_text("echo hi\n(echo sub)\n"), "echo hi\n(echo sub)\n");
    }

    #[test]
    fn if_then_joins_onto_one_line_and_fi_dedents() {
        let src = "if true\nthen\necho hi\nfi\n";
        assert_eq!(format_text(src), "if true; then\n\techo hi\nfi\n");
    }

    #[test]
    fn already_formatted_if_produces_no_diagnostics() {
        let src = "if true; then\n\techo hi\nfi\n";
        assert!(BashFormatter.check(src).unwrap().is_empty());
    }

    #[test]
    fn an_existing_semicolon_before_then_is_reused_not_duplicated() {
        let src = "if true ; then\n\techo hi\nfi\n";
        assert_eq!(format_text(src), "if true; then\n\techo hi\nfi\n");
    }

    #[test]
    fn if_elif_else_fi_all_dedent_to_the_if_s_own_depth() {
        let src = "if a\nthen\nx\nelif b\nthen\ny\nelse\nz\nfi\n";
        assert_eq!(format_text(src), "if a; then\n\tx\nelif b; then\n\ty\nelse\n\tz\nfi\n");
    }

    #[test]
    fn nested_if_indents_two_levels() {
        let src = "if a\nthen\nif b\nthen\nx\nfi\nfi\n";
        assert_eq!(format_text(src), "if a; then\n\tif b; then\n\t\tx\n\tfi\nfi\n");
    }

    #[test]
    fn sequential_top_level_blocks_all_stay_at_depth_zero() {
        // An opener takes the depth from before it pushes its block;
        // otherwise every later block at that depth drifts one level
        // deeper.
        let src = "if true\nthen\nx\nfi\nfor i in a\ndo\ny\ndone\ncase i in\na)\nz\nesac\n";
        let expected = "if true; then\n\tx\nfi\nfor i in a; do\n\ty\ndone\ncase i in\n\ta)\n\t\tz\nesac\n";
        assert_eq!(format_text(src), expected);
    }

    #[test]
    fn for_do_done() {
        let src = "for x in a b\ndo\necho $x\ndone\n";
        assert_eq!(format_text(src), "for x in a b; do\n\techo $x\ndone\n");
    }

    #[test]
    fn while_and_until_do_done() {
        assert_eq!(format_text("while true\ndo\nx\ndone\n"), "while true; do\n\tx\ndone\n");
        assert_eq!(format_text("until true\ndo\nx\ndone\n"), "until true; do\n\tx\ndone\n");
    }

    #[test]
    fn select_do_done() {
        let src = "select x in a b\ndo\necho $x\ndone\n";
        assert_eq!(format_text(src), "select x in a b; do\n\techo $x\ndone\n");
    }

    #[test]
    fn case_arms_and_each_terminator_kind() {
        let src = "case $x in\nfoo)\ncmd1\n;;\nbar)\ncmd2\n;&\nbaz)\ncmd3\n;;&\nqux)\ncmd4\nesac\n";
        let expected = "case $x in\n\tfoo)\n\t\tcmd1\n\t;;\n\tbar)\n\t\tcmd2\n\t;&\n\tbaz)\n\t\tcmd3\n\t;;&\n\tqux)\n\t\tcmd4\nesac\n";
        assert_eq!(format_text(src), expected);
    }

    #[test]
    fn case_pattern_pipes_and_paren_get_no_surrounding_space() {
        let src = "case $x in\nfoo | bar)\ncmd\n;;\nesac\n";
        assert_eq!(format_text(src), "case $x in\n\tfoo|bar)\n\t\tcmd\n\t;;\nesac\n");
    }

    #[test]
    fn function_def_both_spellings_join_the_brace() {
        assert_eq!(format_text("foo()\n{\nx\n}\n"), "foo() {\n\tx\n}\n");
    }

    // `NAME() { ... }` is the one spelling: both other productions become
    // it, dropping `function` and adding `()` when absent.
    #[test]
    fn function_keyword_spelling_without_parens_normalizes_to_name_parens_brace() {
        assert_eq!(format_text("function foo\n{\nx\n}\n"), "foo() {\n\tx\n}\n");
    }

    #[test]
    fn function_keyword_spelling_with_parens_normalizes_to_name_parens_brace() {
        assert_eq!(format_text("function foo()\n{\nx\n}\n"), "foo() {\n\tx\n}\n");
    }

    #[test]
    fn already_canonical_function_def_produces_no_diagnostics() {
        assert!(BashFormatter.check("foo() {\n\tx\n}\n").unwrap().is_empty());
    }

    #[test]
    fn function_keyword_spelling_normalizes_regardless_of_surrounding_whitespace() {
        // A newline between `function` and the name isn't valid bish, so
        // only extra spaces are tried.
        assert_eq!(format_text("function   foo   {\nx\n}\n"), "foo() {\n\tx\n}\n");
    }

    #[test]
    fn a_function_body_that_is_not_a_compound_command_is_a_syntax_error() {
        // Not a function in bash either: a function body is a compound
        // command. What the formatter can't parse it doesn't touch.
        assert!(BashFormatter.check("function foo echo hi\n").is_err());
        assert!(BashFormatter.check("foo() echo hi\n").is_err());
    }

    #[test]
    fn multiple_function_defs_and_ordinary_brace_groups_all_normalize_independently() {
        let src = "function a\n{\nx\n}\nb()\n{\ny\n}\nfunction c()\n{\nz\n}\n{\nplain\n}\n";
        let expected = "a() {\n\tx\n}\nb() {\n\ty\n}\nc() {\n\tz\n}\n{\n\tplain\n}\n";
        assert_eq!(format_text(src), expected);
    }

    #[test]
    fn bare_brace_group_is_not_joined_but_still_indents() {
        let src = "{\nx\n}\n";
        assert_eq!(format_text(src), "{\n\tx\n}\n");
    }

    #[test]
    fn standalone_comment_gets_its_own_indented_line() {
        let src = "if true; then\n# a comment\necho hi\nfi\n";
        assert_eq!(format_text(src), "if true; then\n\t# a comment\n\techo hi\nfi\n");
    }

    #[test]
    fn trailing_comment_stays_on_the_same_line_with_two_spaces() {
        let src = "echo hi # trailing\n";
        assert_eq!(format_text(src), "echo hi  # trailing\n");
    }

    #[test]
    fn a_comment_before_then_is_not_joined_across() {
        // Joining `; then` past the comment would put `then` inside it, so
        // `then` stays on its own line.
        let src = "if true # why\nthen\nx\nfi\n";
        let out = format_text(src);
        assert!(out.contains("true  # why\n\tthen"), "got: {out:?}");
        assert!(BashFormatter.check(&out).unwrap().is_empty());
    }

    #[test]
    fn blank_lines_collapse_to_at_most_one() {
        let src = "echo a\n\n\n\necho b\n";
        assert_eq!(format_text(src), "echo a\n\necho b\n");
    }

    #[test]
    fn no_blank_line_forced_right_after_an_opener() {
        let src = "if true; then\n\n\techo hi\nfi\n";
        assert_eq!(format_text(src), "if true; then\n\techo hi\nfi\n");
    }

    #[test]
    fn heredoc_body_is_left_byte_for_byte_untouched() {
        let src = "cat <<EOF\n    weird   spacing\n\t\tand tabs\nEOF\necho after\n";
        assert_eq!(format_text(src), src);
    }

    #[test]
    fn subshell_and_arith_and_command_substitution_bodies_are_untouched() {
        let src = "x=$(\n  echo a\n    echo b\n)\n(\n  echo c\n)\n((\n  1+1\n))\n";
        assert_eq!(format_text(src), src);
    }

    #[test]
    fn multiline_test_expression_is_reindented_like_ordinary_content() {
        let src = "if [[ -n a\n&& -n b ]]\nthen\nx\nfi\n";
        assert_eq!(format_text(src), "if [[ -n a\n\t&& -n b ]]; then\n\tx\nfi\n");
    }

    #[test]
    fn leading_and_trailing_whitespace_at_file_boundaries_is_normalized() {
        assert_eq!(format_text("\n\necho hi\n\n\n"), "echo hi\n");
        assert_eq!(format_text("echo hi"), "echo hi\n");
    }

    #[test]
    fn unparseable_script_returns_an_error() {
        assert!(BashFormatter.check("if true then").is_err());
    }
}
