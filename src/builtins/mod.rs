// The builtins, one file per family, as free functions taking
// `&mut Shell`: a builtin is a command that runs in-process, not a method.
// exec.rs keeps the dispatch (`dispatch_builtin_or_external_impl`) and
// what builtins are dispatched from: the executor, expansion,
// redirection, variable scoping and signals.
//
// The cost is that much of `Shell` (fields, helpers, `JobTable`,
// `OutputSink`, `CallFrame`) is `pub(crate)` so builtins elsewhere can
// reach it.

pub(crate) mod bish;
pub(crate) mod completion;
pub(crate) mod dirs;
pub(crate) mod history;
pub(crate) mod io;
pub(crate) mod jobs;
pub(crate) mod limits;
pub(crate) mod shell;
pub(crate) mod vars;

pub fn break_loop(args: &[String]) -> crate::exec::ExecResult {
    let n = args.first().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).max(1);
    crate::exec::ExecResult::Break(n)
}

pub fn continue_loop(args: &[String]) -> crate::exec::ExecResult {
    let n = args.first().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).max(1);
    crate::exec::ExecResult::Continue(n)
}

/// The two unary tests that ask the shell rather than the filesystem or
/// the text: `-v NAME` (is that variable set) and `-o OPTNAME` (is that
/// option on). Passed in, since this evaluator is a function of its
/// arguments. Answered up front for the operands in this argument list,
/// not by callback: the answers need `&mut Shell` (a subscript is
/// arithmetic), which the caller still needs for its diagnostics.
pub struct ShellFacts<'a> {
    pub var_is_set: &'a std::collections::HashMap<String, bool>,
    pub option_on: &'a std::collections::HashMap<String, bool>,
}

// `test` or `[`. `use_glob` is true for `[[` (`==`/`!=` match patterns)
// and false for `[`/`test` (literal equality). `Err` is a usage error, a
// non-integer operand or more words than any form of `test` has: bash
// reports those with status 2, distinct from 1 for false. The caller
// prints it, since only it knows whether `test` or `[` was written.
pub fn test(args: &[String], use_glob: bool, facts: &ShellFacts<'_>) -> Result<i32, String> {
    Ok(i32::from(!eval_test_expr(args, use_glob, facts)?))
}

