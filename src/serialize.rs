use std::rc::Rc;
// Writes parsed AST back out as bish source: the function table and
// command handed to a self-exec'd child (see exec.rs), `declare -f`,
// `$BASH_COMMAND`, and quoting for builtins that print shell input. Not
// a pretty-printer; it has to round-trip.

use crate::lexer::{Chunk, Quoting, ReplaceAnchor, TransformKind, VarOp};
use crate::parser::{AndOr, ArrayLiteralItem, AssignMode, Combinator, Command, ListItem, Pipeline, Redirect, Sep, SimpleCommand, Word};

pub fn serialize_program(prog: &[ListItem]) -> String {
    let mut s = String::new();
    // Statements go back on the lines they were written on, so `$LINENO`
    // reads the same after a round trip: `PS4='+$LINENO '; set -x; echo t`
    // is three statements on line 1, not lines 1, 2, 3.
    //
    // Relative to the first statement, so a nested program (a function or
    // loop body) keeps its own spacing wherever it starts.
    let base = prog.iter().map(|i| i.line).find(|l| *l > 0);
    let mut at = base.unwrap_or(0);
    for item in prog {
        // Blank lines to reach this statement's line. A synthetic item
        // (`declare -f`'s body, the function preamble) has no line and
        // follows the one before.
        if let (Some(base), true) = (base, item.line > 0) {
            let _ = base;
            while item.line > at {
                s.push('\n');
                at += 1;
            }
        }
        let (line, bodies) = serialize_and_or_parts(&item.and_or);
        at += line.matches('\n').count();
        s.push_str(&line);
        // A heredoc body starts on the next line, so the separator can't
        // share this one; the body's newline stands in for a `;`.
        if !bodies.is_empty() {
            s.push('\n');
            at += 1;
            for b in bodies {
                at += b.matches('\n').count();
                s.push_str(&b);
            }
            continue;
        }
        // The separator only; a following newline is the next statement's
        // business, and the last needs none.
        s.push_str(match item.sep {
            Sep::Seq => "; ",
            Sep::Background => "& ",
        });
    }
    s
}

fn serialize_and_or_parts(ao: &AndOr) -> (String, Vec<String>) {
    let (mut s, mut bodies) = serialize_pipeline_parts(&ao.first);
    for (comb, p) in &ao.rest {
        s.push_str(match comb {
            Combinator::And => " && ",
            Combinator::Or => " || ",
        });
        let (text, more) = serialize_pipeline_parts(p);
        s.push_str(&text);
        bodies.extend(more);
    }
    (s, bodies)
}

fn serialize_pipeline_parts(p: &Pipeline) -> (String, Vec<String>) {
    let mut s = String::new();
    // `time` is a reserved word before the whole pipeline, not a command in
    // it, so it's written here.
    match p.timed {
        Some(crate::parser::TimeStyle::Shell) => s.push_str("time "),
        Some(crate::parser::TimeStyle::Posix) => s.push_str("time -p "),
        None => {}
    }
    if p.negate {
        s.push_str("! ");
    }
    // A stage's heredoc body goes between it and the `|`, as bash writes it:
    // `cat <<E |`, the body, then the next stage.
    let mut trailing: Vec<String> = Vec::new();
    for (i, c) in p.commands.iter().enumerate() {
        let (text, bodies) = serialize_command_parts(c);
        if i > 0 {
            match trailing.is_empty() {
                true => s.push_str(" | "),
                false => {
                    s.push_str(" |\n");
                    for b in trailing.drain(..) {
                        s.push_str(&b);
                    }
                }
            }
        }
        s.push_str(&text);
        trailing.extend(bodies);
    }
    (s, trailing)
}

/// A compound's redirects, written after its closing keyword, so
/// `{ cmd; } 2>e` keeps its redirect.
fn with_redirects(mut s: String, redirects: &[Redirect]) -> String {
    for r in redirects {
        s.push(' ');
        s.push_str(&serialize_redirect(r));
    }
    s
}

pub fn serialize_command(cmd: &Command) -> String {
    let (line, bodies) = serialize_command_parts(cmd);
    match bodies.is_empty() {
        true => line,
        false => format!("{}\n{}", line, bodies.concat()),
    }
}

/// `serialize_command`, with heredoc bodies kept apart so a caller
/// building a line can put them after it: a pipeline's `|` goes before
/// the body.
fn serialize_command_parts(cmd: &Command) -> (String, Vec<String>) {
    match cmd {
        Command::Simple(sc) => serialize_simple_with_heredocs(sc),
        // The body's heredoc bodies come back separately, so what follows the
        // definition goes after them; glued in, a `; next` landed after the
        // delimiter, a syntax error.
        Command::FuncDef { name, body } => {
            let (line, bodies) = serialize_command_parts(body);
            (format!("{}() {}", name, line), bodies)
        }
        other => (serialize_command_inner(other, true), heredoc_bodies(compound_redirects(other))),
    }
}

/// A compound command's redirects (after its `}`, `done` or `fi`), or
/// none.
fn compound_redirects(cmd: &Command) -> &[Redirect] {
    match cmd {
        Command::If { redirects, .. }
        | Command::While { redirects, .. }
        | Command::For { redirects, .. }
        | Command::CFor { redirects, .. }
        | Command::Select { redirects, .. }
        | Command::Case { redirects, .. }
        | Command::Group(_, redirects)
        | Command::Subshell(_, redirects)
        | Command::Arith(_, redirects)
        | Command::Test(_, redirects) => redirects,
        _ => &[],
    }
}

