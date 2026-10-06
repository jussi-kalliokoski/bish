// `ulimit`, `umask` and `times`: the three builtins that ask the kernel
// about this process rather than about the shell.
//
// Free functions taking `&mut Shell` rather than methods on it -- see
// `builtins/mod.rs` for why the whole family moved out of `impl Shell`.

use crate::exec::{Shell, current_umask, sh_eprintln, sh_println};

use crate::platform::{ResourceLimit, UNLIMITED};

struct LimitSpec {
    flag: char,
    label: &'static str,
    unit: &'static str,
    div: u64,
}

/// Which `RLIMIT_*` a flag asks about -- the platform layer's answer,
/// since the numbers differ per OS and six of them do not exist on macOS
/// at all. `None` also covers `-p`, which no OS has an rlimit for.
fn resource_of(spec: &LimitSpec) -> Option<i32> {
    match spec.flag {
        'p' => None,
        flag => crate::platform::resource_for_flag(flag),
    }
}

// What each flag is called and how it is counted -- bish's own half of
// `ulimit`. The resource *numbers* are the OS's, and `resource_of` asks
// for them, so a limit this OS does not have is simply absent from `-a`
// rather than listed with a number that means something else here.
//
// Order matches real bash's own `-a` listing: -R first, then the rest
// alphabetically by flag.
const LIMIT_SPECS: &[LimitSpec] = &[
    LimitSpec { flag: 'R', label: "real-time non-blocking time", unit: "microseconds", div: 1 },
    LimitSpec { flag: 'c', label: "core file size", unit: "blocks", div: 512 },
    LimitSpec { flag: 'd', label: "data seg size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'e', label: "scheduling priority", unit: "", div: 1 },
    LimitSpec { flag: 'f', label: "file size", unit: "blocks", div: 512 },
    LimitSpec { flag: 'i', label: "pending signals", unit: "", div: 1 },
    LimitSpec { flag: 'l', label: "max locked memory", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'm', label: "max memory size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'n', label: "open files", unit: "", div: 1 },
    // No RLIMIT_PIPE exists on Linux -- pipe capacity is a per-pipe
    // fcntl setting, not an rlimit -- so bash reports a fixed 8 (i.e.
    // POSIX's 4096-byte guarantee, in 512-byte blocks) and refuses to
    // set it. RESOURCE_FIXED marks that; the value lives in `div`.
    LimitSpec { flag: 'p', label: "pipe size", unit: "512 bytes", div: 8 },
    LimitSpec { flag: 'q', label: "POSIX message queues", unit: "bytes", div: 1 },
    LimitSpec { flag: 'r', label: "real-time priority", unit: "", div: 1 },
    LimitSpec { flag: 's', label: "stack size", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 't', label: "cpu time", unit: "seconds", div: 1 },
    LimitSpec { flag: 'u', label: "max user processes", unit: "", div: 1 },
    LimitSpec { flag: 'v', label: "virtual memory", unit: "kbytes", div: 1024 },
    LimitSpec { flag: 'x', label: "file locks", unit: "", div: 1 },
];

// Where bash puts the `)` of the `(unit, -X)` column in `ulimit -a`.
const PAREN_COLUMN: usize = 40;

// One limit's current value as `-a` and the single-limit query form both
// want it -- including the `-p` entry, which no getrlimit can answer.
fn read_limit(spec: &LimitSpec, hard: bool) -> String {
    let Some(resource) = resource_of(spec) else {
        // `-p`: a fixed answer, with the value kept in `div`.
        return spec.div.to_string();
    };
    let limit = crate::platform::resource_limit(resource);
    fmt_limit(if hard { limit.hard } else { limit.soft }, spec.div)
}

fn fmt_limit(v: u64, div: u64) -> String {
    if v == UNLIMITED { "unlimited".to_string() } else { (v / div.max(1)).to_string() }
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
        // A limit this OS does not have is left out rather than shown
        // with a number from the other one -- `-p` excepted, which has no
        // rlimit anywhere and a fixed answer.
        for spec in LIMIT_SPECS.iter().filter(|s| s.flag == 'p' || resource_of(s).is_some()) {
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
    let resource = resource_of(spec);
    let mut rl = resource.map(crate::platform::resource_limit).unwrap_or_default();
    match value {
        None => {
            sh_println!(sh, "{}", read_limit(spec, hard));
            0
        }
        Some(v) => {
            let Some(resource) = resource else {
                sh_eprintln!(sh, "bish: ulimit: {}: cannot modify limit: Invalid argument", spec.label);
                return 1;
            };
            let new_val: u64 = if v == "unlimited" {
                UNLIMITED
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
                rl = ResourceLimit { soft: new_val, hard: new_val };
            } else {
                if soft {
                    rl.soft = new_val;
                }
                if hard {
                    rl.hard = new_val;
                }
            }
            if let Err(failure) = crate::platform::set_resource_limit(resource, &rl) {
                sh_eprintln!(sh, "bish: ulimit: cannot modify limit: {failure}");
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
// Straight from `times(2)`, which reports all four in one call. The
// platform layer does the dividing: the tick rate is asked for by a
// `sysconf` name that is 2 on Linux and 3 on macOS, and asking for the
// wrong one answers about something else entirely.
pub(crate) fn run_times(sh: &mut Shell, args: &[String]) -> i32 {
    if !args.is_empty() {
        sh_eprintln!(sh, "bish: times: too many arguments");
        return 2;
    }
    let Some((user, system, child_user, child_system)) = crate::platform::cpu_seconds() else {
        sh_eprintln!(sh, "bish: times: cannot read process times");
        return 1;
    };
    // bash's own shape: whole minutes, then seconds to milliseconds.
    let show = |secs: f64| format!("{}m{:.3}s", (secs as i64) / 60, secs % 60.0);
    sh_println!(sh, "{} {}", show(user), show(system));
    sh_println!(sh, "{} {}", show(child_user), show(child_system));
    0
}

pub(crate) fn run_umask(sh: &mut Shell, args: &[String]) -> i32 {
    // Clustered, like every other builtin's: `umask -pS` is `-p -S`.
    let has = |want: char| args.iter().filter(|a| a.len() > 1 && a.starts_with('-')).any(|a| a.chars().skip(1).any(|c| c == want));
    let symbolic = has('S');
    match args.iter().find(|a| !a.starts_with('-')) {
        Some(s) => match u32::from_str_radix(s, 8) {
            Ok(m) => {
                sh.note_umask_change();
                crate::platform::set_umask(m);
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
