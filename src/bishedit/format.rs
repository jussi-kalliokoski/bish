// A structural formatter for bish scripts -- built the same way lint.rs
// is, on top of lexer::tokenize_spanned's flat, char-offset-spanned
// token/comment stream, rather than a parallel implementation or the
// (unspanned) AST. See lint.rs's own doc comment for why the AST
// doesn't carry the source positions this needs.
//
// The whole formatter reduces to one idea: a token's own span is never
// rewritten -- its literal source text (quoting, expansion syntax, a
// subshell's raw interior, everything) is always copied through
// verbatim. All this ever changes is the *gap* between one real item's
// end and the next real item's start. Indentation is just the gap
// immediately before a line's first token; joining `then`/`do`/`in`/
// `{` onto their own header line (deliberately, per the user's own
// design -- not "detect and preserve the existing style") is choosing
// "; " (or " ") as that gap's text instead of a newline.
// $(...)/`...`/(...)/((...)) stay untouched with zero special-casing as
// a direct consequence of that same rule: their entire raw text,
// newlines included, is one token's own span (see Tok::Subshell/
// Tok::Arith) -- deliberately not recursed into for this first version,
// unlike lint.rs's own raw_capture_spans recursion. `[[ ... ]]` is the
// opposite case: its contents are ordinary individual items (see
// Parser::parse_test_atoms), so they fall out of the general gap rules
// with no special-casing at all.
//
// "Real items" deliberately excludes plain Tok::Newline/Tok::Semi --
// pure separators, never content in their own right -- from the walk
// entirely; their source text is just part of whatever gap surrounds
// them, inspected (for a literal ';' already there, or for a heredoc
// body -- see below) rather than treated as something needing its own
// leading/trailing gap. This is what keeps a script that already
// spells `if cond; then` from getting a *second*, redundant "; "
// spliced in right next to its own real semicolon.
//
// The one real hazard is heredoc bodies: Tok::HereDoc's own span only
// covers the "<<DELIM"/"<<-DELIM" operator text -- the body itself
// (consumed by Lexer::capture_heredoc_body) is never given a span at
// all, so it ends up as part of the gap between the item before it and
// the next real item after its terminator line. A gap containing
// anything other than whitespace/';' is never something this formatter
// generates on its own, so any gap like that is left completely
// untouched rather than guessed at -- covers heredoc bodies without
// needing to track them specially, and is a safe fallback for anything
// else unexpected (a `\` line continuation, say).

use crate::bishedit::lint::{Diagnostic, Fix, Severity};
use crate::lexer::{self, SpannedItem, Tok};
use crate::parser::Parser;

pub struct BashFormatter;