/// The bodies of whichever `redirects` are heredocs, in order, so
/// `{ cat; } <<EOF` keeps its document.
fn heredoc_bodies(redirects: &[Redirect]) -> Vec<String> {
    redirects
        .iter()
        .filter_map(|r| match r {
            Redirect::HereDoc(w, spelling) | Redirect::FdHereDoc { body: w, spelling, .. } => Some(heredoc_body(w, spelling)),
            _ => None,
        })
        .collect()
}

/// The same without the compound's own trailing redirects, for a caller
/// applying those itself. `run_compound_redirected` sets them up on a
/// self-exec'd child and hands it the compound as source; with the
/// redirects included, the child would take the same path and spawn
/// another child forever. Only the top level is stripped: redirects in
/// the body belong to commands the child runs.
pub fn serialize_command_body(cmd: &Command) -> String {
    serialize_command_inner(cmd, false)
}

fn serialize_command_inner(cmd: &Command, own_redirects: bool) -> String {
    let with_redirects = |s: String, r: &[Redirect]| if own_redirects { with_redirects(s, r) } else { s };
    match cmd {
        Command::Simple(sc) => serialize_simple(sc),
        Command::If { branches, else_branch, redirects } => {
            let mut s = String::new();
            for (i, (cond, body)) in branches.iter().enumerate() {
                s.push_str(if i == 0 { "if " } else { "elif " });
                s.push_str(&serialize_program(cond));
                s.push_str("then\n");
                s.push_str(&serialize_program(body));
            }
            if let Some(e) = else_branch {
                s.push_str("else\n");
                s.push_str(&serialize_program(e));
            }
            s.push_str("fi");
            with_redirects(s, redirects)
        }
        Command::While { cond, body, until, redirects } => {
            let mut s = String::new();
            s.push_str(if *until { "until " } else { "while " });
            s.push_str(&serialize_program(cond));
            s.push_str("do\n");
            s.push_str(&serialize_program(body));
            s.push_str("done");
            with_redirects(s, redirects)
        }
        Command::For { var, words, body, redirects } => {
            let mut s = format!("for {} ", var);
            if let Some(words) = words {
                s.push_str("in ");
                s.push_str(&words.iter().map(serialize_word).collect::<Vec<_>>().join(" "));
                s.push('\n');
            }
            s.push_str("do\n");
            s.push_str(&serialize_program(body));
            s.push_str("done");
            with_redirects(s, redirects)
        }
        Command::CFor { init, cond, step, body, redirects } => {
            let mut s = format!("for (({}; {}; {}))\n", init, cond, step);
            s.push_str("do\n");
            s.push_str(&serialize_program(body));
            s.push_str("done");
            with_redirects(s, redirects)
        }
        Command::Select { var, words, body, redirects } => {
            let mut s = format!("select {} ", var);
            if let Some(words) = words {
                s.push_str("in ");
                s.push_str(&words.iter().map(serialize_word).collect::<Vec<_>>().join(" "));
                s.push('\n');
            }
            s.push_str("do\n");
            s.push_str(&serialize_program(body));
            s.push_str("done");
            with_redirects(s, redirects)
        }
        Command::Case { word, arms, redirects } => {
            let mut s = format!("case {} in\n", serialize_word(word));
            for (patterns, body, term) in arms {
                s.push_str(&patterns.iter().map(serialize_word).collect::<Vec<_>>().join("|"));
                s.push_str(")\n");
                s.push_str(&serialize_program(body));
                s.push_str(match term {
                    crate::parser::CaseTerm::Stop => ";;\n",
                    crate::parser::CaseTerm::FallThrough => ";&\n",
                    crate::parser::CaseTerm::Continue => ";;&\n",
                });
            }
            s.push_str("esac");
            with_redirects(s, redirects)
        }
        Command::Group(prog, redirects) => with_redirects(format!("{{\n{}}}", serialize_program(prog)), redirects),
        Command::FuncDef { name, body } => format!("{}() {}", name, serialize_command(body)),
        Command::Subshell(raw, redirects) => with_redirects(format!("({})", raw), redirects),
        Command::Arith(raw, redirects) => with_redirects(format!("(({}))", raw), redirects),
        Command::Test(atoms, redirects) => with_redirects(format!("[[ {} ]]", serialize_test_atoms(atoms)), redirects),
        Command::Coproc { name, body } => match name {
            Some(n) => format!("coproc {} {}", n, serialize_command(body)),
            None => format!("coproc {}", serialize_command(body)),
        },
    }
}

