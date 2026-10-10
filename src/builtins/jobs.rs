// Job control: the builtins that name a job and act on it.

use crate::exec::{ExecResult, JobWaitOutcome, SIGCONT, Shell, getpgrp, send_signal, send_signal_to_pgrp, sh_eprintln, sh_println, waitpid_untraced};
use crate::pty;

// One line of `jobs` output in bash's columns: the id and its `+`/`-`,
// the pid with `-l`, the status padded to 24, then the command. `suffix`
// is the ` &` a background job carries and a stopped one doesn't.
fn job_line(job: &crate::exec::Job, mark: &str, long: bool, status: &str, suffix: &str) -> String {
    let pid = match long {
        true => job.pids.first().map(|p| format!("{p} ")).unwrap_or_default(),
        false => " ".to_string(),
    };
    format!("[{}]{} {}{:<27}{}{}", job.id, mark, pid, status, job.cmd_text, suffix)
}

pub(crate) fn run_jobs(sh: &mut Shell, args: &[String]) -> i32 {
    // The flags shape nothing beyond what's handled below, but an unknown
    // letter is a typo worth reporting.
    if let Some(bad) = crate::exec::first_unknown_option(args, "lnprsx") {
        let usage = "jobs [-lnprs] [jobspec ...] or jobs -x command [args]";
        return crate::exec::bad_option_status(sh, "jobs", &bad, usage);
    }
    // `-p` is the pids alone, for `kill $(jobs -p)`; `-l` adds each pid to
    // the normal listing.
    let pids_only = args.iter().any(|a| a == "-p");
    let long = args.iter().any(|a| a == "-l");
    // `-r` and `-s` narrow the listing to running or stopped jobs.
    let running_only = args.iter().any(|a| a == "-r");
    let stopped_only = args.iter().any(|a| a == "-s");
    if pids_only {
        let mut table = sh.jobs.borrow_mut();
        let mut done = Vec::new();
        for (i, job) in table.jobs.iter_mut().enumerate() {
            match (job.stopped, job.poll()) {
                (false, Some(_)) => done.push(i),
                _ => {
                    if let Some(pid) = job.pids.first() {
                        sh_println!(sh, "{pid}");
                    }
                }
            }
        }
        for i in done.into_iter().rev() {
            table.jobs.remove(i);
        }
        return 0;
    }
    let mut table = sh.jobs.borrow_mut();
    let last_idx = table.jobs.len().checked_sub(1);
    let prev_idx = table.jobs.len().checked_sub(2);
    let mut to_remove = Vec::new();
    for (i, job) in table.jobs.iter_mut().enumerate() {
        let mark = if Some(i) == last_idx {
            "+"
        } else if Some(i) == prev_idx {
            "-"
        } else {
            " "
        };
        // Checked before `Job::poll`, which only wraps `Child::try_wait`
        // and never sees a stop (no WUNTRACED), so a stopped job would look
        // running forever (see `Job::stopped`).
        if job.stopped {
            // No trailing ` &`: bash shows it only for jobs launched with
            // `&`, not one stopped from the foreground.
            if !running_only {
                sh_println!(sh, "{}", job_line(job, mark, long, "Stopped", ""));
            }
            continue;
        }
        match job.poll() {
            Some(_) => {
                // A finished job is neither running nor stopped, so a
                // narrowed listing omits it, but it's still reaped.
                if !running_only && !stopped_only {
                    sh_println!(sh, "{}", job_line(job, mark, long, "Done", " &"));
                }
                to_remove.push(i);
            }
            None => {
                if !stopped_only {
                    sh_println!(sh, "{}", job_line(job, mark, long, "Running", " &"));
                }
            }
        }
    }
    for i in to_remove.into_iter().rev() {
        table.jobs.remove(i);
    }
    0
}

