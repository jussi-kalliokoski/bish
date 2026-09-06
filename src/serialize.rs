// Reconstructs valid bish source text from parsed AST -- used to forward
// currently-defined functions into a self-exec'd child process for command
// substitution / subshells (see exec.rs), since those run as a fresh `bish
// -c` process that otherwise has no knowledge of the parent's in-memory
// function table. Doesn't need to be a general pretty-printer: just needs
// to round-trip whatever a function body can contain.

use crate::lexer::{Chunk, Quoting, ReplaceAnchor, TransformKind, VarOp};
use crate::parser::{AndOr, ArrayLiteralItem, AssignMode, Combinator, Command, ListItem, Pipeline, Redirect, Sep, SimpleCommand, Word};

pub fn serialize_program(prog: &[ListItem]) -> String {
    let mut s = String::new();
    // Statements go back on the lines they were written on, so that a
    // script that reads `$LINENO` says the same thing after a round
    // trip as before it. Putting every statement on a line of its own
    // was the last thing that broke one: `PS4='+$LINENO '; set -x;
    // echo t` is three statements on line 1, and came back as three
    // lines, so the trace numbered them 1, 2, 3.
    //
    // Relative to the first statement rather than to line 1, so a
    // nested program -- a function body, a loop's body -- keeps the
    // spacing it had inside itself without the enclosing text having to
    // agree about where it starts.
    let base = prog.iter().map(|i| i.line).find(|l| *l > 0);
    let mut at = base.unwrap_or(0);
    for item in prog {
        // Blank lines to reach this statement's own. A synthetic item
        // (`declare -f`'s re-serialised body, the function preamble)
        // has no line and simply follows the one before it.
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
        // A heredoc body starts on the next line, so the separator
        // cannot share this one -- and the newline the body brings is
        // the separator a `;` would have been.
        if !bodies.is_empty() {
            s.push('\n');
            at += 1;
            for b in bodies {
                at += b.matches('\n').count();
                s.push_str(&b);
            }
            continue;
        }
        // The separator only; whether a newline follows is the next
        // statement's business, and the last one needs none.
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
    // `time` is a reserved word in front of the whole pipeline, not a
    // command inside it, so it lives here rather than in any argv --
    // and was simply not written back at all, which is how six corpus
    // scripts lost their timing report on the way through.
    match p.timed {
        Some(crate::parser::TimeStyle::Shell) => s.push_str("time "),
        Some(crate::parser::TimeStyle::Posix) => s.push_str("time -p "),
        None => {}
    }
    if p.negate {
        s.push_str("! ");
    }
    // A stage's heredoc body goes between it and the `|`, exactly as
    // bash writes one: `cat <<E |` then the body, then the next stage.
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

/// A compound's own redirects, written back after its closing keyword.
///
/// Every arm below used to match these with `..` and drop them on the
/// floor, so `{ cmd; } 2>e` came back as `{ cmd; }` -- the redirect
/// silently gone rather than merely reordered, which is what its
/// round-trip cases had been recorded as.
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

/// `serialize_command`, keeping the heredoc bodies separate so a caller
/// that is building a *line* can put them after it -- a pipeline's `|`
/// goes before the body, not after.
fn serialize_command_parts(cmd: &Command) -> (String, Vec<String>) {
    match cmd {
        Command::Simple(sc) => serialize_simple_with_heredocs(sc),
        other => (serialize_command_inner(other, true), Vec::new()),
    }
}

/// The same text without the compound's *own* trailing redirects, for a
/// caller that is applying those itself.
///
/// `run_compound_redirected` resolves `{ ...; } 3>&-`'s redirects, sets
/// them up on a self-exec'd child, and hands that child the compound as
/// source. Once the redirects were written back too, the child read its
/// own `3>&-`, took the same path, and spawned another child -- forever,
/// with the shell simply appearing to hang. Only the top level is
/// stripped: a redirect inside the body belongs to a command the child
/// really does have to run.
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

pub(crate) fn serialize_simple(sc: &SimpleCommand) -> String {
    let (line, bodies) = serialize_simple_with_heredocs(sc);
    // Inline, for the callers that want one command's text and have
    // nowhere to put a body: `$BASH_COMMAND`, a job's name. A single
    // command with its heredoc after it is still valid source.
    match bodies.is_empty() {
        true => line,
        false => format!("{}\n{}", line, bodies.concat()),
    }
}

/// The command's text and the heredoc bodies that must follow the line
/// it lands on -- see `format_simple`, which has the same shape and the
/// same reason.
fn serialize_simple_with_heredocs(sc: &SimpleCommand) -> (String, Vec<String>) {
    let mut parts = serialize_simple_parts(sc);
    let mut bodies = Vec::new();
    for r in &sc.redirects {
        parts.push(serialize_redirect(r));
        if let Redirect::HereDoc(w, spelling) = r {
            bodies.push(heredoc_body(w, spelling));
        }
    }
    (parts.join(" "), bodies)
}

/// Everything a simple command is made of *except* its redirects:
/// assignments, then words, with a declare-family array literal spliced
/// back in at its own recorded position. Split out because `declare -f`
/// writes the redirects differently (see `format_redirect`) and there
/// is no reason for it to rebuild the rest.
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
    // array_word_assigns (a later-word declare-family array literal, e.g.
    // `declare -A m=([a]=1)`) has no placeholder in `sc.words` -- it's
    // spliced back in at its own recorded position instead, matching
    // SimpleCommand::array_word_assigns's own doc comment.
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

// `>`, `>>` or `>|` -- the third only exists to defeat `set -C`, so it
// has to survive a round-trip through the formatter intact.
fn redirect_op(append: bool, clobber: bool) -> &'static str {
    match (append, clobber) {
        (true, _) => ">>",
        (false, true) => ">|",
        (false, false) => ">",
    }
}

/// The word after a redirect operator, always spaced from it.
///
/// Process substitution is why it cannot be joined: `wc -l < <(cmd)`
/// written back as `wc -l <<(cmd)` makes `<<` a heredoc operator and
/// the redirect disappears, and `cmd > >(cat)` becomes an append to a
/// file named `(cat)`. Both are still valid scripts, which is why the
/// corpus caught them by behaviour rather than the parser by error.
///
/// Spaced *always*, rather than only when it would collide, because
/// that is how bash prints one -- `echo z > f`, and `$BASH_COMMAND`
/// names a command the way bash would write it.
fn redirect_target(w: &Word) -> String {
    format!(" {}", serialize_word(w))
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
        // A real heredoc, not the equivalent here-string it used to be.
        // Both reproduce the same runtime content -- the body is
        // already captured by now -- but only one of them still *looks*
        // like a heredoc, and `declare -f` prints what a round trip
        // left behind.
        Redirect::HereDoc(_, spelling) => {
            let dash = if spelling.strip_tabs { "-" } else { "" };
            let delim = if spelling.quoted { format!("'{}'", spelling.delimiter) } else { spelling.delimiter.clone() };
            format!("<<{}{}", dash, delim)
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
        Redirect::FdDup { fd, target } => format!("{}>&{}", fd, target),
        Redirect::FdDupWord { fd, word } => format!("{}>&{}", fd, redirect_target(word)),
        Redirect::FdClose { fd } => format!("{}>&-", fd),
    }
}

/// Whether a chunk was inside a pair of double quotes.
///
/// Both halves matter: a literal run knows it directly, and an
/// expansion knows it as `quoted`, which the lexer has always
/// recorded. A run of them is one pair of quotes in the source and is
/// written back as one -- `"a$x b"` rather than `'a'"$x"' b'`.
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

/// The text of a chunk as it goes *inside* a `"..."` -- so a literal
/// run keeps its characters and only the four that mean something to
/// double quotes are escaped.
fn inside_double_quotes(c: &Chunk) -> String {
    match c {
        Chunk::LiteralStr(t, _) => t.chars().flat_map(escaped_in_double_quotes).collect(),
        // An expansion writes itself the same either way; the quotes
        // around the run are what make it quoted.
        other => serialize_chunk_unquoted(other),
    }
}

/// The four characters that mean something inside `"..."`, and
/// therefore the only ones that need a backslash there.
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
            // The whole run, back into the one pair of quotes it came
            // from.
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
    // A word that writes as nothing has to be written as `''`, or it
    // stops being a word at all: `echo ""` would come back as `echo`,
    // which prints a blank line where the original printed one too --
    // but `f ""` would lose an argument. Empty chunks and a chunk that
    // is itself empty (the parser makes `Chunk::Str("")` for the tail
    // of some assignments) are the same case.
    match text.is_empty() {
        true => "''".to_string(),
        false => text,
    }
}