fn serialize_test_atoms(atoms: &[crate::parser::TestAtom]) -> String {
    use crate::parser::TestAtom;
    atoms
        .iter()
        .map(|a| match a {
            TestAtom::Word(w) => serialize_word(w),
            TestAtom::And => "&&".to_string(),
            TestAtom::Or => "||".to_string(),
            TestAtom::Not => "!".to_string(),
            TestAtom::Group(g) => format!("( {} )", serialize_test_atoms(g)),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `serialize_simple`, memoised on the command. Every simple command
/// sets `$BASH_COMMAND` before running, and in a loop that's the same
/// answer for the same node, so it's computed once and shared.
pub(crate) fn rendered_command(sc: &SimpleCommand) -> Rc<str> {
    Rc::clone(sc.rendered.get_or_init(|| Rc::from(serialize_simple(sc).into_boxed_str())))
}

pub(crate) fn serialize_simple(sc: &SimpleCommand) -> String {
    let (line, bodies) = serialize_simple_with_heredocs(sc);
    // Inline, for callers wanting one command's text with nowhere for a
    // body (`$BASH_COMMAND`, a job's name); a command followed by its
    // heredoc is still valid source.
    match bodies.is_empty() {
        true => line,
        false => format!("{}\n{}", line, bodies.concat()),
    }
}

/// The command's text and the heredoc bodies following its line; see
/// `format_simple`.
fn serialize_simple_with_heredocs(sc: &SimpleCommand) -> (String, Vec<String>) {
    let mut parts = serialize_simple_parts(sc);
    let mut bodies = Vec::new();
    for r in &sc.redirects {
        parts.push(serialize_redirect(r));
        if let Redirect::HereDoc(w, spelling) | Redirect::FdHereDoc { body: w, spelling, .. } = r {
            bodies.push(heredoc_body(w, spelling));
        }
    }
    (parts.join(" "), bodies)
}

/// A simple command without its redirects: assignments, then words,
/// with a declare-family array literal spliced back at its recorded
/// position. Separate because `declare -f` writes redirects its own way
/// (see `format_redirect`).
fn serialize_simple_parts(sc: &SimpleCommand) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for (name, mode, val) in &sc.assigns {
        let op = if *mode == AssignMode::Append { "+=" } else { "=" };
        parts.push(format!("{}{}{}", name, op, serialize_word(val)));
    }
    for (name, mode, items) in &sc.array_assigns {
        parts.push(serialize_array_literal_assign(name, *mode, items));
    }
    for (name, index, mode, val) in &sc.index_assigns {
        let op = if *mode == AssignMode::Append { "+=" } else { "=" };
        parts.push(format!("{}[{}]{}{}", name, index, op, serialize_word(val)));
    }
    // A later-word declare-family array literal (`declare -A m=([a]=1)`)
    // has no placeholder in `sc.words`; it goes back at its recorded
    // position (see `SimpleCommand::array_word_assigns`).
    let mut pending = sc.array_word_assigns.iter().peekable();
    for (i, w) in sc.words.iter().enumerate() {
        while let Some((pos, name, mode, items)) = pending.peek() {
            if *pos != i {
                break;
            }
            parts.push(serialize_array_literal_assign(name, *mode, items));
            pending.next();
        }
        parts.push(serialize_word(w));
    }
    for (_, name, mode, items) in pending {
        parts.push(serialize_array_literal_assign(name, *mode, items));
    }
    parts
}

fn serialize_array_literal_assign(name: &str, mode: AssignMode, items: &[ArrayLiteralItem]) -> String {
    let op = if mode == AssignMode::Append { "+=" } else { "=" };
    let words: Vec<String> = items
        .iter()
        .map(|item| match item {
            ArrayLiteralItem::Positional(w) => serialize_word(w),
            ArrayLiteralItem::Keyed(index, w) => format!("[{}]={}", index, serialize_word(w)),
        })
        .collect();
    format!("{}{}({})", name, op, words.join(" "))
}

// `>`, `>>` or `>|`. The last exists to defeat `set -C`, so it must
// survive.
fn redirect_op(append: bool, clobber: bool) -> &'static str {
    match (append, clobber) {
        (true, _) => ">>",
        (false, true) => ">|",
        (false, false) => ">",
    }
}

/// The word after a redirect operator, always spaced from it. Joined,
/// `wc -l < <(cmd)` becomes `wc -l <<(cmd)`, a heredoc, and `cmd > >(cat)`
/// an append to a file named `(cat)`, both still valid scripts. Always
/// spaced rather than only on collision, because bash prints it so
/// (`echo z > f`) and `$BASH_COMMAND` should match.
fn redirect_target(w: &Word) -> String {
    format!(" {}", serialize_word(w))
}

/// The operator a dup was written with; the same dup2, written back as
/// it was.
fn dup_op(input: bool) -> &'static str {
    if input { "<" } else { ">" }
}

/// The descriptor before a dup onto a variable's descriptor, as bash
/// writes it: left out when it's the operator's default (`<&$fd`, not
/// `0<&$fd`).
fn dup_word_fd(fd: u32, input: bool) -> String {
    if fd == u32::from(!input) { String::new() } else { fd.to_string() }
}

pub fn serialize_redirect(r: &Redirect) -> String {
    match r {
        Redirect::In(w) => format!("<{}", redirect_target(w)),
        Redirect::InOut(w) => format!("<>{}", redirect_target(w)),
        Redirect::Out { word, append, clobber } => format!("{}{}", redirect_op(*append, *clobber), redirect_target(word)),
        Redirect::Err { word, append, clobber } => format!("2{}{}", redirect_op(*append, *clobber), redirect_target(word)),
        Redirect::Both { word, append } => format!("&{}{}", if *append { ">>" } else { ">" }, redirect_target(word)),
        Redirect::DupErrToOut => "2>&1".to_string(),
        Redirect::HereString(w) => format!("<<<{}", redirect_target(w)),
        // A real heredoc, not an equivalent here-string: both give the same
        // content, but `declare -f` prints what a round trip leaves.
        Redirect::HereDoc(_, spelling) => {
            let dash = if spelling.strip_tabs { "-" } else { "" };
            let delim = if spelling.quoted { format!("'{}'", spelling.delimiter) } else { spelling.delimiter.clone() };
            format!("<<{}{}", dash, delim)
        }
        Redirect::FdHereString { fd, word } => format!("{fd}<<<{}", redirect_target(word)),
        Redirect::FdHereDoc { fd, spelling, .. } => {
            let dash = if spelling.strip_tabs { "-" } else { "" };
            let delim = if spelling.quoted { format!("'{}'", spelling.delimiter) } else { spelling.delimiter.clone() };
            format!("{fd}<<{}{}", dash, delim)
        }
        Redirect::VarFd { var, kind, word } => {
            let op = match kind {
                crate::lexer::VarFdKind::In => "<".to_string(),
                crate::lexer::VarFdKind::InOut => "<>".to_string(),
                crate::lexer::VarFdKind::Out { append, clobber } => redirect_op(*append, *clobber).to_string(),
                crate::lexer::VarFdKind::Dup => ">&".to_string(),
            };
            format!("{{{}}}{}{}", var, op, redirect_target(word))
        }
        Redirect::FdOut { fd, word, append, clobber } => {
            format!("{}{}{}", fd, redirect_op(*append, *clobber), redirect_target(word))
        }
        Redirect::FdIn { fd, word } => format!("{}<{}", fd, redirect_target(word)),
        Redirect::FdInOut { fd, word } => format!("{}<>{}", fd, redirect_target(word)),
        Redirect::FdDup { fd, target, input } => format!("{}{}&{}", fd, dup_op(*input), target),
        Redirect::FdDupWord { fd, word, input } => format!("{}{}&{}", dup_word_fd(*fd, *input), dup_op(*input), redirect_target(word)),
        Redirect::FdClose { fd } => format!("{}>&-", fd),
    }
}

