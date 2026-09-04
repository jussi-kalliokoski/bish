// `ulimit`, `umask` and `times`: the three builtins that ask the kernel
// about this process rather than about the shell.
//
// Free functions taking `&mut Shell` rather than methods on it -- see
// `builtins/mod.rs` for why the whole family moved out of `impl Shell`.

use crate::exec::{Shell, current_umask, sh_eprintln, sh_println};

// Raw libc, the same way the rest of this codebase reaches for it: it
// is already linked, and std has no wrapper. `getrlimit`/`setrlimit`
// are declared inside the one function that calls them, which is this
// codebase's own habit for a one-call-site FFI.
unsafe extern "C" {
    fn umask(mask: u32) -> u32;
}

// Support types for run_ulimit (moved from a builtins.rs free function in
// M6 -- see that method's doc comment).
#[repr(C)]
struct RLimit {
    cur: u64,
    max: u64,
}

const RLIM_INFINITY: u64 = u64::MAX;

// Sentinel `resource` for a limit the kernel has no rlimit for -- see
// the `-p` entry in LIMIT_SPECS.
const RESOURCE_FIXED: i32 = -1;

struct LimitSpec {
    flag: char,
    resource: i32,
    label: &'static str,
    unit: &'static str,
    div: u64,
}

// Standard Linux/glibc RLIMIT_* numbers (stable ABI, safe to hardcode --
// same "libc is already linked, no crate needed" reasoning used elsewhere
// in this codebase for raw syscall numbers/signatures).
//
// Order matches real bash's own `-a` listing: -R first, then the rest
// alphabetically by flag.
const LIMIT_SPECS: &[LimitSpec] = &[
    LimitSpec { flag: 'R', resource: 15, label: "real-time non-blocking time", unit: "microseconds", div: 1 },
    LimitSpec { flag: 'c', resource: 4, label: "core file size", unit: "blocks", div: 512 },
    LimitSpec { flag: 'd', resource: 2, label: "data seg size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'e', resource: 13, label: "scheduling priority", unit: "", div: 1 },
    LimitSpec { flag: 'f', resource: 1, label: "file size", unit: "blocks", div: 512 },
    LimitSpec { flag: 'i', resource: 11, label: "pending signals", unit: "", div: 1 },
    LimitSpec { flag: 'l', resource: 8, label: "max locked memory", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'm', resource: 5, label: "max memory size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'n', resource: 7, label: "open files", unit: "", div: 1 },
    // No RLIMIT_PIPE exists on Linux -- pipe capacity is a per-pipe
    // fcntl setting, not an rlimit -- so bash reports a fixed 8 (i.e.
    // POSIX's 4096-byte guarantee, in 512-byte blocks) and refuses to
    // set it. RESOURCE_FIXED marks that; the value lives in `div`.
    LimitSpec { flag: 'p', resource: RESOURCE_FIXED, label: "pipe size", unit: "512 bytes", div: 8 },
    LimitSpec { flag: 'q', resource: 12, label: "POSIX message queues", unit: "bytes", div: 1 },
    LimitSpec { flag: 'r', resource: 14, label: "real-time priority", unit: "", div: 1 },
    LimitSpec { flag: 's', resource: 3, label: "stack size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 't', resource: 0, label: "cpu time", unit: "seconds", div: 1 },
    LimitSpec { flag: 'u', resource: 6, label: "max user processes", unit: "", div: 1 },
    LimitSpec { flag: 'v', resource: 9, label: "virtual memory", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'x', resource: 10, label: "file locks", unit: "", div: 1 },
];

// Where bash puts the `)` of the `(unit, -X)` column in `ulimit -a`.
const PAREN_COLUMN: usize = 40;

// One limit's current value as `-a` and the single-limit query form both
// want it -- including the `-p` entry, which no getrlimit can answer.
fn read_limit(spec: &LimitSpec, hard: bool) -> String {
    if spec.resource == RESOURCE_FIXED {
        return spec.div.to_string();
    }
    unsafe extern "C" {
        fn getrlimit(resource: i32, rlim: *mut RLimit) -> i32;
    }
    let mut rl = RLimit { cur: 0, max: 0 };
    unsafe {
        getrlimit(spec.resource, &mut rl);
    }
    fmt_limit(if hard { rl.max } else { rl.cur }, spec.div)
}

fn fmt_limit(v: u64, div: u64) -> String {
    if v == RLIM_INFINITY { "unlimited".to_string() } else { (v / div.max(1)).to_string() }
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

// ulimit [-HS] [-a] [-cdefilmnqrstuvx [limit]]. `-a` doesn't attempt to
// byte-match bash's exact column alignment (its internal padding rules
// aren't a fixed width across all entries) -- purely cosmetic output
// that scripts don't parse, unlike the single-limit query/set forms
// below, which do match exactly. Moved here from a builtins.rs free
// function (M6) so its output goes through sh.sink_out/sink_err
// like every other builtin's, instead of always writing straight to
// the real stdout/stderr regardless of which session ran it.
pub(crate) fn run_ulimit(sh: &mut Shell, args: &[String]) -> i32 {
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
            let unit_part = if spec.unit.is_empty() { String::new() } else { format!("{}, ", spec.unit) };
            let group = format!("({}-{})", unit_part, spec.flag);
            // bash right-aligns the closing paren at column 40, keeping
            // at least two spaces after the label -- which is why the
            // one over-long label (`-R`) simply pushes past it.
            let pad = (PAREN_COLUMN.saturating_sub(spec.label.len() + group.len())).max(2);
            sh_println!(sh, "{}{}{} {}", spec.label, " ".repeat(pad), group, read_limit(spec, hard));
        }
        return 0;
    }
    let f = flag.unwrap_or('f');
    let spec = match LIMIT_SPECS.iter().find(|s| s.flag == f) {
        Some(s) => s,
        None => {
            sh_eprintln!(sh, "bish: ulimit: -{}: invalid option", f);
            return 1;
        }
    };
    let mut rl = RLimit { cur: 0, max: 0 };
    if spec.resource != RESOURCE_FIXED {
        unsafe {
            getrlimit(spec.resource, &mut rl);
        }
    }
    match value {
        None => {
            sh_println!(sh, "{}", read_limit(spec, hard));
            0
        }
        Some(v) => {
            if spec.resource == RESOURCE_FIXED {
                sh_eprintln!(sh, "bish: ulimit: {}: cannot modify limit: Invalid argument", spec.label);
                return 1;
            }
            let new_val: u64 = if v == "unlimited" {
                RLIM_INFINITY
            } else {
                match v.parse::<u64>() {
                    Ok(n) => n * spec.div,
                    Err(_) => {
                        sh_eprintln!(sh, "bish: ulimit: {}: invalid number", v);
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
                sh_eprintln!(sh, "bish: ulimit: cannot modify limit: {}", std::io::Error::last_os_error());
                return 1;
            }
            0
        }
    }
}

// `times` -- CPU consumed by this shell and by the commands it has
// waited for, as POSIX specifies: two lines, user then system on
// each, the shell's own first and its children's second.
//
// Straight from `times(2)`, which reports all four in one call, in
// clock ticks. `sysconf(_SC_CLK_TCK)` is the divisor -- 100 on every
// Linux worth naming, but asking costs nothing and hard-coding it is
// the sort of thing that is wrong exactly once and mysteriously.
pub(crate) fn run_times(sh: &mut Shell, args: &[String]) -> i32 {
    if !args.is_empty() {
        sh_eprintln!(sh, "bish: times: too many arguments");
        return 2;
    }
    #[repr(C)]
    struct Tms {
        utime: i64,
        stime: i64,
        cutime: i64,
        cstime: i64,
    }
    unsafe extern "C" {
        fn times(buf: *mut Tms) -> i64;
        fn sysconf(name: i32) -> i64;
    }
    // _SC_CLK_TCK
    const SC_CLK_TCK: i32 = 2;
    let mut tms = Tms { utime: 0, stime: 0, cutime: 0, cstime: 0 };
    if unsafe { times(&mut tms as *mut Tms) } == -1 {
        sh_eprintln!(sh, "bish: times: cannot read process times");
        return 1;
    }
    let ticks = unsafe { sysconf(SC_CLK_TCK) }.max(1);
    // bash's own shape: whole minutes, then seconds to milliseconds.
    let show = |t: i64| {
        let secs = t as f64 / ticks as f64;
        format!("{}m{:.3}s", (secs as i64) / 60, secs % 60.0)
    };
    sh_println!(sh, "{} {}", show(tms.utime), show(tms.stime));
    sh_println!(sh, "{} {}", show(tms.cutime), show(tms.cstime));
    0
}

pub(crate) fn run_umask(sh: &mut Shell, args: &[String]) -> i32 {
    // Clustered, like every other builtin's: `umask -pS` is `-p -S`.
    let has = |want: char| args.iter().filter(|a| a.len() > 1 && a.starts_with('-')).any(|a| a.chars().skip(1).any(|c| c == want));
    let symbolic = has('S');
    match args.iter().find(|a| !a.starts_with('-')) {
        Some(s) => match u32::from_str_radix(s, 8) {
            Ok(m) => {
                unsafe {
                    umask(m);
                }
                // Keep this session's own remembered umask in lockstep
                // -- see sync_real_state_in/out's own doc comment for
                // why a mutation of this real, process-wide syscall
                // needs a Shell-owned mirror at all.
                sh.umask_snapshot = m;
                0
            }
            Err(_) => {
                sh_eprintln!(sh, "bish: umask: {}: invalid octal number", s);
                1
            }
        },
        None => {
            let cur = current_umask();
            // `-p` prints it as the command that would set it again,
            // which is the whole point of the flag: `umask -p` into a
            // file, source it back, same mask. It was being accepted
            // and ignored, so the output could not be re-read.
            let prefix = match (has('p'), symbolic) {
                (true, true) => "umask -S ",
                (true, false) => "umask ",
                (false, _) => "",
            };
            if symbolic {
                sh_println!(sh, "{}{}", prefix, umask_symbolic(cur));
            } else {
                sh_println!(sh, "{}{:04o}", prefix, cur);
            }
            0
        }
    }
}
