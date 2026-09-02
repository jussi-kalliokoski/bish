// Job control: the builtins that name a job and do something to it.
//
// Free functions taking `&mut Shell` -- see `builtins/mod.rs`.

use crate::exec::{
    ExecResult, JobWaitOutcome, SIGCONT, Shell, getpgrp, send_signal, send_signal_to_pgrp, sh_eprintln, sh_println, signal_number, waitpid_untraced,
};
use crate::pty;

pub(crate) fn run_jobs(sh: &mut Shell, args: &[String]) -> i32 {
    // The flags themselves are accepted and ignored -- this listing has
    // one shape -- but a letter bash does not have is a typo, and
    // reporting it is the point of the check.
    if let Some(bad) = crate::exec::first_unknown_option(args, "lnprsx") {
        let usage = "jobs [-lnprs] [jobspec ...] or jobs -x command [args]";
        return crate::exec::bad_option_status(sh, "jobs", &bad, usage);
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
        // Checked before Job::poll -- that only ever wraps Child::
        // try_wait, which never observes a stop (no WUNTRACED), so a
        // job this shell has already recorded as Stopped would
        // otherwise just look Running to it forever (see Job::
        // stopped's own doc comment).
        if job.stopped {
            // No trailing " &": bash only shows that for a job
            // actually launched with `&`, and a job stopped via
            // Ctrl-Z from the foreground wasn't.
            sh_println!(sh, "[{}]{}  Stopped                 {}", job.id, mark, job.cmd_text);
            continue;
        }
        match job.poll() {
            Some(_) => {
                sh_println!(sh, "[{}]{}  Done                    {} &", job.id, mark, job.cmd_text);
                to_remove.push(i);
            }
            None => {
                sh_println!(sh, "[{}]{}  Running                 {} &", job.id, mark, job.cmd_text);
            }
        }
    }
    for i in to_remove.into_iter().rev() {
        table.jobs.remove(i);
    }
    0
}

// disown [-a|-r] [%job|pid...]: removes matching jobs from the job
// table without touching their children at all -- Rust's own
// `Child::drop` never kills a still-running process, only closes the
// handle, so simply removing the entry already gives disown's
// "stop tracking, let it keep running independently" effect. bish
// has no SIGHUP-on-exit for background jobs to begin with (a
// separate, pre-existing gap), so the other half of real disown's
// job -- surviving that signal -- doesn't apply here; this only
// affects `jobs`/`wait`/`fg`/`bg` no longer seeing the job. Bare
// `disown` (no flags, no specs) disowns just the current job,
// matching bash.
pub(crate) fn run_disown(sh: &mut Shell, args: &[String]) -> i32 {
    let mut all = false;
    let mut running_only = false;
    let mut specs: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "-a" => all = true,
            "-r" => running_only = true,
            _ => specs.push(a),
        }
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
    // resolve_job_spec takes its own immutable borrow of sh.jobs,
    // so every spec is resolved to an index before the single
    // borrow_mut below that actually removes them (highest index
    // first, so earlier removals don't shift later indices out from
    // under this same pass).
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
    idxs.sort_unstable();
    idxs.dedup();
    let mut table = sh.jobs.borrow_mut();
    for idx in idxs.into_iter().rev() {
        table.jobs.remove(idx);
    }
    status
}

// A job that *was* spawned pty-attached (Job::pty_master.is_some() --
// only true for a promoted, unredirected background job) bubbles
// ExecResult::Fg without blocking at all: see that variant's doc
// comment for why the actual poll loop has to happen through
// repl.rs's Shell::take_pending_fg + drive_fg_job instead of directly
// here.
//
// A job real job control isolated into its own process group (Job::
// pgid -- see its own doc comment) gets the real terminal-foregrounding
// dance: SIGCONT it if it was Stopped, tcsetpgrp the real terminal at
// it, wait watching for it to stop *again* (not just exit -- see
// waitpid_untraced) rather than Job::wait's plain blocking wait
// (which can never observe a stop), then reclaim the terminal for
// bish either way.
//
// Anything else (no pty, no pgid -- a multi-stage pipeline, or a job
// spawned before this shell ever ran under `set -m`) falls back to
// the original plain blocking wait: scripts don't distinguish that
// from real terminal foregrounding since they never interactively
// signal the job via the keyboard anyway.
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
                // Real bug, found interactively: if this job was
                // Stopped (Ctrl-Z while it was pty-driven -- see
                // FgJob::send_stop), it's sitting there SIGSTOP'd
                // right now. Bubbling it to repl.rs's drive_fg_job
                // without resuming it first means that loop just
                // forwards keystrokes (including further Ctrl-C/
                // Ctrl-Z) into a pty whose sole reader is frozen and
                // can't act on any of them -- not even run a signal
                // handler, since a stopped process runs *no* code at
                // all until continued -- which looked like the whole
                // shell hanging, unrecoverable short of the
                // undiscoverable Ctrl+Space detach. This job's own
                // pgid always equals its own (single) pid -- see
                // Job::pgid's doc comment on why pty-attached jobs
                // don't separately store one -- so send_signal_to_pgrp
                // on that pid reaches the same process SIGCONT would
                // via a real pgid.
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
                unsafe {
                    pty::tcsetpgrp(0, getpgrp()).ok();
                }
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