/// Whether a chunk was inside double quotes: a literal run knows
/// directly, an expansion through `quoted`. A run of them is one pair of
/// quotes and is written back as one (`"a$x b"`, not `'a'"$x"' b'`).
fn is_double_quoted(c: &Chunk) -> bool {
    match c {
        Chunk::LiteralStr(_, Quoting::Double) => true,
        Chunk::Str(_) | Chunk::LiteralStr(..) | Chunk::Tilde { .. } => false,
        Chunk::Var { quoted, .. }
        | Chunk::Sub { quoted, .. }
        | Chunk::Arith { quoted, .. }
        | Chunk::VarExpand { quoted, .. }
        | Chunk::ArrayVar { quoted, .. }
        | Chunk::ArrayVarExpand { quoted, .. }
        | Chunk::Indirect { quoted, .. }
        | Chunk::ArrayKeys { quoted, .. }
        | Chunk::VarNamesMatchingPrefix { quoted, .. } => *quoted,
        _ => false,
    }
}

/// A chunk's text inside `"..."`: a literal keeps its characters, with
/// only the four special there escaped.
fn inside_double_quotes(c: &Chunk) -> String {
    match c {
        Chunk::LiteralStr(t, _) => t.chars().flat_map(escaped_in_double_quotes).collect(),
        // An expansion writes itself the same; the run's quotes make it
        // quoted.
        other => serialize_chunk_unquoted(other),
    }
}

/// The four characters special inside `"..."`, the only ones needing a
/// backslash.
fn escaped_in_double_quotes(c: char) -> Vec<char> {
    match c {
        '"' | '\\' | '$' | '`' => vec!['\\', c],
        c => vec![c],
    }
}

pub(crate) fn serialize_word(w: &Word) -> String {
    let mut text = String::new();
    let mut at = 0;
    while at < w.chunks.len() {
        if is_double_quoted(&w.chunks[at]) {
            // The whole run, back into its one pair of quotes.
            let end = w.chunks[at..].iter().position(|c| !is_double_quoted(c)).map_or(w.chunks.len(), |n| at + n);
            text.push('"');
            for c in &w.chunks[at..end] {
                text.push_str(&inside_double_quotes(c));
            }
            text.push('"');
            at = end;
            continue;
        }
        text.push_str(&serialize_chunk(&w.chunks[at]));
        at += 1;
    }
    // A word that writes as nothing must be `''`, or it stops being a word:
    // `f ""` would lose an argument. Empty chunk lists and an empty
    // `Chunk::Str("")` (some assignments' tails) are the same case.
    match text.is_empty() {
        true => "''".to_string(),
        false => text,
    }
}

fn serialize_chunk(c: &Chunk) -> String {
    serialize_chunk_inner(c, true)
}

/// The same, for a chunk inside double quotes: the run has already
/// opened the quotes an expansion would put around itself.
fn serialize_chunk_unquoted(c: &Chunk) -> String {
    serialize_chunk_inner(c, false)
}

fn serialize_chunk_inner(c: &Chunk, wrap: bool) -> String {
    let wrap_quoted = |s: String, quoted: bool| if wrap { wrap_quoted(s, quoted) } else { s };
    match c {
        // As written; `~` is only a tilde prefix at a word's start.
        Chunk::Tilde { name } => format!("~{}", name),
        // `Str` is text written unquoted and `LiteralStr` text that was
        // quoted or escaped, and they must stay apart: quoting both made
        // `echo *` print an asterisk, `case $x in a*)` match only `a*`, and
        // `[[ x < y ]]` lose its operator.
        //
        // `Str` is safe verbatim because the lexer already separated
        // anything escaped into `LiteralStr`: `echo a\ b` lexes to
        // `[Str("a"), LiteralStr(" "), Str("b")]`, `echo \*` to
        // `[LiteralStr("*")]`, `echo *` to `[Str("*")]`. What's in `Str`
        // reads back as itself.
        Chunk::Str(s) => s.clone(),
        // In the spelling it was written in. `Double` arrives here only
        // alone; `serialize_word` gathers longer runs.
        Chunk::LiteralStr(s, quoting) => match quoting {
            Quoting::Single => quote_literal(s),
            Quoting::Double => format!("\"{}\"", s.chars().flat_map(escaped_in_double_quotes).collect::<String>()),
            // A backslash per character, as the lexer makes a chunk per
            // escape.
            Quoting::Escape => s.chars().map(|c| format!("\\{c}")).collect(),
            // bash doesn't print `$'...'` back: it prints the resolved text
            // as an ordinary literal, so `$'a\tb'` comes back as
            // `'a<tab>b'`.
            Quoting::Dollar => quote_literal(s),
        },
        // As written: `$x` braced only if the source braced it. They expand
        // the same; `declare -f` shows the difference.
        Chunk::Var { name, quoted, braced } => {
            let text = if *braced { format!("${{{}}}", name) } else { format!("${}", name) };
            wrap_quoted(text, *quoted)
        }
        Chunk::Sub { raw, quoted, backticks } => {
            let text = if *backticks { format!("`{}`", raw) } else { format!("$({})", raw) };
            wrap_quoted(text, *quoted)
        }
        Chunk::Arith { raw, quoted } => wrap_quoted(format!("$(({}))", raw), *quoted),
        Chunk::VarExpand { name, op, quoted } => wrap_quoted(serialize_var_op(name, op), *quoted),
        Chunk::ArrayVar { name, index, quoted } => wrap_quoted(format!("${{{}[{}]}}", name, index), *quoted),
        Chunk::ArrayLength { name, index } => format!("${{#{}[{}]}}", name, index),
        Chunk::ArrayVarExpand { name, index, op, quoted } => wrap_quoted(serialize_array_var_op(name, index, op), *quoted),
        Chunk::Indirect { name, quoted } => wrap_quoted(format!("${{!{}}}", name), *quoted),
        Chunk::ArrayKeys { name, quoted } => wrap_quoted(format!("${{!{}[@]}}", name), *quoted),
        Chunk::VarNamesMatchingPrefix { prefix, at, quoted } => wrap_quoted(format!("${{!{}{}}}", prefix, if *at { "@" } else { "*" }), *quoted),
        Chunk::ProcSubIn { raw } => format!("<({})", raw),
        Chunk::ProcSubOut { raw } => format!(">({})", raw),
    }
}

