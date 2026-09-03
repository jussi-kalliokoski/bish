// The builtins: one file per family, and `ls src/builtins` is the index
// of what bish has.
//
// They live here as free functions taking `&mut Shell` rather than as
// methods on it. Nothing about a builtin wants to be a method -- it is
// a command that happens to run in-process -- and seventy of them
// inside one `impl Shell` was four thousand lines that `exec.rs` had to
// carry on top of the executor, the expander and the redirection
// machinery, which are what that file is actually about.
//
// `exec.rs` keeps the dispatch (`dispatch_builtin_or_external_impl`)
// and everything a builtin is dispatched *from*: the executor proper
// (`run_program`/`run_single`/`run_multi`/`run_pipeline`), expansion,
// redirection, variable scoping, and the signal/FFI block. That is the
// actual shell, and it belongs together.
//
// The cost, stated plainly: a good deal of `Shell` that was private to
// one file is now `pub(crate)`. Its fields, several of its helpers, and
// types like `JobTable`, `OutputSink` and `CallFrame` are visible to
// the crate because a builtin in another file has to reach them. That
// is a real loss of encapsulation, traded for `exec.rs` being 2,700
// lines shorter and for `ls src/builtins` answering "what builtins does
// bish have?" -- the question that `ls src/ | grep tar` could not.

pub(crate) mod bish;
pub(crate) mod completion;
pub(crate) mod dirs;
pub(crate) mod history;
pub(crate) mod io;
pub(crate) mod jobs;
pub(crate) mod limits;
pub(crate) mod shell;
pub(crate) mod vars;

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

pub fn break_loop(args: &[String]) -> crate::exec::ExecResult {
    let n = args.first().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).max(1);
    crate::exec::ExecResult::Break(n)
}

pub fn continue_loop(args: &[String]) -> crate::exec::ExecResult {
    let n = args.first().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).max(1);
    crate::exec::ExecResult::Continue(n)
}

// `use_glob` is true for `[[` (bash pattern-matches `==`/`!=`) and false for
// `[`/`test` (POSIX literal-equality).
// `Err` is a usage error -- an operand where an integer was wanted, or
// more words than any form of `test` has. bash reports those and
// returns 2, distinct from the 1 that means "the expression is false",
// and a script that checks `$?` for 1 has to be able to tell them
// apart. The caller prints it: only it knows whether the user wrote
/// The two unary tests that ask the *shell* rather than the filesystem
/// or the text: `-v NAME` (is that variable set) and `-o OPTNAME` (is
/// that shell option on).
///
/// Passed in, because this evaluator is deliberately a function of its
/// arguments -- see `unary`, which has no shell to ask. Without them
/// `[ -v x ]` was always false and `-o` was not a test at all: it was
/// read as the OR connective, so `test -o errexit` came out as "empty
/// OR errexit", which is true whatever the option is set to.
/// Answered up front, for the operands that appear in this argument
/// list, rather than as a callback: the answers need `&mut Shell` (an
/// array subscript is an arithmetic expression), and the evaluator runs
/// while the caller still needs the shell for its own diagnostics.
pub struct ShellFacts<'a> {
    pub var_is_set: &'a std::collections::HashMap<String, bool>,
    pub option_on: &'a std::collections::HashMap<String, bool>,
}

// `test` or `[`.
pub fn test(args: &[String], use_glob: bool, facts: &ShellFacts<'_>) -> Result<i32, String> {
    Ok(i32::from(!eval_test_expr(args, use_glob, facts)?))
}

fn eval_test_expr(args: &[String], use_glob: bool, facts: &ShellFacts<'_>) -> Result<bool, String> {
    // Split on top-level -a/-o (no parens support). Not strictly
    // POSIX-precedence-correct, but covers real-world usage.
    let mut clauses: Vec<Vec<String>> = vec![Vec::new()];
    let mut combinators: Vec<&str> = Vec::new();
    for a in args {
        // Only where an operator would already have an operand to
        // connect. `-o` is also a *unary* test, and one at the start of
        // a clause is that one: `[ -o errexit -a -o xtrace ]` is two
        // option tests joined by AND, not four empty clauses.
        let connective = (a == "-a" || a == "-o") && !clauses.last().is_none_or(Vec::is_empty);
        if connective {
            combinators.push(if a == "-a" { "-a" } else { "-o" });
            clauses.push(Vec::new());
        } else {
            clauses.last_mut().unwrap().push(a.clone());
        }
    }
    let mut result = eval_simple(&clauses[0], use_glob, facts)?;
    for (i, comb) in combinators.iter().enumerate() {
        let next = eval_simple(&clauses[i + 1], use_glob, facts)?;
        result = match *comb {
            "-a" => result && next,
            "-o" => result || next,
            _ => unreachable!(),
        };
    }
    Ok(result)
}

