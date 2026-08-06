// Reconstructs valid bish source text from parsed AST -- used to forward
// currently-defined functions into a self-exec'd child process for command
// substitution / subshells (see exec.rs), since those run as a fresh `bish
// -c` process that otherwise has no knowledge of the parent's in-memory
// function table. Doesn't need to be a general pretty-printer: just needs
// to round-trip whatever a function body can contain.

use crate::lexer::{Chunk, ReplaceAnchor, VarOp};
use crate::parser::{AndOr, AssignMode, Combinator, Command, ListItem, Pipeline, Redirect, Sep, SimpleCommand, Word};

pub fn serialize_program(prog: &[ListItem]) -> String {
    let mut s = String::new();
    for item in prog {
        s.push_str(&serialize_and_or(&item.and_or));
        s.push_str(match item.sep {
            Sep::Seq => ";\n",
            Sep::Background => "&\n",
        });
    }
    s
}

fn serialize_and_or(ao: &AndOr) -> String {
    let mut s = serialize_pipeline(&ao.first);
    for (comb, p) in &ao.rest {
        s.push_str(match comb {
            Combinator::And => " && ",
            Combinator::Or => " || ",
        });
        s.push_str(&serialize_pipeline(p));
    }
    s
}

fn serialize_pipeline(p: &Pipeline) -> String {
    let mut s = String::new();
    if p.negate {
        s.push_str("! ");
    }
    let parts: Vec<String> = p.commands.iter().map(serialize_command).collect();
    s.push_str(&parts.join(" | "));
    s
}