// Preserves quoted-ness across the preamble round-trip (see
// exec.rs::functions_preamble) so a re-parsed expansion keeps the same
// word-splitting eligibility it had in the original source.
fn wrap_quoted(s: String, quoted: bool) -> String {
    if quoted { format!("\"{}\"", s) } else { s }
}

pub fn quote_literal(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn serialize_var_op(name: &str, op: &VarOp) -> String {
    match op {
        VarOp::Length => format!("${{#{}}}", name),
        VarOp::Default { word, colon } => format!("${{{}{}-{}}}", name, if *colon { ":" } else { "" }, word),
        VarOp::AssignDefault { word, colon } => format!("${{{}{}={}}}", name, if *colon { ":" } else { "" }, word),
        VarOp::ErrorIfUnset { word, colon } => format!("${{{}{}?{}}}", name, if *colon { ":" } else { "" }, word),
        VarOp::AltIfSet { word, colon } => format!("${{{}{}+{}}}", name, if *colon { ":" } else { "" }, word),
        VarOp::RemovePrefix { pattern, longest } => {
            format!("${{{}{}{}}}", name, if *longest { "##" } else { "#" }, pattern)
        }
        VarOp::RemoveSuffix { pattern, longest } => {
            format!("${{{}{}{}}}", name, if *longest { "%%" } else { "%" }, pattern)
        }
        VarOp::CaseConvert { pattern, upper, all } => {
            let op = match (*upper, *all) {
                (true, true) => "^^",
                (true, false) => "^",
                (false, true) => ",,",
                (false, false) => ",",
            };
            format!("${{{}{}{}}}", name, op, pattern)
        }
        VarOp::Substring { offset, length } => match length {
            Some(l) => format!("${{{}:{}:{}}}", name, offset, l),
            None => format!("${{{}:{}}}", name, offset),
        },
        VarOp::Replace { pattern, repl, global, anchor } => {
            let slashes = if *global { "//" } else { "/" };
            let anchor_ch = match anchor {
                ReplaceAnchor::None => "",
                ReplaceAnchor::Start => "#",
                ReplaceAnchor::End => "%",
            };
            format!("${{{}{}{}{}/{}}}", name, slashes, anchor_ch, pattern, repl)
        }
        VarOp::Transform(kind) => {
            let letter = match kind {
                TransformKind::Quote => "Q",
                TransformKind::Upper => "U",
                TransformKind::UpperFirst => "u",
                TransformKind::Lower => "L",
                TransformKind::Escape => "E",
                TransformKind::Attributes => "A",
                TransformKind::AttributeFlags => "a",
                TransformKind::KeyValue => "K",
                TransformKind::Prompt => "P",
            };
            format!("${{{}@{}}}", name, letter)
        }
    }
}

fn serialize_array_var_op(name: &str, index: &str, op: &VarOp) -> String {
    let full = format!("{}[{}]", name, index);
    serialize_var_op(&full, op)
}

// ---------------------------------------------------------------------
// `declare -f`
// ---------------------------------------------------------------------
//
// A second way to write a command out. `serialize_*` only has to mean
// the same to a child shell; `declare -f` output is read by people,
// completion scripts and `sh -c "$(declare -f f); f"`, and bash's shape
// for it is specific enough that anything else looks foreign.
//
// The shape, as bash prints it:
//
//   - `f () ` and `{ ` each keep a trailing space, on their own lines.
//   - Four spaces per level of nesting.
//   - Every statement ends in `;` except the last before a `}` or `;;`;
//     before `fi`, `done`, `else` and `esac` it keeps its `;`.
//   - `if C; then` and `while C; do` keep the keyword on the condition's
//     line; `for` and `select` put `do` on its own line.
//   - `elif` prints as a nested `else`/`if`.
//   - `case W in ` has a trailing space; arms sit one level in, bodies
//     two, with `;;` at the arm's level.
//   - A redirect's target is spaced (`> f`, `2> f`, `&> f`), a descriptor
//     isn't (`2>&1`), and `>&2` is written `1>&2`.
//   - A function body is always a brace group, even if written as
//     `f() ( ... )`.

fn pad(indent: usize) -> String {
    " ".repeat(indent * 4)
}

/// One function, in bash's `declare -f` shape, with no trailing newline.
pub fn format_function(name: &str, body: &Command) -> String {
    format_function_at(name, body, 0, false)
}

/// `nested` is bash's asymmetry: a function defined inside another is
/// printed with the `function` keyword, the outer one isn't.
fn format_function_at(name: &str, body: &Command, indent: usize, nested: bool) -> String {
    let here = pad(indent);
    let keyword = if nested { "function " } else { "" };
    let mut out = format!("{}{} () \n{}{{ \n", keyword, name, here);
    // Whatever the body was written as, it prints as a brace group:
    // `f() ( echo x )` comes back as a group containing the subshell.
    match body {
        Command::Group(prog, redirects) => {
            format_list(prog, indent + 1, false, &mut out);
            out.push_str(&here);
            out.push('}');
            for r in redirects {
                out.push(' ');
                out.push_str(&format_redirect(r));
            }
            // The function's heredocs, whose bodies follow its `}` line.
            // The last keeps its newline, which with the one ending every
            // function is the blank line bash leaves after a heredoc.
            let bodies = heredoc_bodies(redirects);
            if !bodies.is_empty() {
                out.push('\n');
                out.push_str(&bodies.concat());
            }
        }
        other => {
            let (text, bodies) = format_command(other, indent + 1);
            out.push_str(&pad(indent + 1));
            out.push_str(&text);
            out.push('\n');
            if !bodies.is_empty() {
                for b in bodies {
                    out.push_str(&b);
                }
                out.push('\n');
            }
            out.push_str(&here);
            out.push('}');
        }
    }
    out
}

/// A program, one statement per line, each indented. `semi_on_last`
/// says whether the final statement keeps its separator: not before a
/// `}` or `;;`, which stand alone, but before `fi`, `done`, `else` or
/// `esac`.
fn format_list(prog: &[ListItem], indent: usize, semi_on_last: bool, out: &mut String) {
    // `&` doesn't end the line as `;` does: bash writes `echo a & echo b` on
    // one.
    let mut at_line_start = true;
    for (i, item) in prog.iter().enumerate() {
        if at_line_start {
            out.push_str(&pad(indent));
        }
        let (text, bodies) = format_and_or(&item.and_or, indent);
        out.push_str(&text);
        let last = i + 1 == prog.len();
        at_line_start = true;
        // A heredoc body starts on the next line, so that line carries no
        // `;`, as in bash. The blank line after the delimiter is the
        // body's.
        if !bodies.is_empty() {
            out.push('\n');
            for b in bodies {
                out.push_str(&b);
            }
            // The blank line ending a line that carries heredocs.
            out.push('\n');
            continue;
        }
        match item.sep {
            Sep::Background if last => out.push_str(" &\n"),
            Sep::Background => {
                out.push_str(" & ");
                at_line_start = false;
            }
            Sep::Seq if !last || semi_on_last => out.push_str(";\n"),
            Sep::Seq => out.push('\n'),
        }
    }
}

/// A program on one line, where bash puts one: an `if`/`while`
/// condition, or a subshell's body. A multi-statement condition wraps at
/// the enclosing indent: `if a;\n    b; then`.
fn format_list_inline(prog: &[ListItem], indent: usize) -> String {
    let mut out = String::new();
    for (i, item) in prog.iter().enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(&pad(indent));
        }
        let (text, bodies) = format_and_or(&item.and_or, indent);
        out.push_str(&text);
        for b in bodies {
            out.push('\n');
            out.push_str(&b);
        }
        if i + 1 < prog.len() {
            out.push_str(match item.sep {
                Sep::Background => " &",
                Sep::Seq => ";",
            });
        }
    }
    out
}