impl BashFormatter {
    // Err(parse error message) if the script doesn't parse at all --
    // refuses to reformat something that isn't valid bish in the first
    // place, same spirit as a real formatter bailing on a syntax error
    // rather than guessing at one. Not `bishedit::lint`'s own `Linter`
    // trait despite sharing its Diagnostic/Fix types: that trait's own
    // `check` can't fail, and a formatter genuinely needs to be able to.
    pub fn check(&self, source: &str) -> Result<Vec<Diagnostic>, String> {
        let toks = lexer::Lexer::new(source).tokenize()?;
        Parser::new(toks).parse_program()?;

        let res = lexer::tokenize_spanned(source);
        if let Some(e) = res.error {
            return Err(e);
        }
        let chars: Vec<char> = source.chars().collect();
        let real: Vec<&SpannedItem> = res.items.iter().filter(|it| !matches!(it, SpannedItem::Tok(Tok::Newline, _) | SpannedItem::Tok(Tok::Semi, _))).collect();
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
    // The depth of the block's own header line (the `if`/`for`/`while`/
    // `until`/`select`/`case`/`{` itself, or -- for CaseArm -- the
    // `pattern)` line). Body content is always header_depth + 1; a
    // closer/dedenting keyword always targets header_depth itself.
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

// Like current_depth, but always one level under the innermost open
// block's own header, even while it's still awaiting its own join
// keyword (then/do/in) -- used only for indenting a *preserved* line
// break within a header/condition itself (e.g. a multi-line `[[ ... ]]`
// before `then`), which should read as visually under the header the
// same way a wrapped condition would in any other language, without
// changing where current_depth says a *new* nested block's own header
// sits (current_depth's whole point -- see e.g. a condition that is
// itself another `if`, which must stay flush with the outer header, not
// jump ahead of it).
fn content_depth(stack: &[Block]) -> usize {
    stack.last().map(|b| b.header_depth + 1).unwrap_or(0)
}

// Whether `real[lbrace_idx]` (an LBrace) is a function body's own
// opening brace -- `foo() {` / `function foo {` / `function foo() {`
// -- rather than a bare `{ ... }` group command, matching
// Parser::looks_like_func_def/parse_function_kw's own two productions
// (src/parser.rs:406-440). No Newline/Semi-skipping needed here (unlike
// the parser's own lookback) since `real` has already filtered those
// out -- the immediately preceding real items are exactly the tokens
// that matter.
fn is_function_brace(real: &[&SpannedItem], lbrace_idx: usize) -> bool {
    let at = |off: usize| -> Option<&Tok> {
        let idx = lbrace_idx.checked_sub(off)?;
        match real.get(idx)? {
            SpannedItem::Tok(t, _) => Some(t),
            SpannedItem::Comment(_) => None,
        }
    };
    match (at(1), at(2)) {
        (Some(Tok::Subshell(s)), Some(Tok::Word(..))) if s.is_empty() => true,
        (Some(Tok::Word(..)), Some(Tok::KwFunction)) => true,
        _ => false,
    }
}

// True for is_function_brace's own "no `()`" production (`function foo
// {`) specifically -- i.e. real[lbrace_idx-1] is the function's Word and
// real[lbrace_idx-2] is KwFunction directly, with no Subshell("") (empty
// parens) in between. Used to decide whether normalizing to `NAME() {
// ... }` (see Tok::KwFunction's own arm below, and the user's own
// JS-style convention this whole rule follows) needs to synthesize `()`
// or they're already there.
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

// One forward pass over `real` (already Newline/Semi-filtered -- see
// this module's own doc comment), resolving -- for every adjacent pair,
// including the implicit boundaries before the first item and after
// the last -- what the gap between them ought to be, and comparing that
// against what's actually there. `forced_next` carries a gap
// requirement set by the item just processed (e.g. KwThen always wants
// a line break right after itself, at the body's new depth) forward to
// the *next* iteration, where it's read before that iteration's own
// item gets to set (or clear) its own leading requirement -- the two
// never alias the same variable at the same time, since a closer's own
// leading requirement (e.g. KwFi dedenting) always takes priority over
// whatever the previous item optimistically forced (see the empty
// `if ...; then fi` case).
fn format_gaps(chars: &[char], real: &[&SpannedItem]) -> Vec<Diagnostic> {
    let mut stack: Vec<Block> = Vec::new();
    let mut diagnostics = Vec::new();
    let mut prev_end = 0usize;
    let mut forced_next: Option<String> = None;
    // Set by Tok::KwFunction's own arm below when it's just pushed a
    // dedicated fix deleting `function` -- consumed (same take()-once
    // pattern as forced_next/prev_forced) by the very next iteration (the
    // function's own name) to skip its generic gap diagnostic entirely,
    // since that gap has already been folded into the deletion fix. See
    // that arm's own doc comment for why letting both run risks an
    // overlapping-fix conflict.
    let mut suppress_next_gap = false;

    for i in 0..=real.len() {
        let prev_forced = forced_next.take();
        let this_suppressed = std::mem::replace(&mut suppress_next_gap, false);
        let item = real.get(i).copied();
        let this_start = item.map(|it| span_of(it).start).unwrap_or(chars.len());

        // In a case's pattern list (top of stack is Case, `in` already
        // consumed, no arm open yet): `|` and the `)` that ends the
        // pattern list get no surrounding space ("foo|bar)"), matching
        // ordinary shell style -- everything else still uses the
        // generic rules below.
        let in_case_patterns = matches!(stack.last(), Some(b) if b.kind == BlockKind::Case && b.awaiting.is_none());
        let comment_precedes = i > 0 && matches!(real.get(i - 1), Some(SpannedItem::Comment(_)));
        // Captured *before* this item's own match arm below can mutate
        // `stack` (an opener like KwFor/KwCase/a bare LBrace pushes a
        // new block for its own body, which must not affect where the
        // opener keyword *itself* -- i.e. this same gap -- lands).
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
                // `NAME()` -- an empty Subshell right after a Word is
                // never a real (necessarily non-empty) subshell command;
                // it's always the parens of a function definition (see
                // is_function_brace/Parser::looks_like_func_def), which
                // idiomatically never has a space before them.
                Tok::Subshell(s) if s.is_empty() && matches!(i.checked_sub(1).and_then(|j| real.get(j)), Some(SpannedItem::Tok(Tok::Word(..), _))) => {
                    leading_override = Some(String::new());
                }
                Tok::LBrace => {
                    let is_func = is_function_brace(real, i);
                    if is_func && !comment_precedes {
                        // `function foo {` (no `()`) -- the Tok::KwFunction
                        // arm below strips `function` itself and suppresses
                        // its own gap check so there's nothing else to
                        // collide with here; synthesizing `()` right in
                        // this same gap-fix (rather than as its own,
                        // separately-positioned fix) is what keeps this a
                        // single, unambiguous edit at this position.
                        let parens = if function_brace_omits_parens(real, i) { "()" } else { "" };
                        leading_override = Some(format!("{parens} "));
                    }
                    let hd = current_depth(&stack);
                    stack.push(Block { kind: BlockKind::Brace, header_depth: hd, awaiting: None });
                    forced_next = Some(newline_at(current_depth(&stack)));
                }
                // `function foo { ... }` / `function foo() { ... }` ->
                // `foo() { ... }`, matching real bash's/JS's own
                // parenthesized style rather than the `function` keyword
                // spelling -- the user's own convention for this rule.
                // Only touches the brace-bodied shape (checked all the
                // way through to a genuine LBrace, not just "the next
                // token or two look right"): `function foo` followed by
                // anything else (a bare command, `if`/`case`/... as the
                // body -- valid but rare bash) isn't something dropping
                // the keyword alone would correctly rewrite, so it's left
                // untouched entirely, same "don't touch what isn't
                // squarely in scope" tolerance as the heredoc-body/
                // subshell-interior cases in this module's own doc
                // comment.
                //
                // Deletes exactly `function` plus whatever gap follows it
                // up to the function's own name -- never the gap *before*
                // `function` itself (still resolved completely normally
                // for this same iteration, by the code below, since
                // deleting the keyword doesn't change how the statement
                // it belongs to should be indented). Sets suppress_next_gap
                // so the *next* iteration (the name) skips its own
                // generic gap diagnostic entirely -- that gap has already
                // been folded into this single deletion fix, and letting
                // both run would risk two overlapping fixes fighting over
                // the same span (see apply_fixes' own overlap handling in
                // tool.rs) whenever the source had unusual spacing right
                // after `function`.
                Tok::KwFunction => {
                    let at = |off: usize| -> Option<&Tok> {
                        match real.get(i + off)? {
                            SpannedItem::Tok(t, _) => Some(t),
                            SpannedItem::Comment(_) => None,
                        }
                    };
                    if matches!(at(1), Some(Tok::Word(..))) {
                        let brace_off = if matches!(at(2), Some(Tok::Subshell(s)) if s.is_empty()) { 3 } else { 2 };
                        if matches!(at(brace_off), Some(Tok::LBrace)) {
                            let word_start = span_of(real[i + 1]).start;
                            let kw_span = span_of(item.unwrap());
                            diagnostics.push(Diagnostic {
                                start: kw_span.start,
                                end: word_start,
                                severity: Severity::Warning,
                                code: "format",
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

        // ---- Resolve the gap text for [prev_end, this_start) ----
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
                    // A ';' combined with a real line break in the same
                    // gap is an unusual, out-of-scope style (ordinary
                    // statement separation isn't part of what this
                    // formatter reflows) -- left untouched rather than
                    // guessed at.
                    actual.clone()
                } else if newlines == 0 {
                    if matches!(item, Some(SpannedItem::Comment(_))) {
                        "  ".to_string()
                    } else {
                        " ".to_string()
                    }
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
                    code: "format",
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
        // Regression: an opener (KwFor/KwCase/...) that falls through to
        // the generic same-depth-as-before branch must use the depth
        // from *before* it pushes its own block, not after -- otherwise
        // every block following the first one at a given depth drifts
        // one level too deep.
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

    // JS-style `NAME() { ... }` is the one canonical spelling this
    // formatter enforces -- both of bash's other productions normalize
    // to it, dropping `function` and synthesizing `()` when it's absent.
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
        // Note: a newline right after `function` (before its own name)
        // isn't valid bish syntax at all -- parse_function_kw doesn't
        // skip terminators there -- so only extra *spaces* are exercised.
        assert_eq!(format_text("function   foo   {\nx\n}\n"), "foo() {\n\tx\n}\n");
    }

    #[test]
    fn a_function_keyword_def_whose_body_is_not_a_brace_group_is_left_untouched() {
        // Out of scope: normalizing this would require wrapping the body
        // in `{ ... }` too, a different (and riskier) transformation.
        let src = "function foo echo hi\n";
        assert!(BashFormatter.check(src).unwrap().is_empty());
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
        // Joining "; then" past the comment would put `then` inside the
        // comment text -- must fall back to leaving `then` on its own
        // line instead of corrupting the script.
        let src = "if true # why\nthen\nx\nfi\n";
        let out = format_text(src);
        assert!(out.contains("true  # why\n\tthen"), "got: {out:?}");
        // And the result must still parse the same as the input.
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