fn serialize_chunk(c: &Chunk) -> String {
    serialize_chunk_inner(c, true)
}

/// The same, for a chunk that is already inside a pair of double
/// quotes: the quotes an expansion would otherwise put around itself
/// are the ones the run has already opened.
fn serialize_chunk_unquoted(c: &Chunk) -> String {
    serialize_chunk_inner(c, false)
}

fn serialize_chunk_inner(c: &Chunk, wrap: bool) -> String {
    let wrap_quoted = |s: String, quoted: bool| if wrap { wrap_quoted(s, quoted) } else { s };
    match c {
        // Written back as the source wrote it -- `~` is only a tilde
        // prefix at the start of a word, so it needs no quoting here.
        Chunk::Tilde { name } => format!("~{}", name),
        // The two are kept apart here because the parse tree keeps them
        // apart, and collapsing them was this serializer's one real
        // bug: `Chunk::Str` is text that was written unquoted and
        // `Chunk::LiteralStr` is text that was quoted or escaped, so
        // quoting both made every unquoted word literal. `echo *`
        // came back as `'echo' '*'` and printed an asterisk; `case $x
        // in a*)` stopped matching anything but a literal `a*`; and
        // `[[ x < y ]]` lost its operator, since `'<'` inside `[[ ]]`
        // is a string rather than a comparison.
        //
        // Emitting `Str` verbatim is safe precisely because the lexer
        // has already done the separating. Anything that had to be
        // escaped to survive being read -- a quoted run, a backslashed
        // space, a backslashed glob character -- arrives as
        // `LiteralStr`: `echo a\ b` lexes to `[Str("a"), LiteralStr(" "),
        // Str("b")]`, and `echo \*` to `[LiteralStr("*")]` where `echo *`
        // gives `[Str("*")]`. So what is left in `Str` is by
        // construction text that reads back as itself.
        Chunk::Str(s) => s.clone(),
        // Back in the spelling it was written in. `Double` reaches here
        // only for a run of one, since `serialize_word` gathers the
        // longer ones itself.
        Chunk::LiteralStr(s, quoting) => match quoting {
            Quoting::Single => quote_literal(s),
            Quoting::Double => format!("\"{}\"", s.chars().flat_map(escaped_in_double_quotes).collect::<String>()),
            // One backslash per character, which is how it arrived:
            // the lexer makes a chunk of each escape.
            Quoting::Escape => s.chars().map(|c| format!("\\{c}")).collect(),
            // bash does not put `$'...'` back: it prints the resolved
            // text as an ordinary literal, so `$'a\tb'` comes back as
            // `'a<tab>b'`. Measured, not assumed.
            Quoting::Dollar => quote_literal(s),
        },
        // Written back the way it was written: `$x` braced only if
        // the source braced it. Both expand the same, and `declare -f`
        // is where the difference is visible.
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
// A second way to write a command out, and a different job from the one
// above. `serialize_*` exists so a construct survives a trip through a
// child shell: what matters there is that it means the same thing, and
// nothing looks at it. `declare -f` output is *read* -- by people, by
// completion scripts, by `sh -c "$(declare -f f); f"` -- and bash's
// shape for it is specific enough that anything else reads as a
// different shell.
//
// The shape, measured against bash rather than guessed:
//
//   - `f () ` and `{ ` each keep a trailing space, on their own lines.
//   - Four spaces per level of nesting.
//   - Every statement ends in `;` except the last one before a `}` or a
//     `;;`. Before `fi`, `done`, `else` and `esac` the last statement
//     keeps its `;`.
//   - `if C; then` and `while C; do` put the keyword on the condition's
//     line; `for` and `select` put `do` on a line of its own.
//   - `elif` does not survive: bash prints it as a nested `else`/`if`,
//     and so does this.
//   - `case W in ` has a trailing space, arms sit one level in and their
//     bodies two, with `;;` back at the arm's level.
//   - A redirect's target is spaced (`> f`, `2> f`, `&> f`) where a
//     descriptor is not (`2>&1`), and `>&2` is written with its implied
//     descriptor as `1>&2`.
//   - A function body is always printed as a brace group, even when it
//     was written as `f() ( ... )`.
//
// Two things are normalised rather than reproduced, because the parse
// tree records what a word *means* and not how it was spelled: quoting
// comes back in one style (`"a"` prints as `'a'`), and a heredoc prints
// as the here-string it round-trips through. Both would need the lexer
// to keep the source form, which is a change to the tree and not to
// this.

fn pad(indent: usize) -> String {
    " ".repeat(indent * 4)
}

/// One function, in bash's `declare -f` shape, with no trailing newline.
pub fn format_function(name: &str, body: &Command) -> String {
    format_function_at(name, body, 0, false)
}

/// `nested` is bash's own asymmetry, not an option: a function defined
/// inside another one is printed with the `function` keyword in front
/// of it, where the outer one is not.
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

/// A program, one statement per line, each already indented.
///
/// `semi_on_last` is the whole subtlety: what follows the list decides
/// whether its final statement keeps its separator. A `}` or a `;;`
/// stands on its own, so the statement before it does not need one; a
/// `fi`, `done`, `else` or `esac` does.
fn format_list(prog: &[ListItem], indent: usize, semi_on_last: bool, out: &mut String) {
    // `&` does not end the line the way `;` does: bash writes
    // `echo a & echo b` on one, and only breaks when a `;` or the end
    // of the list says to.
    let mut at_line_start = true;
    for (i, item) in prog.iter().enumerate() {
        if at_line_start {
            out.push_str(&pad(indent));
        }
        let (text, bodies) = format_and_or(&item.and_or, indent);
        out.push_str(&text);
        let last = i + 1 == prog.len();
        at_line_start = true;
        // A heredoc body has to start on the next line, so the line it
        // follows cannot also carry a `;` -- and bash does not put one
        // there. The blank line after the delimiter is the body's own.
        if !bodies.is_empty() {
            out.push('\n');
            for b in bodies {
                out.push_str(&b);
            }
            // The blank line that ends a line carrying heredocs.
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

/// A program written on one line, for the places bash puts one there:
/// an `if`/`while` condition, or a subshell's body. A condition of more
/// than one statement wraps, with the continuation at the enclosing
/// statement's own indent -- `if a;\n    b; then`.
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
            // A stage whose predecessor opened a heredoc starts a line
            // of its own, because the body had to go between them. Two
            // spaces, which is bash's own choice here and not this
            // block's indent.
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
    let plain = |s: String| (s, Vec::new());
    match cmd {
        Command::Simple(sc) => format_simple(sc),
        // `elif` is not in bash's output at all: it prints the second
        // `if` as the first one's `else` body, nested a level in. This
        // reproduces that rather than the source, because that is what
        // reading `declare -f` in bash shows.
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
            // Closed from the inside out, each `fi` but the outermost
            // ending a statement that the `else` above it continues.
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
            // A `for x` with no list iterates the positional parameters,
            // and bash prints the list it stands for rather than the
            // shorthand.
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
        // Held as source text rather than a tree (see Command::Subshell),
        // so it is parsed here to be laid out. A body that will not parse
        // is written back as it stands -- it is still what the function
        // contains.
        Command::Subshell(raw, redirects) => {
            let s = match reparse(raw) {
                Some(prog) => format!("( {} )", format_list_inline(&prog, indent)),
                None => format!("( {} )", raw.trim()),
            };
            plain(with_formatted_redirects(s, redirects))
        }
        // Verbatim between the parentheses, spaces and all: bash keeps
        // whatever was written there, so `((n++))` and `(( n++ ))` each
        // come back as themselves.
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

/// A heredoc as bash writes one back: the body at column zero, then
/// the delimiter. Column zero because that is where a heredoc body
/// lives -- indenting it would change what it contains.
///
/// No blank line here. bash puts one after the *last* body on a line,
/// so two heredocs on one command run together and a stage that has
/// another after it in a pipeline gets none at all. The caller that
/// knows the line has ended is the one that adds it.
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

/// The command's own text, and the heredoc bodies that have to follow
/// the line it ends up on. They are kept apart because only the caller
/// knows where that line ends: in a pipeline the `|` goes before them.
fn format_simple(sc: &SimpleCommand) -> (String, Vec<String>) {
    let mut parts: Vec<String> = serialize_simple_parts(sc);
    let mut bodies = Vec::new();
    for r in &sc.redirects {
        parts.push(format_redirect(r));
        if let Redirect::HereDoc(w, spelling) = r {
            bodies.push(heredoc_body(w, spelling));
        }
    }
    (parts.join(" "), bodies)
}

/// A redirect the way bash prints one: the operator, then a space
/// before a *file* and none before a descriptor. `>&2` is printed with
/// the descriptor it implies, as `1>&2`.
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
        Redirect::FdDup { fd, target } => format!("{}>&{}", fd, target),
        Redirect::FdDupWord { fd, word } => format!("{}>&{}", fd, serialize_word(word)),
        Redirect::FdClose { fd } => format!("{}>&-", fd),
        // Written as the heredoc it was, which is what the body
        // emitted after this line completes. `<<-` and a quoted
        // delimiter both change what the body means, so both survive.
        Redirect::HereDoc(_, spelling) => {
            let dash = if spelling.strip_tabs { "-" } else { "" };
            let delim = if spelling.quoted { format!("'{}'", spelling.delimiter) } else { spelling.delimiter.clone() };
            format!("<<{}{}", dash, delim)
        }
        // The forms with nothing to normalise, or nothing bash's own
        // layout says about them.
        other => serialize_redirect(other),
    }
}

#[cfg(test)]
mod quoting_tests {
    use super::quote_literal;

    // Everything that re-enters the shell as text goes through this
    // function: `functions_preamble` rebuilding a function body for a
    // subshell, `$(...)` capture, `complete -W`'s word list, `abbr`
    // listing itself back in a form that can be sourced. If it is ever
    // wrong for some byte, the failure is not a wrong answer -- it is a
    // string that stops being data and starts being syntax.
    //
    // So this checks the property directly, on the bytes most likely to
    // break it, against two independent readers: bish's own lexer and
    // expansion, and real bash.
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

        // Plus random strings over the same alphabet, so the corpus is
        // not just the cases I happened to think of. Seeded, because a
        // test that fails only sometimes is a test nobody trusts.
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

    // The same corpus through real bash. bish agreeing with itself
    // would prove only that its lexer undoes its own quoter; the
    // question is whether the quoting is *right*, and bash is what
    // decides that.
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

    // The shape of the output, pinned separately: a single-quoted run
    // with `'\''` for each quote is the POSIX idiom, and the empty
    // string still has to be a word rather than nothing at all.
    #[test]
    fn quote_literal_produces_the_posix_idiom() {
        assert_eq!(quote_literal(""), "''", "an empty word has to survive as a word");
        assert_eq!(quote_literal("a"), "'a'");
        assert_eq!(quote_literal("a'b"), "'a'\\''b'");
        assert_eq!(quote_literal("\\"), "'\\'");
    }
}