pub fn serialize_command(cmd: &Command) -> String {
    match cmd {
        Command::Simple(sc) => serialize_simple(sc),
        Command::If { branches, else_branch, .. } => {
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
            s
        }
        Command::While { cond, body, until, .. } => {
            let mut s = String::new();
            s.push_str(if *until { "until " } else { "while " });
            s.push_str(&serialize_program(cond));
            s.push_str("do\n");
            s.push_str(&serialize_program(body));
            s.push_str("done");
            s
        }
        Command::For { var, words, body, .. } => {
            let mut s = format!("for {} ", var);
            if let Some(words) = words {
                s.push_str("in ");
                s.push_str(&words.iter().map(serialize_word).collect::<Vec<_>>().join(" "));
                s.push('\n');
            }
            s.push_str("do\n");
            s.push_str(&serialize_program(body));
            s.push_str("done");
            s
        }
        Command::CFor { init, cond, step, body, .. } => {
            let mut s = format!("for (({}; {}; {}))\n", init, cond, step);
            s.push_str("do\n");
            s.push_str(&serialize_program(body));
            s.push_str("done");
            s
        }
        Command::Select { var, words, body, .. } => {
            let mut s = format!("select {} ", var);
            if let Some(words) = words {
                s.push_str("in ");
                s.push_str(&words.iter().map(serialize_word).collect::<Vec<_>>().join(" "));
                s.push('\n');
            }
            s.push_str("do\n");
            s.push_str(&serialize_program(body));
            s.push_str("done");
            s
        }
        Command::Case { word, arms, .. } => {
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
            s
        }
        Command::Group(prog, _) => format!("{{\n{}}}", serialize_program(prog)),
        Command::FuncDef { name, body } => format!("{}() {}", name, serialize_command(body)),
        Command::Subshell(raw, _) => format!("({})", raw),
        Command::Arith(raw, _) => format!("(({}))", raw),
        Command::Test(atoms, _) => format!("[[ {} ]]", serialize_test_atoms(atoms)),
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

fn serialize_simple(sc: &SimpleCommand) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (name, mode, val) in &sc.assigns {
        let op = if *mode == AssignMode::Append { "+=" } else { "=" };
        parts.push(format!("{}{}{}", name, op, serialize_word(val)));
    }
    for (name, mode, items) in &sc.array_assigns {
        let op = if *mode == AssignMode::Append { "+=" } else { "=" };
        let words: Vec<String> = items.iter().map(serialize_word).collect();
        parts.push(format!("{}{}({})", name, op, words.join(" ")));
    }
    for (name, index, val) in &sc.index_assigns {
        parts.push(format!("{}[{}]={}", name, index, serialize_word(val)));
    }
    for w in &sc.words {
        parts.push(serialize_word(w));
    }
    for r in &sc.redirects {
        parts.push(serialize_redirect(r));
    }
    parts.join(" ")
}

pub fn serialize_redirect(r: &Redirect) -> String {
    match r {
        Redirect::In(w) => format!("<{}", serialize_word(w)),
        Redirect::Out { word, append } => format!("{}{}", if *append { ">>" } else { ">" }, serialize_word(word)),
        Redirect::Err { word, append } => format!("2{}{}", if *append { ">>" } else { ">" }, serialize_word(word)),
        Redirect::Both { word, append } => format!("&{}{}", if *append { ">>" } else { ">" }, serialize_word(word)),
        Redirect::DupErrToOut => "2>&1".to_string(),
        Redirect::HereString(w) => format!("<<<{}", serialize_word(w)),
        // Re-emit as an equivalent here-string: by serialization time the
        // body is already fully captured, so a real <<DELIM...DELIM block
        // isn't needed to reproduce the same runtime content.
        Redirect::HereDoc(w) => format!("<<<{}", serialize_word(w)),
        Redirect::FdOut { fd, word, append } => {
            format!("{}{}{}", fd, if *append { ">>" } else { ">" }, serialize_word(word))
        }
        Redirect::FdIn { fd, word } => format!("{}<{}", fd, serialize_word(word)),
        Redirect::FdDup { fd, target } => format!("{}>&{}", fd, target),
        Redirect::FdDupWord { fd, word } => format!("{}>&{}", fd, serialize_word(word)),
        Redirect::FdClose { fd } => format!("{}>&-", fd),
    }
}

fn serialize_word(w: &Word) -> String {
    if w.chunks.is_empty() {
        return "''".to_string();
    }
    w.chunks.iter().map(serialize_chunk).collect()
}

fn serialize_chunk(c: &Chunk) -> String {
    match c {
        Chunk::Str(s) | Chunk::LiteralStr(s) => quote_literal(s),
        Chunk::Var { name, quoted } => wrap_quoted(format!("${{{}}}", name), *quoted),
        Chunk::Sub { raw, quoted } => wrap_quoted(format!("$({})", raw), *quoted),
        Chunk::Arith { raw, quoted } => wrap_quoted(format!("$(({}))", raw), *quoted),
        Chunk::VarExpand { name, op, quoted } => wrap_quoted(serialize_var_op(name, op), *quoted),
        Chunk::ArrayVar { name, index, quoted } => wrap_quoted(format!("${{{}[{}]}}", name, index), *quoted),
        Chunk::ArrayLength { name, index } => format!("${{#{}[{}]}}", name, index),
        Chunk::ArrayVarExpand { name, index, op, quoted } => {
            wrap_quoted(serialize_array_var_op(name, index, op), *quoted)
        }
        Chunk::Indirect { name, quoted } => wrap_quoted(format!("${{!{}}}", name), *quoted),
        Chunk::ArrayKeys { name, quoted } => wrap_quoted(format!("${{!{}[@]}}", name), *quoted),
        Chunk::ProcSubIn { raw } => format!("<({})", raw),
        Chunk::ProcSubOut { raw } => format!(">({})", raw),
    }
}

// Preserves quoted-ness across the preamble round-trip (see
// exec.rs::functions_preamble) so a re-parsed expansion keeps the same
// word-splitting eligibility it had in the original source.
fn wrap_quoted(s: String, quoted: bool) -> String {
    if quoted {
        format!("\"{}\"", s)
    } else {
        s
    }
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
    }
}

fn serialize_array_var_op(name: &str, index: &str, op: &VarOp) -> String {
    let full = format!("{}[{}]", name, index);
    serialize_var_op(&full, op)
}