fn format_and_or(ao: &AndOr, indent: usize) -> (String, Vec<String>) {
    let (mut s, mut bodies) = format_pipeline(&ao.first, indent);
    for (comb, p) in &ao.rest {
        s.push_str(match comb {
            Combinator::And => " && ",
            Combinator::Or => " || ",
        });
        let (text, more) = format_pipeline(p, indent);
        s.push_str(&text);
        bodies.extend(more);
    }
    (s, bodies)
}

fn format_pipeline(p: &Pipeline, indent: usize) -> (String, Vec<String>) {
    let mut s = String::new();
    match p.timed {
        Some(crate::parser::TimeStyle::Shell) => s.push_str("time "),
        Some(crate::parser::TimeStyle::Posix) => s.push_str("time -p "),
        None => {}
    }
    if p.negate {
        s.push_str("! ");
    }
    let mut trailing: Vec<String> = Vec::new();
    for (i, c) in p.commands.iter().enumerate() {
        let (text, bodies) = format_command(c, indent);
        if i > 0 {
            // A stage after one that opened a heredoc starts its own line,
            // as the body goes between them. Two spaces, as bash does, not
            // this block's indent.
            match trailing.is_empty() {
                true => s.push_str(" | "),
                false => {
                    s.push_str(" |\n");
                    for b in trailing.drain(..) {
                        s.push_str(&b);
                    }
                    s.push_str("  ");
                }
            }
        }
        s.push_str(&text);
        trailing.extend(bodies);
    }
    (s, trailing)
}

