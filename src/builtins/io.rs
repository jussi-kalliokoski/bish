// `echo`, `printf` and `test`. `test`'s expression evaluator is in
// `mod.rs`.

use crate::exec::{ExecResult, Shell, echo_expand_escapes, printf_format_once, sh_eprintln, sh_print};
use crate::parser;

// `echo [-neE] [arg...]`: the args separated by spaces, then a newline
// unless `-n`. Flags come first and are only `n`, `e` and `E`, bundled or
// not; the first argument that isn't one ends them, with no `--`, as in
// bash. `-e` enables bash's echo escapes (see `echo_expand_escapes`); `-E`,
// the default, leaves them.
pub(crate) fn run_echo(sh: &Shell, args: &[String]) -> i32 {
    let mut interpret_escapes = false;
    let mut trailing_newline = true;
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a.len() < 2 || !a.starts_with('-') || !a[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E')) {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'n' => trailing_newline = false,
                'e' => interpret_escapes = true,
                'E' => interpret_escapes = false,
                _ => unreachable!(),
            }
        }
        i += 1;
    }

    let mut out = String::new();
    let mut stopped_early = false;
    for (pos, a) in args[i..].iter().enumerate() {
        if pos > 0 {
            out.push(' ');
        }
        if interpret_escapes {
            let (expanded, hit_c) = echo_expand_escapes(a);
            out.push_str(&expanded);
            if hit_c {
                stopped_early = true;
                break;
            }
        } else {
            out.push_str(a);
        }
    }
    if trailing_newline && !stopped_early {
        out.push('\n');
    }
    sh_print!(sh, "{}", out);
    0
}

// `printf FORMAT [ARGS...]` and `printf -v NAME FORMAT [ARGS...]`: the
// conversions `%s %d %i %o %u %x %X %c %b %q %%`, the `-` and `0` flags, a
// width, and a precision that only truncates `%s` (real printf also uses
// it as `%d`'s minimum digits). FORMAT's backslash escapes are always
// interpreted, as POSIX says. FORMAT is reused while arguments remain
// (`printf "%s\n" a b c` prints three lines); a missing or non-numeric
// argument is 0 for a number and "" for a string. `-v NAME` assigns the
// result instead of printing it.
pub(crate) fn run_printf(sh: &mut Shell, args: &[String]) -> i32 {
    let (var_name, rest) = if args.first().map(String::as_str) == Some("-v") {
        match args.get(1) {
            Some(name) => (Some(name.clone()), &args[2..]),
            None => {
                sh_eprintln!(sh, "bish: printf: -v: option requires an argument");
                return 1;
            }
        }
    } else {
        (None, &args[..])
    };
    // `--` ends the options, so a format can start with `-`.
    let rest = rest.strip_prefix(&["--".to_string()][..]).unwrap_or(rest);
    let Some(format) = rest.first() else {
        sh_eprintln!(sh, "bish: printf: usage: printf format [arguments]");
        return 1;
    };
    let values = &rest[1..];

    let mut out = String::new();
    let mut idx = 0;
    let mut status = 0;
    let mut errors: Vec<String> = Vec::new();
    loop {
        let before = idx;
        // `\c` in a `%b` argument ends the output there, including the
        // reruns for remaining arguments. So does a malformed conversion,
        // as in bash.
        let outcome = printf_format_once(format, values, &mut idx, &mut out);
        errors.extend(outcome.errors);
        if outcome.status != 0 {
            status = outcome.status;
        }
        if outcome.stop {
            break;
        }
        if idx >= values.len() || idx == before {
            break;
        }
    }
    // Diagnostics before the output, as bash orders them.
    for e in errors {
        sh_eprintln!(sh, "bish: printf: {}", e);
    }

    match var_name {
        // `printf -v 'arr[0]'` writes an array element, so a loop can fill an
        // array without a subshell.
        Some(name) => match name.split_once('[').and_then(|(base, rest)| rest.strip_suffix(']').map(|i| (base, i))) {
            Some((base, index)) => {
                let (base, index) = (base.to_string(), index.to_string());
                sh.array_set_index_public(&base, &index, out);
            }
            None => {
                sh.assign_var(&name, out);
            }
        },
        None => sh_print!(sh, "{}", out),
    }
    status
}

// `[[ expr ]]`, by recursive descent over the parser's `TestAtom`s: NOT
// binds tightest, then simple tests, then AND, then OR, as in bash.
pub(crate) fn run_test(sh: &mut Shell, atoms: &[parser::TestAtom]) -> ExecResult {
    let mut pos = 0;
    match sh.eval_test_or(atoms, &mut pos) {
        Ok(b) => ExecResult::Status(if b { 0 } else { 1 }),
        Err(e) => {
            sh_eprintln!(sh, "bish: [[: {}", e);
            ExecResult::Status(2)
        }
    }
}