// `disown [-a|-r] [%job|pid...]`: removes jobs from the table without
// touching the processes. Dropping a `Child` never kills it, so removing
// the entry is enough. bish sends no SIGHUP to background jobs on exit,
// so there's nothing else to protect them from. Bare `disown` takes the
// current job, as in bash.
pub(crate) fn run_disown(sh: &mut Shell, args: &[String]) -> i32 {
    let mut all = false;
    let mut running_only = false;
    // `-h` keeps the job and only marks it not to get SIGHUP on exit. bish
    // sends none (`huponexit` is listed and off), so the job just stays. It
    // has to parse as a flag, not a job spec.
    let mut keep = false;
    let mut specs: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "-a" => all = true,
            "-r" => running_only = true,
            "-h" => keep = true,
            _ => specs.push(a),
        }
    }
    if keep {
        return 0;
    }
    if all {
        sh.jobs.borrow_mut().jobs.clear();
        return 0;
    }
    if running_only && specs.is_empty() {
        sh.jobs.borrow_mut().jobs.retain(|j| j.stopped);
        return 0;
    }
    if specs.is_empty() {
        let mut table = sh.jobs.borrow_mut();
        if table.jobs.is_empty() {
            sh_eprintln!(sh, "bish: disown: current: no such job");
            return 1;
        }
        table.jobs.pop();
        return 0;
    }
    // `resolve_job_spec` borrows `sh.jobs`, so every spec is resolved to an
    // index before the one mutable borrow that removes them, highest index
    // first so earlier removals don't shift later ones.
    let mut status = 0;
    let mut idxs: Vec<usize> = Vec::new();
    for s in specs {
        match sh.resolve_job_spec(s) {
            Some(i) => idxs.push(i),
            None => {
                sh_eprintln!(sh, "bish: disown: {}: no such job", s);
                status = 1;
            }
        }
    }
    crate::sort::sort(&mut idxs);
    idxs.dedup();
    let mut table = sh.jobs.borrow_mut();
    for idx in idxs.into_iter().rev() {
        table.jobs.remove(idx);
    }
    status
}

// A pty-attached job (`Job::pty_master`, a promoted, unredirected
// background job) returns `ExecResult::Fg` without blocking; repl.rs's
// `take_pending_fg` and `drive_fg_job` run the loop (see that variant).
//
// A job in its own process group (`Job::pgid`) gets the terminal: SIGCONT
// if stopped, `tcsetpgrp` to it, a wait that also sees it stop again
// (`waitpid_untraced`, where `Job::wait` can't), then the terminal back.
//
// Anything else (a multi-stage pipeline, a job from before `set -m`) is a
// plain blocking wait, which scripts can't tell apart.
pub(crate) fn run_fg(sh: &mut Shell, args: &[String]) -> ExecResult {
    if !sh.opt_monitor {
        sh_eprintln!(sh, "bish: fg: no job control");
        return ExecResult::Status(1);
    }
    let idx = match args.first() {
        Some(spec) => sh.resolve_job_spec(spec),
        None => sh.jobs.borrow().jobs.len().checked_sub(1),
    };
    match idx {
        Some(i) => {
            let mut job = {
                let mut table = sh.jobs.borrow_mut();
                sh_println!(sh, "{}", table.jobs[i].cmd_text);
                table.jobs.remove(i)
            };
            if job.pty_master.is_some() {
                // A stopped job has to be continued before `drive_fg_job`
                // takes over, or that loop forwards keys (Ctrl-C and Ctrl-Z
                // included) to a pty whose only reader is frozen, and the
                // shell looks hung. A pty-attached job's pgid is its pid
                // (see `Job::pgid`), so signalling that group reaches it.
                if job.stopped {
                    if let Some(&pid) = job.pids.first() {
                        send_signal_to_pgrp(pid, SIGCONT);
                    }
                    job.stopped = false;
                }
                sh.pending_fg = Some(job);
                return ExecResult::Fg;
            }
            if let Some(pgid) = job.pgid {
                if job.stopped {
                    send_signal_to_pgrp(pgid, SIGCONT);
                    job.stopped = false;
                }
                pty::tcsetpgrp(0, pgid as i32).ok();
                let outcome = waitpid_untraced(job.pids[0]);
                pty::tcsetpgrp(0, getpgrp()).ok();
                return match outcome {
                    JobWaitOutcome::Exited(status) => ExecResult::Status(status),
                    JobWaitOutcome::Stopped(_sig) => {
                        job.stopped = true;
                        let id = job.id;
                        let cmd_text = job.cmd_text.clone();
                        sh.jobs.borrow_mut().jobs.push(job);
                        sh_println!(sh, "\n[{}]+  Stopped                 {}", id, cmd_text);
                        ExecResult::Status(148)
                    }
                };
            }
            ExecResult::Status(job.wait())
        }
        None => {
            sh_eprintln!(sh, "bish: fg: no current job");
            ExecResult::Status(1)
        }
    }
}

// Resumes a stopped job in place without giving it the terminal: SIGCONT
// to its group is enough. It keeps its stdio but isn't the foreground
// group, so it gets no Ctrl-C or Ctrl-Z, and the kernel stops it on
// SIGTTIN/SIGTTOU if it reads the terminal. A job already running has
// nothing to resume: as in bash, `bg` says "already in background" and
// returns 0.
pub(crate) fn run_bg(sh: &mut Shell, args: &[String]) -> i32 {
    if !sh.opt_monitor {
        sh_eprintln!(sh, "bish: bg: no job control");
        return 1;
    }
    let idx = match args.first() {
        Some(spec) => sh.resolve_job_spec(spec),
        None => sh.jobs.borrow().jobs.len().checked_sub(1),
    };
    match idx {
        Some(i) => {
            let mut table = sh.jobs.borrow_mut();
            let job = &mut table.jobs[i];
            if job.stopped {
                // As in `run_fg`: a pty-attached job's pid is its pgid.
                if let Some(pid) = job.pgid.or_else(|| job.pids.first().copied()) {
                    send_signal_to_pgrp(pid, SIGCONT);
                }
                job.stopped = false;
                sh_println!(sh, "[{}]+  {} &", job.id, job.cmd_text);
                return 0;
            }
            sh_eprintln!(sh, "bish: bg: job {} already in background", job.id);
            0
        }
        None => {
            sh_eprintln!(sh, "bish: bg: no current job");
            1
        }
    }
}