// Resumes a Stopped job (Job::stopped) in place, without reclaiming
// the real terminal for it -- SIGCONT to its process group is enough;
// it keeps running with whatever stdio it already had (inherited from
// the real terminal, same as any backgrounded command), it just isn't
// the terminal's foreground process group, so it won't receive
// further Ctrl-C/Ctrl-Z (and will itself stop again on SIGTTIN/
// SIGTTOU if it ever tries to read from the terminal -- ordinary
// kernel behavior, nothing this shell needs to implement).
// A job that's *already* running (no pgid at all, or pgid but not
// stopped) has nothing to resume -- matches real bash, confirmed:
// `bg` on an already-running job just reports "already in background"
// and returns 0.
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
                // Same fix as run_fg's own pty_master.is_some()
                // branch: a pty-attached job doesn't store a
                // separate pgid (see Job::pgid's doc comment), but
                // its own pid always equals its own pgid (setsid),
                // so send_signal_to_pgrp on that pid resumes it the
                // same way.
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

// `wait` with no operands waits for every active job and always
// returns 0 (POSIX-specified, confirmed against real bash); with
// operands, waits for just those and returns the *last* one's status.
// Stopped jobs (Job::stopped) are skipped rather than waited on --
// Job::wait is a plain Child::wait, which (no WUNTRACED) would just
// block forever on a job that's merely stopped, not exited.
// `wait -n [id ...]`: block until the *next* of the named jobs (or of
// every job, with none named) finishes, and report its status. 127 when
// there is nothing left to wait for -- which is also how a script's
// `while wait -n; do ...; done` loop knows to stop.
//
// A poll loop rather than a blocking `waitpid(-1)`: the children are
// owned by `std::process::Child`, and reaping one behind its back would
// leave every later `try_wait`/`wait` on it reporting a status that no
// longer exists. Job::poll is the same non-blocking primitive `jobs`
// already uses, so this stays on the one reaping path.
fn wait_for_next(sh: &mut Shell, ids: &[String]) -> i32 {
    // Resolved once: a job's index shifts as finished jobs are removed,
    // but its id doesn't.
    let watched: Option<Vec<u32>> = if ids.is_empty() {
        None
    } else {
        let mut out = Vec::new();
        for a in ids {
            if let Some(idx) = sh.resolve_job_spec(a) {
                out.push(sh.jobs.borrow().jobs[idx].id);
                continue;
            }
            // `wait -n` has its own wording for an unusable id, distinct
            // from plain `wait`'s "not a child of this shell" -- it
            // rejects the *spec* before it ever looks for a process.
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
            return job.wait();
        }
        drop(table);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

pub(crate) fn run_wait(sh: &mut Shell, args: &[String]) -> i32 {
    if args.first().is_some_and(|a| a == "-n") {
        return wait_for_next(sh, &args[1..]);
    }
    if args.is_empty() {
        loop {
            let idx = sh.jobs.borrow().jobs.iter().position(|j| !j.stopped);
            let Some(idx) = idx else { break };
            let mut job = sh.jobs.borrow_mut().jobs.remove(idx);
            job.wait();
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
            status = job.wait();
            continue;
        }
        match a.parse::<u32>() {
            Ok(pid) => {
                let idx = sh.jobs.borrow().jobs.iter().position(|j| j.pids.contains(&pid));
                match idx {
                    Some(idx) => {
                        let mut job = sh.jobs.borrow_mut().jobs.remove(idx);
                        status = job.wait();
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

// kill [-SIGNAME|-N] pid|%job ... Negative PIDs (process-group kill)
// aren't specially handled -- see the `jobs` field comment on why real
// process-group management is out of scope here.
// `kill -l ARG`: a number becomes the bare name, a name becomes the
// number. An exit status is accepted where a number is, so `kill -l
// $?` after a signalled command names the signal -- 128 is subtracted
// when it is over that, which is what makes `kill -l 137` say `KILL`.
fn signal_name_or_number(arg: &str) -> Option<String> {
    if let Ok(n) = arg.parse::<i32>() {
        let n = if n > 128 { n - 128 } else { n };
        return crate::exec::all_signals().iter().find(|(_, num)| *num == n).map(|(name, _)| (*name).to_string());
    }
    let bare = arg.strip_prefix("SIG").unwrap_or(arg).to_uppercase();
    crate::exec::all_signals().iter().find(|(name, _)| *name == bare).map(|(_, num)| num.to_string())
}

pub(crate) fn run_kill(sh: &mut Shell, args: &[String]) -> i32 {
    let mut sig = 15; // SIGTERM
    let mut targets: Vec<&String> = Vec::new();
    for a in args {
        if let Some(rest) = a.strip_prefix('-') {
            if rest == "l" {
                // `kill -l` on its own lists them; with arguments it
                // translates each one the other way -- a number to its
                // name, a name to its number -- which is how a script
                // reads `$?` back after a signal.
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
            if let Some(n) = signal_number(rest) {
                sig = n;
                continue;
            }
            // A `-something` that is not a signal is not a target
            // either -- treating `kill -99999 1` as "signal pid -99999
            // and pid 1 with SIGTERM" is how a typo becomes a signal
            // sent to the wrong process.
            if !rest.is_empty() {
                sh_eprintln!(sh, "bish: kill: {rest}: invalid signal specification");
                return 1;
            }
        }
        targets.push(a);
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