fn format_command(cmd: &Command, indent: usize) -> (String, Vec<String>) {
    let inner = pad(indent + 1);
    let here = pad(indent);
    // A compound's text and its own heredoc bodies, which follow the line it
    // ends on, as a simple command's do.
    let plain = |s: String| (s, heredoc_bodies(compound_redirects(cmd)));
    match cmd {
        Command::Simple(sc) => format_simple(sc),
        // bash prints `elif` as the first `if`'s `else` containing a nested
        // `if`, and so does this.
        Command::If { branches, else_branch, redirects } => {
            let mut s = String::new();
            for (i, (cond, body)) in branches.iter().enumerate() {
                let at = indent + i;
                if i > 0 {
                    s.push_str(&pad(at - 1));
                    s.push_str("else\n");
                    s.push_str(&pad(at));
                }
                s.push_str("if ");
                s.push_str(&format_list_inline(cond, at));
                s.push_str("; then\n");
                format_list(body, at + 1, true, &mut s);
            }
            let deepest = indent + branches.len() - 1;
            if let Some(e) = else_branch {
                s.push_str(&pad(deepest));
                s.push_str("else\n");
                format_list(e, deepest + 1, true, &mut s);
            }
            // Closed inside out, each `fi` but the outermost ending a
            // statement the `else` above continues.
            for depth in (indent..=deepest).rev() {
                s.push_str(&pad(depth));
                s.push_str("fi");
                if depth > indent {
                    s.push_str(";\n");
                }
            }
            plain(with_formatted_redirects(s, redirects))
        }
        Command::While { cond, body, until, redirects } => {
            let mut s = String::new();
            s.push_str(if *until { "until " } else { "while " });
            s.push_str(&format_list_inline(cond, indent));
            s.push_str("; do\n");
            format_list(body, indent + 1, true, &mut s);
            s.push_str(&here);
            s.push_str("done");
            plain(with_formatted_redirects(s, redirects))
        }
        Command::For { var, words, body, redirects } => {
            // `for x` with no list iterates the positional parameters, and
            // bash prints the list it stands for.
            let items = match words {
                Some(words) => words.iter().map(serialize_word).collect::<Vec<_>>().join(" "),
                None => "\"$@\"".to_string(),
            };
            let mut s = format!("for {} in {};\n{}do\n", var, items, here);
            format_list(body, indent + 1, true, &mut s);
            s.push_str(&here);
            s.push_str("done");
            plain(with_formatted_redirects(s, redirects))
        }
        Command::CFor { init, cond, step, body, redirects } => {
            let mut s = format!("for (({}; {}; {}))\n{}do\n", init.trim(), cond.trim(), step.trim(), here);
            format_list(body, indent + 1, true, &mut s);
            s.push_str(&here);
            s.push_str("done");
            plain(with_formatted_redirects(s, redirects))
        }
        Command::Select { var, words, body, redirects } => {
            let items = match words {
                Some(words) => words.iter().map(serialize_word).collect::<Vec<_>>().join(" "),
                None => "\"$@\"".to_string(),
            };
            let mut s = format!("select {} in {};\n{}do\n", var, items, here);
            format_list(body, indent + 1, true, &mut s);
            s.push_str(&here);
            s.push_str("done");
            plain(with_formatted_redirects(s, redirects))
        }
        Command::Case { word, arms, redirects } => {
            let mut s = format!("case {} in \n", serialize_word(word));
            for (patterns, body, term) in arms {
                s.push_str(&inner);
                s.push_str(&patterns.iter().map(serialize_word).collect::<Vec<_>>().join(" | "));
                s.push_str(")\n");
                format_list(body, indent + 2, false, &mut s);
                s.push_str(&inner);
                s.push_str(match term {
                    crate::parser::CaseTerm::Stop => ";;\n",
                    crate::parser::CaseTerm::FallThrough => ";&\n",
                    crate::parser::CaseTerm::Continue => ";;&\n",
                });
            }
            s.push_str(&here);
            s.push_str("esac");
            plain(with_formatted_redirects(s, redirects))
        }
        Command::Group(prog, redirects) => {
            let mut s = String::from("{ \n");
            format_list(prog, indent + 1, false, &mut s);
            s.push_str(&here);
            s.push('}');
            plain(with_formatted_redirects(s, redirects))
        }
        // Held as source text (see `Command::Subshell`), so parsed here for
        // layout. A body that won't parse is written as it stands.
        Command::Subshell(raw, redirects) => {
            let s = match reparse(raw) {
                Some(prog) => format!("( {} )", format_list_inline(&prog, indent)),
                None => format!("( {} )", raw.trim()),
            };
            plain(with_formatted_redirects(s, redirects))
        }
        // Verbatim, spaces and all, as bash keeps it: `((n++))` and
        // `(( n++ ))` each come back as themselves.
        Command::Arith(raw, redirects) => plain(with_formatted_redirects(format!("(({}))", raw), redirects)),
        Command::Test(atoms, redirects) => plain(with_formatted_redirects(format!("[[ {} ]]", serialize_test_atoms(atoms)), redirects)),
        Command::FuncDef { name, body } => plain(format_function_at(name, body, indent, true)),
        Command::Coproc { name, body } => {
            let (text, bodies) = format_command(body, indent);
            let line = match name {
                Some(n) => format!("coproc {} {}", n, text),
                None => format!("coproc {}", text),
            };
            (line, bodies)
        }
    }
}

fn reparse(raw: &str) -> Option<Vec<ListItem>> {
    let tokens = crate::lexer::Lexer::new(raw).tokenize().ok()?;
    crate::parser::Parser::new(tokens).parse_program().ok()
}

fn with_formatted_redirects(mut s: String, redirects: &[Redirect]) -> String {
    for r in redirects {
        s.push(' ');
        s.push_str(&format_redirect(r));
    }
    s
}

/// A heredoc as bash writes one: the body at column zero (indenting would
/// change it), then the delimiter. No blank line: bash puts one after
/// the last body on a line only, which the caller knows.
fn heredoc_body(w: &Word, spelling: &crate::lexer::HereDocSpelling) -> String {
    let body: String = w
        .chunks
        .iter()
        .map(|c| match c {
            Chunk::Str(t) | Chunk::LiteralStr(t, _) => t.clone(),
            other => serialize_chunk_unquoted(other),
        })
        .collect();
    format!("{}{}\n", body, spelling.delimiter)
}