// `wait -n [id ...]`: blocks until the next of the named jobs (or of any
// job) finishes and returns its status; 127 when there's nothing to wait
// for, which is how `while wait -n; do ...; done` ends.
//
// A poll loop rather than a blocking `waitpid(-1)`: the children belong to
// `std::process::Child`, and reaping one behind its back would leave its
// later `try_wait` with a status that's gone. `Job::poll` is the one
// reaping path.
fn wait_for_next(sh: &mut Shell, ids: &[String]) -> i32 {
    // Resolved once: an index shifts as finished jobs go, an id doesn't.
    let watched: Option<Vec<u32>> = if ids.is_empty() {
        None
    } else {
        let mut out = Vec::new();
        for a in ids {
            if let Some(idx) = sh.resolve_job_spec(a) {
                out.push(sh.jobs.borrow().jobs[idx].id);
                continue;
            }
            // `wait -n` words an unusable id its own way, rejecting the
            // spec before looking for a process.
            let found = a.parse::<u32>().ok().and_then(|pid| sh.jobs.borrow().jobs.iter().find(|j| j.pids.contains(&pid)).map(|j| j.id));
            match found {
                Some(id) => out.push(id),
                None => {
                    sh_eprintln!(sh, "bish: wait: `{}': not a pid or valid job spec", a);
                    return 127;
                }
            }
        }
        Some(out)
    };
    let eligible = |j: &crate::exec::Job| -> bool { !j.stopped && watched.as_ref().is_none_or(|w| w.contains(&j.id)) };
    loop {
        if !sh.jobs.borrow().jobs.iter().any(eligible) {
            return 127;
        }
        let mut table = sh.jobs.borrow_mut();
        let finished = table.jobs.iter_mut().position(|j| !j.stopped && watched.as_ref().is_none_or(|w| w.contains(&j.id)) && j.poll().is_some());
        if let Some(idx) = finished {
            let mut job = table.jobs.remove(idx);
            drop(table);
            return job.wait_draining();
        }
        drop(table);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// `wait` with no operands waits for every job and returns 0 (POSIX); with
// operands, for those, returning the last one's status. Stopped jobs are
// skipped: `Job::wait` (no WUNTRACED) would block on them forever.
pub(crate) fn run_wait(sh: &mut Shell, args: &[String]) -> i32 {
    if args.first().is_some_and(|a| a == "-n") {
        return wait_for_next(sh, &args[1..]);
    }
    if args.is_empty() {
        loop {
            let idx = sh.jobs.borrow().jobs.iter().position(|j| !j.stopped);
            let Some(idx) = idx else { break };
            let mut job = sh.jobs.borrow_mut().jobs.remove(idx);
            job.wait_draining();
        }
        return 0;
    }
    let mut status = 0;
    for a in args {
        if let Some(idx) = sh.resolve_job_spec(a) {
            if sh.jobs.borrow().jobs[idx].stopped {
                sh_eprintln!(sh, "bish: wait: job {} is stopped", sh.jobs.borrow().jobs[idx].id);
                status = 127;
                continue;
            }
            let mut job = sh.jobs.borrow_mut().jobs.remove(idx);
            status = job.wait_draining();
            continue;
        }
        match a.parse::<u32>() {
            Ok(pid) => {
                let idx = sh.jobs.borrow().jobs.iter().position(|j| j.pids.contains(&pid));
                match idx {
                    Some(idx) => {
                        let mut job = sh.jobs.borrow_mut().jobs.remove(idx);
                        status = job.wait_draining();
                    }
                    None => {
                        sh_eprintln!(sh, "bish: wait: pid {} is not a child of this shell", pid);
                        status = 127;
                    }
                }
            }
            Err(_) => {
                sh_eprintln!(sh, "bish: wait: {}: no such job", a);
                status = 127;
            }
        }
    }
    status
}

// `kill -l ARG`: a number becomes the name and a name the number. An exit
// status over 128 has 128 taken off, so `kill -l $?` names the signal
// (`kill -l 137` is `KILL`).
fn signal_name_or_number(arg: &str) -> Option<String> {
    if let Ok(n) = arg.parse::<i32>() {
        let n = if n > 128 { n - 128 } else { n };
        return crate::exec::all_signals().iter().find(|(_, num)| *num == n).map(|(name, _)| name.clone());
    }
    let bare = arg.strip_prefix("SIG").unwrap_or(arg).to_uppercase();
    crate::exec::all_signals().iter().find(|(name, _)| **name == bare).map(|(_, num)| num.to_string())
}

// `kill`'s reading of a signal spec, which differs from `trap`'s since
// sending and catching differ. It accepts 0, which sends nothing and only
// checks the process exists (`kill -0 $$`). And it accepts KILL and STOP,
// which `signal_number` leaves out so it can answer trap's "may I catch
// this?", but which can certainly be sent.
fn kill_signal_number(spec: &str) -> Option<i32> {
    if spec == "0" || spec == "SIG0" {
        return Some(0);
    }
    let bare = spec.strip_prefix("SIG").unwrap_or(spec);
    if let Some(&(_, n)) = crate::exec::UNCATCHABLE_SIGNALS.iter().find(|(name, _)| *name == bare) {
        return Some(n);
    }
    crate::exec::signal_number(spec)
}

// `kill [-SIGNAME|-N] pid|%job ...`, including process groups as
// negative targets.
pub(crate) fn run_kill(sh: &mut Shell, args: &[String]) -> i32 {
    let mut sig = 15; // SIGTERM
    let mut targets: Vec<&String> = Vec::new();
    let mut i = 0;
    // Whether a signal has been named. Only the first argument can be one, so
    // a negative number after it is a process group: `kill -TERM -$$`
    // signals the whole group.
    let mut signal_given = false;
    while i < args.len() {
        let a = &args[i];
        // Once a signal or any target is given, a leading `-` is a process
        // group, as in bash: `kill 111 -222` signals process 111 and group
        // 222.
        let still_options = !signal_given && targets.is_empty();
        if let Some(rest) = a.strip_prefix('-').filter(|_| still_options) {
            if rest == "l" {
                // `kill -l` alone lists the signals; with arguments it
                // translates each way.
                let rest_args: Vec<&String> = args.iter().skip_while(|a| a.as_str() != "-l").skip(1).collect();
                if rest_args.is_empty() {
                    for (name, num) in crate::exec::all_signals() {
                        sh_println!(sh, "{num}) SIG{name}");
                    }
                    return 0;
                }
                let mut status = 0;
                for arg in rest_args {
                    match signal_name_or_number(arg) {
                        Some(text) => sh_println!(sh, "{text}"),
                        None => {
                            sh_eprintln!(sh, "bish: kill: {arg}: invalid signal specification");
                            status = 1;
                        }
                    }
                }
                return status;
            }
            // `--` ends the options, so a process group can follow
            // (`kill -- -$pgid`).
            if rest == "-" {
                i += 1;
                while i < args.len() {
                    targets.push(&args[i]);
                    i += 1;
                }
                break;
            }
            // `-s SIGSPEC` and `-n SIGNUM` name the signal in the next
            // argument.
            if rest == "s" || rest == "n" {
                let Some(spec) = args.get(i + 1) else {
                    sh_eprintln!(sh, "bish: kill: -{rest}: option requires an argument");
                    return 1;
                };
                match kill_signal_number(spec) {
                    Some(n) => {
                        sig = n;
                        signal_given = true;
                        i += 2;
                        continue;
                    }
                    None => {
                        sh_eprintln!(sh, "bish: kill: {spec}: invalid signal specification");
                        return 1;
                    }
                }
            }
            if let Some(n) = kill_signal_number(rest) {
                sig = n;
                signal_given = true;
                i += 1;
                continue;
            }
            // A `-something` that isn't a signal isn't a target either: a
            // typo mustn't send a signal to the wrong process.
            if !rest.is_empty() {
                sh_eprintln!(sh, "bish: kill: {rest}: invalid signal specification");
                return 1;
            }
        }
        targets.push(a);
        i += 1;
    }
    let mut status = 0;
    for t in targets {
        if let Some(idx) = t.strip_prefix('%').and_then(|_| sh.resolve_job_spec(t)) {
            let pids = sh.jobs.borrow().jobs[idx].pids.clone();
            for pid in pids {
                send_signal(pid, sig);
            }
        } else if let Ok(pid) = t.parse::<i32>() {
            if !send_signal(pid as u32, sig) {
                sh_eprintln!(sh, "bish: kill: ({}) - No such process", pid);
                status = 1;
            }
        } else {
            sh_eprintln!(sh, "bish: kill: {}: arguments must be process or job IDs", t);
            status = 1;
        }
    }
    status
}
