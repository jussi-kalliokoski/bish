// `history` and `fc`.

use crate::exec::{Shell, sh_eprintln, sh_println};
use std::rc::Rc;

// `history [N]`, `history -c`, `history -d N`. Not `-w`/`-r`/`-a`: the
// history file is appended to by every running bish (see history.rs's
// `load`), so writing one shell's list back would mean deciding whose
// list wins.
pub(crate) fn run_history(sh: &mut Shell, args: &[String]) -> i32 {
    let history = Rc::clone(&sh.history);
    match args.first().map(String::as_str) {
        Some("-c") => {
            history.borrow_mut().clear();
            0
        }
        Some("-d") => {
            let Some(n) = args.get(1).and_then(|a| a.parse::<usize>().ok()) else {
                sh_eprintln!(sh, "bish: history: -d: usage: history -d OFFSET");
                return 2;
            };
            if history.borrow_mut().delete(n) {
                0
            } else {
                sh_eprintln!(sh, "bish: history: {n}: history position out of range");
                1
            }
        }
        Some(flag) if flag.starts_with('-') => {
            sh_eprintln!(sh, "bish: history: {flag}: invalid option (expected -c or -d)");
            2
        }
        other => {
            let entries = history.borrow().entries();
            // A bare count shows the last N.
            let start = match other.and_then(|a| a.parse::<usize>().ok()) {
                Some(n) => entries.len().saturating_sub(n),
                None => 0,
            };
            // `HISTTIMEFORMAT`, as in bash: when set, each line starts with
            // when the command ran. An entry with no time gets padding,
            // keeping the commands aligned.
            let time_format = sh.var_is_set("HISTTIMEFORMAT").then(|| sh.raw_var_lookup("HISTTIMEFORMAT"));
            for (i, (entry, when)) in entries.iter().enumerate().skip(start) {
                match &time_format {
                    Some(fmt) if !fmt.is_empty() => {
                        let stamp = match when {
                            Some(secs) => crate::time::strftime_at(fmt, &crate::time::local_time_at(*secs), Some(*secs)),
                            None => String::new(),
                        };
                        sh_println!(sh, "{:5}  {}{}", i + 1, stamp, entry);
                    }
                    _ => sh_println!(sh, "{:5}  {}", i + 1, entry),
                }
            }
            0
        }
    }
}

// `fc -l [first [last]]`, the listing. Bare `fc` runs whatever comes back
// from editing the last command, which is much sharper than a listing,
// so it's refused by name, this shell's convention for something not
// implemented.
pub(crate) fn run_fc(sh: &mut Shell, args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("-l") {
        sh_eprintln!(sh, "bish: fc: only `fc -l [first [last]]` is implemented (use `history` to list, and an editor to edit)");
        return 2;
    }
    let entries = sh.history.borrow().entries();
    if entries.is_empty() {
        return 0;
    }
    // bash's defaults: the last 16, and a negative number counts back from
    // the end.
    let resolve = |arg: Option<&String>, fallback: usize| -> usize {
        match arg.and_then(|a| a.parse::<i64>().ok()) {
            Some(n) if n < 0 => entries.len().saturating_sub((-n) as usize).saturating_add(1).max(1),
            Some(n) if n > 0 => (n as usize).min(entries.len()),
            _ => fallback,
        }
    };
    let first = resolve(args.get(1), entries.len().saturating_sub(15).max(1));
    let last = resolve(args.get(2), entries.len());
    let (lo, hi) = (first.min(last), first.max(last));
    for n in lo..=hi.min(entries.len()) {
        sh_println!(sh, "{:5}\t{}", n, entries[n - 1].0);
    }
    0
}