/// The command's text and the heredoc bodies following its line, kept
/// apart because only the caller knows where the line ends: in a
/// pipeline the `|` comes first.
fn format_simple(sc: &SimpleCommand) -> (String, Vec<String>) {
    let mut parts: Vec<String> = serialize_simple_parts(sc);
    let mut bodies = Vec::new();
    for r in &sc.redirects {
        parts.push(format_redirect(r));
        if let Redirect::HereDoc(w, spelling) | Redirect::FdHereDoc { body: w, spelling, .. } = r {
            bodies.push(heredoc_body(w, spelling));
        }
    }
    (parts.join(" "), bodies)
}

/// A redirect as bash prints it: the operator, a space before a file and
/// none before a descriptor, and `>&2` as `1>&2`.
fn format_redirect(r: &Redirect) -> String {
    match r {
        Redirect::In(w) => format!("< {}", serialize_word(w)),
        Redirect::InOut(w) => format!("<> {}", serialize_word(w)),
        Redirect::Out { word, append, clobber } => format!("{} {}", redirect_op(*append, *clobber), serialize_word(word)),
        Redirect::Err { word, append, clobber } => format!("2{} {}", redirect_op(*append, *clobber), serialize_word(word)),
        Redirect::Both { word, append } => format!("&{} {}", if *append { ">>" } else { ">" }, serialize_word(word)),
        Redirect::DupErrToOut => "2>&1".to_string(),
        Redirect::FdOut { fd, word, append, clobber } => format!("{}{} {}", fd, redirect_op(*append, *clobber), serialize_word(word)),
        Redirect::FdIn { fd, word } => format!("{}< {}", fd, serialize_word(word)),
        Redirect::FdInOut { fd, word } => format!("{}<> {}", fd, serialize_word(word)),
        Redirect::FdDup { fd, target, input } => format!("{}{}&{}", fd, dup_op(*input), target),
        Redirect::FdDupWord { fd, word, input } => format!("{}{}&{}", dup_word_fd(*fd, *input), dup_op(*input), serialize_word(word)),
        Redirect::FdClose { fd } => format!("{}>&-", fd),
        // The heredoc as written: `<<-` and a quoted delimiter change what
        // the body means, so both survive.
        Redirect::HereDoc(_, spelling) => {
            let dash = if spelling.strip_tabs { "-" } else { "" };
            let delim = if spelling.quoted { format!("'{}'", spelling.delimiter) } else { spelling.delimiter.clone() };
            format!("<<{}{}", dash, delim)
        }
        // Nothing to normalise, or nothing bash's layout says about them.
        other => serialize_redirect(other),
    }
}

#[cfg(test)]
mod quoting_tests {
    use super::quote_literal;

    // Everything re-entering the shell as text goes through this:
    // `functions_preamble`, `$(...)` capture, `complete -W`'s words, `abbr`
    // listing itself back. Wrong for some byte, and a string stops being
    // data and becomes syntax. So the property is checked on the riskiest
    // bytes against two readers: bish's lexer and expansion, and bash.
    fn corpus() -> Vec<String> {
        let mut out: Vec<String> = [
            "",
            "plain",
            "'",
            "''",
            "'''",
            "a'b",
            "\\",
            "\\'",
            "'\\''",
            "\"",
            "$x",
            "${x}",
            "$(id)",
            "`id`",
            "$((1+1))",
            "!!",
            "!$",
            "~",
            "~root",
            "*",
            "?",
            "[a-z]",
            "{a,b}",
            "#comment",
            ";",
            "|",
            "&&",
            "<",
            ">",
            ">>",
            "\n",
            "a\nb",
            "\t",
            "  spaced  ",
            "-n",
            "--",
            "%s",
            "%",
            "\u{e4}\u{f6}\u{e5}",
            "\u{1b}[2J",
            "\u{7f}",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        // Plus seeded random strings over the same alphabet, so the corpus
        // isn't only the cases thought of.
        let alphabet: Vec<char> = "ab'\\\"$`(){}[]!~*?;|&<>#\n\t %-\u{e4}".chars().collect();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        for _ in 0..300 {
            let len = (next() % 13) as usize;
            out.push((0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect());
        }
        out
    }

    #[test]
    fn quote_literal_survives_bishs_own_lexer_and_expansion() {
        for original in corpus() {
            let script = format!("printf '%s' {}\n", quote_literal(&original));
            let mut sh = crate::exec::Shell::new();
            let out = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
            sh.set_sink_capture(out.clone());
            sh.run_source_here(&script, "<quote-literal>");
            let got = out.borrow().clone();
            assert_eq!(got, original, "quoted as {}", quote_literal(&original));
        }
    }

    // The same corpus through bash: bish agreeing with itself only shows its
    // lexer undoes its quoter; bash decides whether the quoting is right.
    #[test]
    fn quote_literal_survives_real_bash() {
        let available = std::process::Command::new("bash").arg("-c").arg(":").status().is_ok_and(|s| s.success());
        if !available {
            return;
        }
        for original in corpus() {
            let script = format!("printf '%s' {}", quote_literal(&original));
            let out = std::process::Command::new("bash").arg("-c").arg(&script).output().expect("bash");
            assert!(out.status.success(), "bash rejected {script:?}");
            let got = String::from_utf8_lossy(&out.stdout).into_owned();
            assert_eq!(got, original, "bash read {script:?} differently");
        }
    }

    // The output's shape: a single-quoted run with `'\''` for each quote,
    // the POSIX idiom, and the empty string still a word.
    #[test]
    fn quote_literal_produces_the_posix_idiom() {
        assert_eq!(quote_literal(""), "''", "an empty word has to survive as a word");
        assert_eq!(quote_literal("a"), "'a'");
        assert_eq!(quote_literal("a'b"), "'a'\\''b'");
        assert_eq!(quote_literal("\\"), "'\\'");
    }
}
