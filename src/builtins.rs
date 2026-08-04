pub fn cd(args: &[String]) -> i32 {
    let target = if let Some(dir) = args.first() {
        dir.clone()
    } else {
        match std::env::var("HOME") {
            Ok(h) => h,
            Err(_) => {
                eprintln!("cd: HOME not set");
                return 1;
            }
        }
    };
    match std::env::set_current_dir(&target) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("cd: {}: {}", target, e);
            1
        }
    }
}

// Variables are always process env vars in v1 (no local-vs-exported
// distinction yet), so `export NAME` with no '=' is a no-op.
pub fn export(args: &[String]) -> i32 {
    for a in args {
        if let Some(eq) = a.find('=') {
            unsafe {
                std::env::set_var(&a[..eq], &a[eq + 1..]);
            }
        }
    }
    0
}

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
        _ => false,
    }
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
        "-eq" => num(a) == num(b),
        "-ne" => num(a) != num(b),
        "-lt" => num(a) < num(b),
        "-le" => num(a) <= num(b),
        "-gt" => num(a) > num(b),
        "-ge" => num(a) >= num(b),
        _ => false,
    }
}

fn num(s: &str) -> i64 {
    s.trim().parse().unwrap_or(0)
}