fn eval_test_expr(args: &[String], use_glob: bool, facts: &ShellFacts<'_>) -> Result<bool, String> {
    // Five or more words go through the real grammar, which is where
    // `\( ... \)`, POSIX `test`'s only grouping, lives. Fewer keep the
    // shortcuts below, as in bash: at those lengths position decides what's
    // an operator, so `[ "(" = "(" ]` compares two strings.
    if args.len() >= 5 {
        let mut p = TestParser { args, at: 0, use_glob, facts };
        let value = p.or()?;
        if p.at != args.len() {
            return Err("too many arguments".to_string());
        }
        return Ok(value);
    }
    // Split on top-level `-a`/`-o`. Not strictly POSIX precedence, but right
    // for real use at these lengths.
    let mut clauses: Vec<Vec<String>> = vec![Vec::new()];
    let mut combinators: Vec<&str> = Vec::new();
    for a in args {
        // Only where there's already an operand to connect. `-o` is also a
        // unary test, and at a clause's start it's that:
        // `[ -o errexit -a -o xtrace ]` is two option tests joined by AND.
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

/// POSIX `test`'s grammar for long expressions: `-o` joins `-a`s, `-a`
/// joins negations, `!` negates a primary, and a primary is a parenthesised
/// expression or a one-, two- or three-word test from `eval_simple`. Only
/// at five words or more (see `eval_test_expr`).
struct TestParser<'a> {
    args: &'a [String],
    at: usize,
    use_glob: bool,
    facts: &'a ShellFacts<'a>,
}

impl TestParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.at).map(String::as_str)
    }

    fn or(&mut self) -> Result<bool, String> {
        let mut value = self.and()?;
        while self.peek() == Some("-o") {
            self.at += 1;
            // Both sides are evaluated: `test` doesn't short-circuit, and a
            // bad integer on the right is an error either way.
            let rhs = self.and()?;
            value = value || rhs;
        }
        Ok(value)
    }

    fn and(&mut self) -> Result<bool, String> {
        let mut value = self.not()?;
        while self.peek() == Some("-a") {
            self.at += 1;
            let rhs = self.not()?;
            value = value && rhs;
        }
        Ok(value)
    }

    fn not(&mut self) -> Result<bool, String> {
        if self.peek() == Some("!") {
            self.at += 1;
            return Ok(!self.not()?);
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<bool, String> {
        if self.peek() == Some("(") {
            self.at += 1;
            let value = self.or()?;
            if self.peek() != Some(")") {
                return Err(format!("`)' expected, found {}", self.peek().unwrap_or("end of expression")));
            }
            self.at += 1;
            return Ok(value);
        }
        // A primary's width comes from recognizing the operator, not from
        // looking ahead for a connective: taking three words wherever they
        // fit would read `[ -o errexit -o -o xtrace ]` as a comparison with
        // operator `errexit`.
        let rest = &self.args[self.at..];
        let width = match rest {
            [_, op, ..] if is_binary_op(op) => 3,
            [op, _, ..] if is_unary_op(op) => 2,
            [_, ..] => 1,
            [] => return Err("argument expected".to_string()),
        };
        self.at += width;
        eval_simple(&rest[..width], self.use_glob, self.facts)
    }
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
        [op, _] if !is_unary_op(op) => Err(format!("{op}: unary operator expected")),
        [op, a] => Ok(unary(op, a)),
        // A binary operator wins over grouping, so `[ "(" = ")" ]` compares
        // strings; only when the middle word isn't one is `( x )` the test
        // of `x`.
        [a, op, b] if !is_binary_op(op) && a == "(" && b == ")" => eval_simple(&args[1..2], use_glob, facts),
        [a, op, b] => binary_checked(a, op, b, use_glob),
        // Four words, opened and closed: the middle two as a test. (`!` plus
        // three words is handled above.)
        [open, _, _, close] if open == "(" && close == ")" => eval_simple(&args[1..3], use_glob, facts),
        // More words is no form of `test`. Reading it as "non-empty, so true"
        // would let `[ "$a" = "$b" = "$c" ]` and an unquoted variable that
        // became two words pass silently.
        _ => Err("too many arguments".to_string()),
    }
}

