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
// and everything a builtin is dispatched *from*.

pub(crate) mod completion;
pub(crate) mod dirs;
pub(crate) mod history;
pub(crate) mod jobs;
pub(crate) mod limits;
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
pub fn test(args: &[String], use_glob: bool) -> i32 {
    if eval_test_expr(args, use_glob) {
        0
    } else {
        1
    }
}

fn eval_test_expr(args: &[String], use_glob: bool) -> bool {
    // Split on top-level -a/-o (no parens support). Not strictly
    // POSIX-precedence-correct, but covers real-world usage.
    let mut clauses: Vec<Vec<String>> = vec![Vec::new()];
    let mut combinators: Vec<&str> = Vec::new();
    for a in args {
        if a == "-a" || a == "-o" {
            combinators.push(if a == "-a" { "-a" } else { "-o" });
            clauses.push(Vec::new());
        } else {
            clauses.last_mut().unwrap().push(a.clone());
        }
    }
    let mut result = eval_simple(&clauses[0], use_glob);
    for (i, comb) in combinators.iter().enumerate() {
        let next = eval_simple(&clauses[i + 1], use_glob);
        result = match *comb {
            "-a" => result && next,
            "-o" => result || next,
            _ => unreachable!(),
        };
    }
    result
}

fn eval_simple(args: &[String], use_glob: bool) -> bool {
    if args.first().map(|s| s.as_str()) == Some("!") {
        return !eval_simple(&args[1..], use_glob);
    }
    match args {
        [] => false,
        [s] => !s.is_empty(),
        [op, a] => unary(op, a),
        [a, op, b] => binary(a, op, b, use_glob),
        _ => !args.is_empty(),
    }
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
    match op {
        "=" | "==" if use_glob => crate::glob::matches(b, a),
        "!=" if use_glob => !crate::glob::matches(b, a),
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
