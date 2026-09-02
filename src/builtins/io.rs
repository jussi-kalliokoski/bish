// `echo`, `printf`, `test`: the builtins that write, and the one that
// only answers. `test`'s own expression evaluator is in `mod.rs`,
// where it has always been.
//
// Free functions taking `&mut Shell` -- see `builtins/mod.rs`.

use crate::parser;
use crate::exec::{echo_expand_escapes, printf_format_once, sh_eprintln, sh_print, ExecResult, Shell};

// `echo [-neE] [arg...]`: writes each arg separated by a single
// space, then a trailing newline unless -n was given. Flags must
// come first and be one of exactly n/e/E (bundled, e.g. "-ne") --
// the first argument that doesn't fit that shape ends flag parsing,
// matching bash's own echo (no long options, no "--" special case).
// -e enables the same backslash escapes real bash's echo recognizes
// (see echo_expand_escapes); -E (the default) leaves them untouched.
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

// `printf FORMAT [ARGS...]` / `printf -v NAME FORMAT [ARGS...]`:
// bash-compatible subset -- %s %d %i %o %u %x %X %c %b %q %%
// conversions, with "-" (left-align) and "0" (zero-pad) flags, a
// numeric width, and a ".precision" (applied to %s only -- a
// truncation length; other conversions ignore it, a minor
// simplification against real printf's %d minimum-digit-count
// behavior). FORMAT's own backslash escapes (\n, \t, ...) are
// always interpreted -- unlike echo, there's no -e/-E switch, POSIX
// printf's format string is escape-interpreted unconditionally.
// FORMAT is cycled -- reused from the start -- for as long as
// there's still at least one unconsumed argument left, matching
// real printf (`printf "%s\n" a b c` prints three lines); a numeric
// conversion given a missing or non-numeric argument is treated as
// 0, a string conversion given a missing one as "". -v NAME assigns
// the formatted result to a shell variable instead of printing it.
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
        // `\c` in a `%b` argument ends the output there -- including
        // the reruns the remaining arguments would have caused. So does
        // a malformed conversion, which bash abandons the format over.
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
    // Before the output, the way bash orders them -- the diagnostic is
    // on stderr and the result on stdout, and a reader watching both
    // sees the complaint first.
    for e in errors {
        sh_eprintln!(sh, "bish: printf: {}", e);
    }

    match var_name {
        Some(name) => {
            sh.assign_var(&name, out);
        }
        None => sh_print!(sh, "{}", out),
    }
    status
}

// `[[ expr ]]`. Real recursive-descent precedence over the flat
// TestAtom stream the parser built: NOT binds tightest, then simple
// tests (unary/binary), then AND, then OR (loosest) -- matching bash.
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