/// Whether a word is a binary operator: tells `[ ( x ) ]` from
/// `[ "(" = ")" ]`, and tells `[[ a -eq ]]` it's missing a right-hand
/// side.
pub(crate) fn is_binary_op(op: &str) -> bool {
    matches!(op, "=" | "==" | "!=" | "<" | ">" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" | "-nt" | "-ot" | "-ef")
}

// `binary`, reporting its one error: a numeric comparison with a
// non-number. `[[ ]]` never gets here; its operands are arithmetic, where
// an unset name is 0.
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

/// The operators `unary` answers for, kept beside it. Recognizing is
/// separate from evaluating because an unknown operator is an error, not
/// false: `[ x y ]` is "unary operator expected", status 2, so a mistyped
/// `-q` can't pass as a failed test.
///
/// bash's whole set: this list is what tells `[[ -a X ]]` (file exists)
/// from `[ x -a y ]` (and), so a missing entry would turn one into the
/// other.
const UNARY_OPS: &[&str] = &[
    "-a", "-b", "-c", "-d", "-e", "-f", "-g", "-h", "-k", "-p", "-r", "-s", "-t", "-u", "-w", "-x", "-z", "-n", "-G", "-L", "-N", "-O", "-R", "-S",
    "-v", "-o",
];

pub(crate) fn is_unary_op(op: &str) -> bool {
    UNARY_OPS.contains(&op)
}

pub(crate) fn unary(op: &str, a: &str) -> bool {
    let path = std::path::Path::new(a);
    match op {
        // `-a` is `-e`, and also `[ x -a y ]`'s `and`; which depends on the
        // word count, the caller's question.
        "-e" | "-a" => path.exists(),
        "-f" => path.is_file(),
        "-d" => path.is_dir(),
        // Asked of the kernel (`access(2)`) rather than worked out from
        // mode bits: the answer depends on who's asking.
        "-r" => accessible(a, crate::platform::Access::Read),
        "-w" => accessible(a, crate::platform::Access::Write),
        "-x" => accessible(a, crate::platform::Access::Execute),
        "-z" => a.is_empty(),
        "-n" => !a.is_empty(),
        "-s" => std::fs::metadata(a).map(|m| m.len() > 0).unwrap_or(false),
        "-L" | "-h" => std::fs::symlink_metadata(a).map(|m| m.file_type().is_symlink()).unwrap_or(false),
        "-p" => file_type_is(a, crate::platform::is_fifo),
        "-S" => file_type_is(a, crate::platform::is_socket),
        "-b" => file_type_is(a, crate::platform::is_block_device),
        "-c" => file_type_is(a, crate::platform::is_char_device),
        "-g" => mode_has(a, 0o2000),
        "-u" => mode_has(a, 0o4000),
        "-k" => mode_has(a, 0o1000),
        "-O" => owned_by(a, Owner::User),
        "-G" => owned_by(a, Owner::Group),
        // Modified since last read: mtime after atime. False for a file never
        // read.
        "-N" => std::fs::metadata(a).map(|m| newer_than(&m)).unwrap_or(false),
        // A nameref. bish has none, so false, which is the true answer,
        // rather than an error.
        "-R" => false,
        // `-t FD`: whether that descriptor is a terminal.
        "-t" => a.trim().parse::<i32>().map(crate::platform::is_terminal).unwrap_or(false),
        _ => false,
    }
}

/// Asked of the OS, which answers for the effective user (see `-r`).
fn accessible(a: &str, mode: crate::platform::Access) -> bool {
    crate::platform::is_accessible(a, mode)
}

#[cfg(unix)]
fn mode_has(a: &str, bits: u32) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(a).map(|m| m.permissions().mode() & bits != 0).unwrap_or(false)
}
#[cfg(not(unix))]
fn mode_has(_a: &str, _bits: u32) -> bool {
    false
}

/// Which of the two ownerships `-O` and `-G` ask about.
enum Owner {
    User,
    Group,
}

/// Against the effective id, as `accessible` does: who the process acts
/// as now.
#[cfg(unix)]
fn owned_by(a: &str, which: Owner) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(m) = std::fs::metadata(a) else { return false };
    match which {
        Owner::User => m.uid() == crate::platform::effective_user(),
        Owner::Group => m.gid() == crate::platform::effective_group(),
    }
}
#[cfg(not(unix))]
fn owned_by(_a: &str, _which: Owner) -> bool {
    false
}

#[cfg(unix)]
fn newer_than(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    m.mtime() > m.atime() || (m.mtime() == m.atime() && m.mtime_nsec() > m.atime_nsec())
}
#[cfg(not(unix))]
fn newer_than(_m: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn file_type_is(a: &str, pred: impl Fn(&std::fs::FileType) -> bool) -> bool {
    std::fs::metadata(a).map(|m| pred(&m.file_type())).unwrap_or(false)
}
#[cfg(not(unix))]
fn file_type_is(_a: &str, _pred: impl Fn(&std::fs::FileType) -> bool) -> bool {
    false
}

pub(crate) fn binary(a: &str, op: &str, b: &str, use_glob: bool) -> bool {
    binary_with_case(a, op, b, use_glob, false)
}

// `binary` with `shopt -s nocasematch` folding for the two operators it
// applies to. `[`/`test` never fold: they compare literally.
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
        // As in bash, `-nt` is true if `a` exists and `b` doesn't.
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
