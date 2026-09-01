// Reconstructs valid bish source text from parsed AST -- used to forward
// currently-defined functions into a self-exec'd child process for command
// substitution / subshells (see exec.rs), since those run as a fresh `bish
// -c` process that otherwise has no knowledge of the parent's in-memory
// function table. Doesn't need to be a general pretty-printer: just needs
// to round-trip whatever a function body can contain.

use crate::lexer::{Chunk, ReplaceAnchor, TransformKind, VarOp};
use crate::parser::{AndOr, ArrayLiteralItem, AssignMode, Combinator, Command, ListItem, Pipeline, Redirect, Sep, SimpleCommand, Word};

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
        parts.push(serialize_array_literal_assign(name, *mode, items));
    }
    for (name, index, val) in &sc.index_assigns {
        parts.push(format!("{}[{}]={}", name, index, serialize_word(val)));
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
    for r in &sc.redirects {
        parts.push(serialize_redirect(r));
    }
    parts.join(" ")
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

pub fn serialize_redirect(r: &Redirect) -> String {
    match r {
        Redirect::In(w) => format!("<{}", serialize_word(w)),
        Redirect::InOut(w) => format!("<>{}", serialize_word(w)),
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
        Redirect::FdInOut { fd, word } => format!("{}<>{}", fd, serialize_word(word)),
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
        Chunk::VarNamesMatchingPrefix { prefix, at, quoted } => {
            wrap_quoted(format!("${{!{}{}}}", prefix, if *at { "@" } else { "*" }), *quoted)
        }
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
        VarOp::Transform(kind) => {
            let letter = match kind {
                TransformKind::Quote => "Q",
                TransformKind::Upper => "U",
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