fn eval_simple(args: &[String], use_glob: bool, facts: &ShellFacts<'_>) -> Result<bool, String> {
    if args.first().map(|s| s.as_str()) == Some("!") {
        return Ok(!eval_simple(&args[1..], use_glob, facts)?);
    }
    match args {
        [] => Ok(false),
        [s] => Ok(!s.is_empty()),
        [op, a] if op == "-v" => Ok(facts.var_is_set.get(a).copied().unwrap_or(false)),
        [op, a] if op == "-o" => Ok(facts.option_on.get(a).copied().unwrap_or(false)),
        [op, a] => Ok(unary(op, a)),
        [a, op, b] => binary_checked(a, op, b, use_glob),
        // Four or more words is no form of `test` there is. Reading it
        // as "non-empty, so true" is how `[ "$a" = "$b" = "$c" ]`, and
        // an unquoted variable that turned into two words, both passed
        // silently.
        _ => Err("too many arguments".to_string()),
    }
}

// `binary`, but reporting the one error it can have: a numeric
// comparison whose operand is not a number. `[[ ]]` never reaches this
// -- there the operands are arithmetic expressions, where a bare name
// is a variable and an unset one is 0.
fn binary_checked(a: &str, op: &str, b: &str, use_glob: bool) -> Result<bool, String> {
    if matches!(op, "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge") {
        for operand in [a, b] {
            if operand.trim().parse::<i64>().is_err() {
                return Err(format!("{}: integer expected", operand));
            }
        }
    }
    Ok(binary(a, op, b, use_glob))
}

pub(crate) fn unary(op: &str, a: &str) -> bool {
    let path = std::path::Path::new(a);
    match op {
        "-e" => path.exists(),
        "-f" => path.is_file(),
        "-d" => path.is_dir(),
        "-r" | "-w" => std::fs::metadata(a).is_ok(),
        "-x" => is_executable(a),
        "-z" => a.is_empty(),
        "-n" => !a.is_empty(),
        "-s" => std::fs::metadata(a).map(|m| m.len() > 0).unwrap_or(false),
        "-L" | "-h" => std::fs::symlink_metadata(a).map(|m| m.file_type().is_symlink()).unwrap_or(false),
        "-p" => file_type_is(a, |ft| ft.is_fifo()),
        "-S" => file_type_is(a, |ft| ft.is_socket()),
        "-b" => file_type_is(a, |ft| ft.is_block_device()),
        "-c" => file_type_is(a, |ft| ft.is_char_device()),
        _ => false,
    }
}

#[cfg(unix)]
fn file_type_is(a: &str, pred: impl Fn(&std::fs::FileType) -> bool) -> bool {
    std::fs::metadata(a).map(|m| pred(&m.file_type())).unwrap_or(false)
}
#[cfg(not(unix))]
fn file_type_is(_a: &str, _pred: impl Fn(&std::fs::FileType) -> bool) -> bool {
    false
}

#[cfg(unix)]
fn is_executable(a: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(a).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(a: &str) -> bool {
    std::fs::metadata(a).is_ok()
}

pub(crate) fn binary(a: &str, op: &str, b: &str, use_glob: bool) -> bool {
    binary_with_case(a, op, b, use_glob, false)
}

// `binary`, with `shopt -s nocasematch`'s case folding for the two
// operators it applies to. `[ ]`/`test` never fold -- the option is
// about pattern matching, and those two compare literally.
pub(crate) fn binary_with_case(a: &str, op: &str, b: &str, use_glob: bool, fold_case: bool) -> bool {
    match op {
        "=" | "==" if use_glob => crate::glob::matches_with_case(b, a, fold_case),
        "!=" if use_glob => !crate::glob::matches_with_case(b, a, fold_case),
        "=" | "==" => a == b,
        "!=" => a != b,
        "<" => a < b,
        ">" => a > b,
        "-eq" => num(a) == num(b),
        "-ne" => num(a) != num(b),
        "-lt" => num(a) < num(b),
        "-le" => num(a) <= num(b),
        "-gt" => num(a) > num(b),
        "-ge" => num(a) >= num(b),
        "-nt" => file_newer(a, b),
        "-ot" => file_newer(b, a),
        "-ef" => files_same(a, b),
        _ => false,
    }
}

fn file_newer(a: &str, b: &str) -> bool {
    let ma = std::fs::metadata(a).and_then(|m| m.modified());
    let mb = std::fs::metadata(b).and_then(|m| m.modified());
    match (ma, mb) {
        (Ok(ta), Ok(tb)) => ta > tb,
        // bash: -nt is true if a exists and b doesn't.
        (Ok(_), Err(_)) => true,
        _ => false,
    }
}

#[cfg(unix)]
fn files_same(a: &str, b: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}
#[cfg(not(unix))]
fn files_same(a: &str, b: &str) -> bool {
    std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok()
}

fn num(s: &str) -> i64 {
    s.trim().parse().unwrap_or(0)
}
