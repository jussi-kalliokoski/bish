#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

#[repr(C)]
struct RLimit {
    cur: u64,
    max: u64,
}

const RLIM_INFINITY: u64 = u64::MAX;

struct LimitSpec {
    flag: char,
    resource: i32,
    label: &'static str,
    unit: &'static str,
    div: u64,
}

// Standard Linux/glibc RLIMIT_* numbers (stable ABI, safe to hardcode --
// same "libc is already linked, no crate needed" reasoning used elsewhere
// in this codebase for raw syscall numbers/signatures). No RLIMIT_PIPE
// exists on Linux (pipe capacity is a per-pipe fcntl setting, not an
// rlimit) so `-p`, which real bash reports as a fixed constant, is left
// out of `-a` rather than fabricating a value.
const LIMIT_SPECS: &[LimitSpec] = &[
    LimitSpec { flag: 'c', resource: 4, label: "core file size", unit: "blocks", div: 512 },
    LimitSpec { flag: 'd', resource: 2, label: "data seg size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'e', resource: 13, label: "scheduling priority", unit: "", div: 1 },
    LimitSpec { flag: 'f', resource: 1, label: "file size", unit: "blocks", div: 512 },
    LimitSpec { flag: 'i', resource: 11, label: "pending signals", unit: "", div: 1 },
    LimitSpec { flag: 'l', resource: 8, label: "max locked memory", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'm', resource: 5, label: "max memory size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'n', resource: 7, label: "open files", unit: "", div: 1 },
    LimitSpec { flag: 'q', resource: 12, label: "POSIX message queues", unit: "bytes", div: 1 },
    LimitSpec { flag: 'r', resource: 14, label: "real-time priority", unit: "", div: 1 },
    LimitSpec { flag: 's', resource: 3, label: "stack size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 't', resource: 0, label: "cpu time", unit: "seconds", div: 1 },
    LimitSpec { flag: 'u', resource: 6, label: "max user processes", unit: "", div: 1 },
    LimitSpec { flag: 'v', resource: 9, label: "virtual memory", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'x', resource: 10, label: "file locks", unit: "", div: 1 },
];

fn fmt_limit(v: u64, div: u64) -> String {
    if v == RLIM_INFINITY {
        "unlimited".to_string()
    } else {
        (v / div.max(1)).to_string()
    }
}

// ulimit [-HS] [-a] [-cdefilmnqrstuvx [limit]]. `-a` doesn't attempt to
// byte-match bash's exact column alignment (its internal padding rules
// aren't a fixed width across all entries) -- purely cosmetic output that
// scripts don't parse, unlike the single-limit query/set forms below,
// which do match exactly.
pub fn ulimit(args: &[String]) -> i32 {
    unsafe extern "C" {
        fn getrlimit(resource: i32, rlim: *mut RLimit) -> i32;
        fn setrlimit(resource: i32, rlim: *const RLimit) -> i32;
    }
    let mut hard = false;
    let mut soft = false;
    let mut show_all = false;
    let mut flag: Option<char> = None;
    let mut value: Option<String> = None;
    for a in args {
        if let Some(rest) = a.strip_prefix('-').filter(|r| !r.is_empty()) {
            for c in rest.chars() {
                match c {
                    'H' => hard = true,
                    'S' => soft = true,
                    'a' => show_all = true,
                    other => flag = Some(other),
                }
            }
        } else {
            value = Some(a.clone());
        }
    }
    if show_all {
        for spec in LIMIT_SPECS {
            let mut rl = RLimit { cur: 0, max: 0 };
            unsafe {
                getrlimit(spec.resource, &mut rl);
            }
            let v = if hard { rl.max } else { rl.cur };
            let unit_part = if spec.unit.is_empty() { String::new() } else { format!("{}, ", spec.unit) };
            println!("{:<24}({}-{}) {}", spec.label, unit_part, spec.flag, fmt_limit(v, spec.div));
        }
        return 0;
    }
    let f = flag.unwrap_or('f');
    let spec = match LIMIT_SPECS.iter().find(|s| s.flag == f) {
        Some(s) => s,
        None => {
            eprintln!("bish: ulimit: -{}: invalid option", f);
            return 1;
        }
    };
    let mut rl = RLimit { cur: 0, max: 0 };
    unsafe {
        getrlimit(spec.resource, &mut rl);
    }
    match value {
        None => {
            let v = if hard { rl.max } else { rl.cur };
            println!("{}", fmt_limit(v, spec.div));
            0
        }
        Some(v) => {
            let new_val: u64 = if v == "unlimited" {
                RLIM_INFINITY
            } else {
                match v.parse::<u64>() {
                    Ok(n) => n * spec.div,
                    Err(_) => {
                        eprintln!("bish: ulimit: {}: invalid number", v);
                        return 1;
                    }
                }
            };
            if !soft && !hard {
                rl.cur = new_val;
                rl.max = new_val;
            } else {
                if soft {
                    rl.cur = new_val;
                }
                if hard {
                    rl.max = new_val;
                }
            }
            if unsafe { setrlimit(spec.resource, &rl) } != 0 {
                eprintln!("bish: ulimit: cannot modify limit: {}", std::io::Error::last_os_error());
                return 1;
            }
            0
        }
    }
}

// POSIX has no query-only umask read -- `umask(new) -> previous` is the
// only syscall shape, so reading the current value means setting a
// throwaway mask and immediately restoring what was there.
pub fn umask(args: &[String]) -> i32 {
    unsafe extern "C" {
        fn umask(mask: u32) -> u32;
    }
    let symbolic = args.iter().any(|a| a == "-S");
    match args.iter().find(|a| !a.starts_with('-')) {
        Some(s) => match u32::from_str_radix(s, 8) {
            Ok(m) => {
                unsafe {
                    umask(m);
                }
                0
            }
            Err(_) => {
                eprintln!("bish: umask: {}: invalid octal number", s);
                1
            }
        },
        None => {
            let cur = unsafe {
                let prev = umask(0);
                umask(prev);
                prev
            };
            if symbolic {
                println!("{}", umask_symbolic(cur));
            } else {
                println!("{:04o}", cur);
            }
            0
        }
    }
}

fn umask_symbolic(mask: u32) -> String {
    let perm_for = |shift: u32| -> String {
        let bits = (mask >> shift) & 0o7;
        let mut s = String::new();
        if bits & 0o4 == 0 {
            s.push('r');
        }
        if bits & 0o2 == 0 {
            s.push('w');
        }
        if bits & 0o1 == 0 {
            s.push('x');
        }
        s
    };
    format!("u={},g={},o={}", perm_for(6), perm_for(3), perm_for(0))
}

pub fn cd(args: &[String]) -> i32 {
    let old = std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned());
    let target = if let Some(dir) = args.first() {
        if dir == "-" {
            match std::env::var("OLDPWD") {
                Ok(p) => {
                    println!("{}", p);
                    p
                }
                Err(_) => {
                    eprintln!("cd: OLDPWD not set");
                    return 1;
                }
            }
        } else {
            dir.clone()
        }
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
        Ok(()) => {
            unsafe {
                if let Some(o) = old {
                    std::env::set_var("OLDPWD", o);
                }
                if let Ok(new_pwd) = std::env::current_dir() {
                    std::env::set_var("PWD", new_pwd.to_string_lossy().into_owned());
                }
            }
            0
        }
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
