use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use crate::arith;
use crate::builtins;
use crate::glob;
use crate::lexer::{Chunk, ReplaceAnchor, VarOp};
use crate::parser::{
    self, AndOr, AssignMode, Combinator, ListItem, Pipeline, Program, Redirect, Sep, SimpleCommand, Word,
};

#[derive(Debug, Clone, Copy)]
pub enum ExecResult {
    Status(i32),
    Break(u32),
    Continue(u32),
    Return(i32),
}

impl ExecResult {
    fn status(self) -> i32 {
        match self {
            ExecResult::Status(s) => s,
            ExecResult::Return(s) => s,
            ExecResult::Break(_) | ExecResult::Continue(_) => 0,
        }
    }

    fn is_signal(self) -> bool {
        matches!(self, ExecResult::Break(_) | ExecResult::Continue(_) | ExecResult::Return(_))
    }
}

pub struct Shell {
    pub last_status: i32,
    functions: HashMap<String, parser::Command>,
    // Stack of positional-parameter frames; last() is the current scope
    // ($0 is tracked separately since it's never shifted/reassigned by calls).
    arg_frames: Vec<Vec<String>>,
    // Stack of `local` overlays; empty unless we're inside a function call.
    // A name only lives here if `local` explicitly declared it -- plain
    // assignment still targets the global (process-env) variable unless it
    // matches an existing local of the same name, matching bash semantics.
    var_scopes: Vec<HashMap<String, String>>,
    script_name: String,
    // Indexed arrays (`arr=(...)`). A BTreeMap (not Vec) so arrays are
    // genuinely sparse like bash's: `arr[10]=x` doesn't materialize empty
    // strings for indices 0..9, and `${#arr[@]}` counts only what's
    // actually set. Kept as one flat global map (every read/write site
    // just indexes straight into it) rather than a var_scopes-style stack,
    // since only `local -a`/`-A` need scoping at all -- see
    // array_local_stack/assoc_local_stack below for how that's retrofitted
    // via save/restore instead of a parallel lookup chain.
    arrays: HashMap<String, std::collections::BTreeMap<usize, String>>,
    // Associative arrays (`declare -A name`). Kept in a separate map from
    // `arrays` since their keys are arbitrary strings, not indices -- a name
    // in `assoc_names` is looked up here instead of `arrays` everywhere an
    // array is read or written.
    assoc_arrays: HashMap<String, OrderedMap>,
    assoc_names: std::collections::HashSet<String>,
    // `alias name=value`: stored and queryable (alias/unalias both work as
    // a plain table), but NOT expanded when a command runs. Real alias
    // expansion happens at parse time on an already-known table, textually
    // substituting a command-position word before the rest of that line is
    // even tokenized -- fundamentally at odds with this shell parsing an
    // entire script upfront before executing anything (a script-mode
    // architecture, unlike bash's line-at-a-time interactive parsing). It's
    // also off by default in bash for non-interactive shells (needs
    // `shopt -s expand_aliases`), so a script that never touches that
    // option -- the overwhelming majority -- already sees this exact
    // behavior from real bash too. Storing without expanding keeps a
    // defensive `alias foo=... ` preamble from failing the script outright
    // under `set -e`, without risking a half-correct expansion that only
    // works for some control-flow shapes.
    // Vec, not a map -- bash's own `alias` listing (and real bash's own
    // internal table) is in definition order, not sorted, and this list is
    // never large enough for linear lookup to matter.
    aliases: Vec<(String, String)>,
    // One frame per active function call (pushed/popped alongside
    // var_scopes in call_function). `local -a`/`-A name` snapshots the
    // array's pre-local value here (None if it didn't exist) before
    // resetting it to empty, so returning from the function restores
    // whatever the caller had -- a save/restore shadow rather than a real
    // nested scope chain, since `arrays`/`assoc_arrays` themselves stay
    // flat (see the comment on `arrays` above).
    array_local_stack: Vec<Vec<(String, Option<std::collections::BTreeMap<usize, String>>)>>,
    assoc_local_stack: Vec<Vec<(String, Option<OrderedMap>)>>,
    // `declare -n`/`local -n ref=target`: ref's own stored value is the
    // *name* of the target variable, not user data -- lookup_var/assign_var/
    // var_is_set all redirect through resolve_nameref for any name in this
    // set before doing anything else, so reading/writing `ref` transparently
    // reads/writes `target` instead. Scalars only; array-element namerefs
    // (`declare -n ref=arr[0]`) aren't supported, a scoped gap.
    nameref_names: std::collections::HashSet<String>,
    // Scoping for `local -n` (mirrors array_local_stack): each frame
    // records, for every name it nameref'd, whether that name was already
    // a nameref beforehand -- so call_function can undo just the
    // membership change on return without disturbing a same-named nameref
    // the *caller* had declared. `declare -n` at top level (empty
    // var_scopes) has no frame to push onto and just leaks globally,
    // matching bash's own top-level `declare -n` behavior.
    nameref_local_stack: Vec<Vec<(String, bool)>>,
    // pushd/popd/dirs: does NOT include the current directory itself (bash
    // convention -- `dirs` prints cwd first, then this stack). `+N`/`-N`
    // rotation forms aren't implemented, a scoped gap.
    dir_stack: Vec<String>,
    // shopt -s/-u NAME: only a handful of names are meaningfully tracked
    // (most of bish's behavior isn't actually gated by any of these -- e.g.
    // extglob is unconditionally on, see glob.rs), but recognizing the
    // builtin at all means `shopt -s extglob`/`shopt -s nullglob` in a
    // script no longer fails as an unknown command, which would otherwise
    // abort the whole script under `set -e`.
    shopt_options: std::collections::HashSet<String>,
    // `readonly NAME`. Checked by assign_var, the single write path plain
    // assignment/local/export/declare/arithmetic-assignment/read/getopts
    // all funnel through, so marking a name here blocks writes everywhere
    // at once.
    readonly_names: std::collections::HashSet<String>,
    // `declare -i`/`local -i`: assignments to these names are evaluated as
    // arithmetic expressions instead of stored as literal text (checked in
    // assign_var, the single write path).
    integer_names: std::collections::HashSet<String>,
    // `declare -u`/`-l`: assignments to these names are case-folded
    // (checked in assign_var alongside integer_names).
    upper_names: std::collections::HashSet<String>,
    lower_names: std::collections::HashSet<String>,
    // `declare -x`/`export -x NAME` on a name that's currently a `local`:
    // globals are already unconditionally visible to children (assign_var
    // writes them straight to the process env), so this only matters for a
    // local -- assign_var additionally mirrors the value into the process
    // env for any name in this set, so child processes can see it despite
    // it living in var_scopes rather than env.
    exported_names: std::collections::HashSet<String>,
    // `>(cmd)` substitutions queued by the command currently being built,
    // to run (reading the temp file back) once it finishes; see
    // run_proc_sub_out/drain_proc_subs.
    proc_sub_out_pending: Vec<(String, String)>,
    // Every proc-sub temp file created for the command currently being
    // built, deleted once it finishes (drain_proc_subs).
    proc_sub_cleanup: Vec<String>,
    // $RANDOM: xorshift64* state, reseeded from the current time at
    // startup (no external RNG crate). Advanced on every read.
    rng_state: u64,
    // $SECONDS: wall-clock time the shell started, so `$SECONDS` reads as
    // elapsed-since-start by default. `SECONDS=n` (bash lets you reset the
    // counter) is handled by assign_var recording an offset instead of
    // trying to rewind this.
    shell_start: std::time::Instant,
    seconds_offset: i64,
    // $!: PID of the most recently backgrounded command (the last stage,
    // for a backgrounded pipeline -- matches bash). Mirrors `jobs.last()`'s
    // PID; kept as its own field since it needs to keep reporting the same
    // PID even after that job is reaped out of the table by `wait`/`jobs`.
    last_bg_pid: Option<u32>,
    // Background jobs (`cmd &`), tracked well enough for `jobs`/`fg`/`bg`/
    // `wait`/`kill %N` to work against real child processes. What's
    // explicitly NOT here: real terminal job control -- no process-group
    // creation, no `tcsetpgrp` foreground reassignment, no SIGTSTP/Ctrl-Z
    // suspend-to-stopped. That's all specifically about a human at a
    // keyboard hitting Ctrl-Z and expecting the terminal to hand control
    // back and forth between the shell and a job; it doesn't affect
    // whether a *script* runs correctly (scripts don't self-suspend), and
    // getting process-group/terminal reassignment wrong risks leaving a
    // real terminal in a broken state -- a bad trade for a feature that
    // isn't script-compatibility-relevant in the first place. `fg`/`bg`
    // here just mean "wait for" / "leave running", not the full terminal
    // dance.
    jobs: Vec<Job>,
    next_job_id: u32,
    // `trap CMD SIGNAL`. EXIT is handled separately (exit_trap below) since
    // it isn't a real OS signal. Everything else here is a genuine
    // sigaction-installed handler (see install_trap_handler) -- the numeric
    // signal is the key since that's what the handler's async-signal-safe
    // bookkeeping (PENDING_SIGNALS) uses; SIGNAL_NAMES maps both directions
    // for `trap`'s own name-based syntax.
    traps: std::collections::HashMap<i32, TrapAction>,
    // `trap CMD EXIT` handler. Not a real signal (there's no SIGEXIT), so
    // it's run directly wherever the shell is about to terminate rather
    // than through the sigaction/PENDING_SIGNALS machinery.
    exit_trap: Option<String>,
    // `coproc`'s pipe halves that the *shell* keeps (the coprocess's own
    // ends are handed to the child and closed here after spawn). Kept
    // alive here, keyed by raw fd number, for as long as the coprocess
    // might still be interacted with -- otherwise the PipeReader/Writer
    // would drop and close the fd the moment run_coproc returns. Numbered-
    // fd redirects on *other* commands (`cmd <&"${NAME[0]}"`) just need
    // the fd number to still be open in this process at dup2 time, which
    // this satisfies without those redirects needing to know anything
    // about coproc specifically; `read -u FD` is the one thing that reads
    // through this table directly (see run_single's "read" arm).
    coproc_fds: std::collections::HashMap<i32, KeptFd>,
    // `set -e`/`-u`/`-x`/`-o pipefail`/`-f`.
    opt_errexit: bool,
    opt_nounset: bool,
    opt_xtrace: bool,
    opt_pipefail: bool,
    opt_noglob: bool,
    // `set -m`/`-o monitor`: real bash's `fg`/`bg` refuse to run at all
    // ("no job control") unless this is on -- confirmed against real bash,
    // which defaults it off for a non-interactive script (only real
    // interactive shells, or a script that explicitly opts in, get it).
    // `jobs`/`wait`/`kill` aren't gated by this; only fg/bg are.
    opt_monitor: bool,
    // Suppresses errexit while >0 -- set around if/while/until conditions
    // and negated (`!`) pipelines, the cases POSIX explicitly exempts from
    // triggering -e (a failing condition is meant to be checked, not
    // treated as a fatal error).
    suppress_errexit: u32,
    // Set by run_single around a command's own word-expansion so mid-
    // expansion diagnostics (nounset's "unbound variable", etc) go to that
    // command's own `2>` target instead of unconditionally to the shell's
    // real stderr, matching bash routing its own error messages through
    // the command's redirects too.
    current_stderr_target: Option<String>,
    // Set while running command-mode (`:`) input: only builtins may run
    // directly (see the fallthrough gate in run_single, right before
    // function lookup) -- `command NAME` is the explicit escape hatch for
    // externals, matching its existing "force external" semantics.
    pub restrict_to_builtins: bool,
    // Whether `window`-family promotion into full-screen mode has already
    // happened (see run_window/promote_if_needed) -- only ever flips once.
    promoted: bool,
    // Mirrors the OS process's real cwd, kept in sync by run_cd/run_pushd/
    // run_popd (which still delegate the actual directory change to
    // std::env::set_current_dir -- this field doesn't yet let a session
    // diverge from the process's real cwd, that only matters once
    // multiple sessions can exist). Exists now so callers read cwd from
    // here rather than re-querying the OS each time, and so every spawn
    // site already passes an explicit cwd -- both wiring this milestone's
    // later multi-session work will need without changing yet again.
    pub cwd: std::path::PathBuf,
}

impl Shell {
    pub fn new() -> Self {
        Shell {
            last_status: 0,
            functions: HashMap::new(),
            arg_frames: vec![Vec::new()],
            var_scopes: Vec::new(),
            script_name: "bish".to_string(),
            arrays: HashMap::new(),
            assoc_arrays: HashMap::new(),
            assoc_names: std::collections::HashSet::new(),
            aliases: Vec::new(),
            array_local_stack: Vec::new(),
            assoc_local_stack: Vec::new(),
            nameref_names: std::collections::HashSet::new(),
            nameref_local_stack: Vec::new(),
            dir_stack: Vec::new(),
            shopt_options: std::collections::HashSet::new(),
            readonly_names: std::collections::HashSet::new(),
            integer_names: std::collections::HashSet::new(),
            upper_names: std::collections::HashSet::new(),
            lower_names: std::collections::HashSet::new(),
            exported_names: std::collections::HashSet::new(),
            proc_sub_out_pending: Vec::new(),
            proc_sub_cleanup: Vec::new(),
            rng_state: {
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x2545F4914F6CDD1D)
                    ^ (std::process::id() as u64).wrapping_mul(0x9E3779B97F4A7C15);
                if seed == 0 { 0x2545F4914F6CDD1D } else { seed }
            },
            shell_start: std::time::Instant::now(),
            seconds_offset: 0,
            last_bg_pid: None,
            jobs: Vec::new(),
            next_job_id: 1,
            traps: std::collections::HashMap::new(),
            exit_trap: None,
            coproc_fds: std::collections::HashMap::new(),
            opt_errexit: false,
            opt_nounset: false,
            opt_xtrace: false,
            opt_pipefail: false,
            opt_noglob: false,
            opt_monitor: false,
            suppress_errexit: 0,
            current_stderr_target: None,
            restrict_to_builtins: false,
            promoted: false,
            cwd: std::env::current_dir().unwrap_or_default(),
        }
    }

    // True once `window`-family promotion has switched the terminal into
    // full-screen mode -- repl.rs checks this at exit time to know whether
    // it needs to restore the normal screen buffer before quitting.
    pub fn is_promoted(&self) -> bool {
        self.promoted
    }

    pub fn run_exit_trap(&mut self) {
        if let Some(cmd) = self.exit_trap.take() {
            self.run_source_here(&cmd, "trap");
        }
    }

    // Resolves a job spec (%N, %%/%+  current, %-  previous, %name  prefix
    // match on the job's command text) to an index into self.jobs. Bare
    // job numbers without the `%` are also accepted, matching bash's own
    // leniency in `fg`/`bg`/`wait`.
    // Registers a freshly spawned background job (one child for a plain
    // backgrounded command, several for a backgrounded pipeline) in the
    // job table, and updates $! to the last child's PID (bash's own
    // convention for a backgrounded pipeline).
    fn push_job(&mut self, children: Vec<std::process::Child>, cmd_text: String) {
        let pids: Vec<u32> = children.iter().map(|c| c.id()).collect();
        self.last_bg_pid = pids.last().copied();
        let id = self.next_job_id;
        self.next_job_id += 1;
        self.jobs.push(Job { id, pids, children, cmd_text });
    }

    fn resolve_job_spec(&self, spec: &str) -> Option<usize> {
        let rest = spec.strip_prefix('%').unwrap_or(spec);
        if rest.is_empty() || rest == "%" || rest == "+" {
            return if self.jobs.is_empty() { None } else { Some(self.jobs.len() - 1) };
        }
        if rest == "-" {
            return if self.jobs.len() >= 2 { Some(self.jobs.len() - 2) } else { None };
        }
        if let Ok(n) = rest.parse::<u32>() {
            return self.jobs.iter().position(|j| j.id == n);
        }
        self.jobs.iter().position(|j| j.cmd_text.starts_with(rest))
    }

    fn run_jobs(&mut self, _args: &[String]) -> i32 {
        let last_idx = self.jobs.len().checked_sub(1);
        let prev_idx = self.jobs.len().checked_sub(2);
        let mut to_remove = Vec::new();
        for (i, job) in self.jobs.iter_mut().enumerate() {
            let mark = if Some(i) == last_idx {
                "+"
            } else if Some(i) == prev_idx {
                "-"
            } else {
                " "
            };
            match job.poll() {
                Some(_) => {
                    println!("[{}]{}  Done                    {} &", job.id, mark, job.cmd_text);
                    to_remove.push(i);
                }
                None => {
                    println!("[{}]{}  Running                 {} &", job.id, mark, job.cmd_text);
                }
            }
        }
        for i in to_remove.into_iter().rev() {
            self.jobs.remove(i);
        }
        0
    }

    // `fg` doesn't reassociate the terminal's controlling process group
    // (see the comment on the `jobs` field) -- it just blocks until the
    // job finishes, in this process, and reports its exit status. Scripts
    // don't distinguish that from real terminal foregrounding since they
    // never interactively signal the job via the keyboard anyway.
    fn run_fg(&mut self, args: &[String]) -> i32 {
        if !self.opt_monitor {
            eprintln!("bish: fg: no job control");
            return 1;
        }
        let idx = match args.first() {
            Some(spec) => self.resolve_job_spec(spec),
            None => self.jobs.len().checked_sub(1),
        };
        match idx {
            Some(i) => {
                let cmd_text = self.jobs[i].cmd_text.clone();
                println!("{}", cmd_text);
                let mut job = self.jobs.remove(i);
                job.wait()
            }
            None => {
                eprintln!("bish: fg: no current job");
                1
            }
        }
    }

    // Every job this shell tracks is, by construction, already running --
    // there's no SIGTSTP/Ctrl-Z support (see the `jobs` field comment), so
    // nothing ever reaches a genuine Stopped state for `bg` to resume.
    // Confirmed against real bash: `bg` on an already-running job doesn't
    // resume anything, it just reports "already in background" and
    // returns 0 -- exactly the one case this implementation can ever
    // reach, so that's what it always does.
    fn run_bg(&mut self, args: &[String]) -> i32 {
        if !self.opt_monitor {
            eprintln!("bish: bg: no job control");
            return 1;
        }
        let idx = match args.first() {
            Some(spec) => self.resolve_job_spec(spec),
            None => self.jobs.len().checked_sub(1),
        };
        match idx {
            Some(i) => {
                eprintln!("bish: bg: job {} already in background", self.jobs[i].id);
                0
            }
            None => {
                eprintln!("bish: bg: no current job");
                1
            }
        }
    }

    // `wait` with no operands waits for every active job and always
    // returns 0 (POSIX-specified, confirmed against real bash); with
    // operands, waits for just those and returns the *last* one's status.
    fn run_wait(&mut self, args: &[String]) -> i32 {
        if args.is_empty() {
            while !self.jobs.is_empty() {
                let mut job = self.jobs.remove(0);
                job.wait();
            }
            return 0;
        }
        let mut status = 0;
        for a in args {
            if let Some(idx) = self.resolve_job_spec(a) {
                let mut job = self.jobs.remove(idx);
                status = job.wait();
                continue;
            }
            match a.parse::<u32>() {
                Ok(pid) => match self.jobs.iter().position(|j| j.pids.contains(&pid)) {
                    Some(idx) => {
                        let mut job = self.jobs.remove(idx);
                        status = job.wait();
                    }
                    None => {
                        eprintln!("bish: wait: pid {} is not a child of this shell", pid);
                        status = 127;
                    }
                },
                Err(_) => {
                    eprintln!("bish: wait: {}: no such job", a);
                    status = 127;
                }
            }
        }
        status
    }

    // kill [-SIGNAME|-N] pid|%job ... Negative PIDs (process-group kill)
    // aren't specially handled -- see the `jobs` field comment on why real
    // process-group management is out of scope here.
    fn run_kill(&mut self, args: &[String]) -> i32 {
        let mut sig = 15; // SIGTERM
        let mut targets: Vec<&String> = Vec::new();
        for a in args {
            if let Some(rest) = a.strip_prefix('-') {
                if rest == "l" {
                    for (name, num) in SIGNAL_NAMES {
                        println!("{}) SIG{}", num, name);
                    }
                    return 0;
                }
                if let Some(n) = signal_number(rest) {
                    sig = n;
                    continue;
                }
            }
            targets.push(a);
        }
        let mut status = 0;
        for t in targets {
            if let Some(idx) = t.strip_prefix('%').and_then(|_| self.resolve_job_spec(t)) {
                let pids = self.jobs[idx].pids.clone();
                for pid in pids {
                    send_signal(pid, sig);
                }
            } else if let Ok(pid) = t.parse::<i32>() {
                if !send_signal(pid as u32, sig) {
                    eprintln!("bish: kill: ({}) - No such process", pid);
                    status = 1;
                }
            } else {
                eprintln!("bish: kill: {}: arguments must be process or job IDs", t);
                status = 1;
            }
        }
        status
    }

    // getopts optstring name [args...]. Options requiring an argument are
    // marked with a trailing ':' in optstring (e.g. "ab:c"); a leading ':'
    // switches to "silent" error mode (custom handling via OPTARG/'?'/':'
    // instead of a printed message), matching bash.
    fn run_getopts(&mut self, args: &[String]) -> ExecResult {
        let optstring = args.first().cloned().unwrap_or_default();
        let varname = match args.get(1) {
            Some(v) => v.clone(),
            None => {
                eprintln!("bish: getopts: usage: getopts optstring name [args]");
                return ExecResult::Status(2);
            }
        };
        let positional: Vec<String> =
            if args.len() > 2 { args[2..].to_vec() } else { self.arg_frames.last().cloned().unwrap_or_default() };

        let optind: usize = self.lookup_var("OPTIND").trim().parse().unwrap_or(1);
        let idx = optind.saturating_sub(1);

        if idx >= positional.len() {
            return ExecResult::Status(1);
        }
        let cur = positional[idx].clone();
        if !cur.starts_with('-') || cur == "-" {
            return ExecResult::Status(1);
        }
        if cur == "--" {
            self.assign_var("OPTIND", (optind + 1).to_string());
            return ExecResult::Status(1);
        }

        let opt_char = cur.chars().nth(1).unwrap_or('?');
        let silent = optstring.starts_with(':');
        let spec = optstring.trim_start_matches(':');

        let Some(pos) = spec.find(opt_char) else {
            if silent {
                self.assign_var(&varname, "?".to_string());
                self.assign_var("OPTARG", opt_char.to_string());
            } else {
                eprintln!("bish: getopts: illegal option -- '{}'", opt_char);
                self.assign_var(&varname, "?".to_string());
            }
            self.assign_var("OPTIND", (optind + 1).to_string());
            return ExecResult::Status(0);
        };

        let needs_arg = spec.as_bytes().get(pos + 1) == Some(&b':');
        if needs_arg {
            let rest: String = cur.chars().skip(2).collect();
            if !rest.is_empty() {
                self.assign_var("OPTARG", rest);
                self.assign_var("OPTIND", (optind + 1).to_string());
            } else if idx + 1 < positional.len() {
                self.assign_var("OPTARG", positional[idx + 1].clone());
                self.assign_var("OPTIND", (optind + 2).to_string());
            } else {
                if silent {
                    self.assign_var(&varname, ":".to_string());
                    self.assign_var("OPTARG", opt_char.to_string());
                } else {
                    eprintln!("bish: getopts: option requires an argument -- '{}'", opt_char);
                    self.assign_var(&varname, "?".to_string());
                }
                self.assign_var("OPTIND", (optind + 1).to_string());
                return ExecResult::Status(0);
            }
        } else {
            self.assign_var("OPTIND", (optind + 1).to_string());
        }
        self.assign_var(&varname, opt_char.to_string());
        ExecResult::Status(0)
    }

    // unset [-f|-v] NAME... Also accepts `arr[i]` to remove one element
    // without touching the rest of the array. `stderr_target` mirrors real
    // bash routing this error through the command's own `2>` (confirmed via
    // a clean bash probe) -- unlike nounset/plain-assignment errors, which
    // always go to real stderr since they happen before any redirect setup.
    fn run_unset(&mut self, args: &[String], stderr_target: &Option<String>) -> i32 {
        let mut only_funcs = false;
        let mut only_vars = false;
        let mut names: Vec<&String> = Vec::new();
        for a in args {
            match a.as_str() {
                "-f" => only_funcs = true,
                "-v" => only_vars = true,
                _ => names.push(a),
            }
        }
        for n in names {
            if only_funcs {
                self.functions.remove(n.as_str());
                continue;
            }
            if let Some(bracket) = n.find('[') {
                if let Some(idx_expr) = n.strip_suffix(']').map(|s| &s[bracket + 1..]) {
                    let arr_name = n[..bracket].to_string();
                    if self.assoc_names.contains(&arr_name) {
                        let key = self.expand_index_as_string(idx_expr);
                        if let Some(map) = self.assoc_arrays.get_mut(&arr_name) {
                            map.remove(&key);
                        }
                    } else if let Ok(i) = arith::eval(idx_expr, self) {
                        if let Some(idx) = self.resolve_array_index(&arr_name, i) {
                            if let Some(map) = self.arrays.get_mut(&arr_name) {
                                map.remove(&idx);
                            }
                        }
                    }
                    continue;
                }
            }
            if self.readonly_names.contains(n.as_str()) {
                write_diagnostic(stderr_target, &format!("bish: unset: {}: cannot unset: readonly variable", n));
                continue;
            }
            self.arrays.remove(n.as_str());
            self.assoc_arrays.remove(n.as_str());
            self.assoc_names.remove(n.as_str());
            let mut removed_local = false;
            for scope in self.var_scopes.iter_mut().rev() {
                if scope.remove(n.as_str()).is_some() {
                    removed_local = true;
                    break;
                }
            }
            if !removed_local {
                unsafe {
                    std::env::remove_var(n);
                }
            }
            if !only_vars {
                self.functions.remove(n.as_str());
            }
        }
        0
    }

    // declare/typeset [-A|-a|-i|-r] [NAME|NAME=value]... `-x` isn't tracked
    // separately since every variable already lives in the process env
    // here; other real bash flags (-u/-l/-n/-p/...) are accepted but not
    // enforced.
    fn run_declare(&mut self, args: &[String]) -> i32 {
        let mut array_mode: Option<bool> = None; // Some(true)=-A, Some(false)=-a
        let mut readonly_flag = false;
        let mut integer_flag = false;
        let mut nameref_flag = false;
        let mut upper_flag = false;
        let mut lower_flag = false;
        let mut export_flag = false;
        for a in args {
            match a.as_str() {
                "-A" => {
                    array_mode = Some(true);
                    continue;
                }
                "-a" => {
                    array_mode = Some(false);
                    continue;
                }
                "-r" => {
                    readonly_flag = true;
                    continue;
                }
                "-i" => {
                    integer_flag = true;
                    continue;
                }
                "-n" => {
                    nameref_flag = true;
                    continue;
                }
                "-u" => {
                    upper_flag = true;
                    continue;
                }
                "-l" => {
                    lower_flag = true;
                    continue;
                }
                "-x" => {
                    export_flag = true;
                    continue;
                }
                _ => {}
            }
            if a.starts_with('-') {
                continue;
            }
            let (name, val) = match a.find('=') {
                Some(eq) => (a[..eq].to_string(), Some(a[eq + 1..].to_string())),
                None => (a.clone(), None),
            };
            if integer_flag {
                self.integer_names.insert(name.clone());
            }
            if upper_flag {
                self.upper_names.insert(name.clone());
            }
            if lower_flag {
                self.lower_names.insert(name.clone());
            }
            if export_flag {
                self.exported_names.insert(name.clone());
            }
            if nameref_flag {
                self.nameref_names.insert(name.clone());
                if let Some(v) = val {
                    self.raw_var_write(&name, v);
                }
                if readonly_flag {
                    self.readonly_names.insert(name);
                }
                continue;
            }
            match array_mode {
                Some(true) => {
                    self.assoc_names.insert(name.clone());
                    self.assoc_arrays.entry(name.clone()).or_default();
                }
                Some(false) => {
                    self.arrays.entry(name.clone()).or_default();
                }
                None => {
                    if let Some(v) = val {
                        self.assign_var(&name, v);
                    } else if export_flag {
                        // Bare `declare -x NAME`/`export NAME` on an
                        // already-set variable (commonly a local: `local
                        // Z=inner; export Z`) -- re-assign its current
                        // value through assign_var so exported_names'
                        // mirror-to-env logic fires immediately, instead
                        // of only on the variable's *next* write. The
                        // empty-fallback branch below wouldn't reach this
                        // case since it only fires for a name with no
                        // value at all yet.
                        let cur = self.lookup_var(&name);
                        self.assign_var(&name, cur);
                    } else if self.lookup_var(&name).is_empty() && std::env::var(&name).is_err() {
                        self.assign_var(&name, String::new());
                    }
                }
            }
            if readonly_flag {
                self.readonly_names.insert(name);
            }
        }
        0
    }

    // readonly NAME[=value]... Marks each name so assign_var refuses future
    // writes. The initializing assignment (if any) happens before the name
    // is added to readonly_names, so it isn't rejected by its own call.
    fn run_readonly(&mut self, args: &[String]) -> i32 {
        for a in args {
            if a.starts_with('-') {
                continue;
            }
            let (name, val) = match a.find('=') {
                Some(eq) => (a[..eq].to_string(), Some(a[eq + 1..].to_string())),
                None => (a.clone(), None),
            };
            if let Some(v) = val {
                self.assign_var(&name, v);
            }
            self.readonly_names.insert(name);
        }
        0
    }

    // shopt -s/-u NAME... [-q NAME]. Most tracked names have no actual
    // effect on bish's behavior (see the comment on shopt_options), but
    // recognizing the builtin at all keeps `shopt -s extglob`-style
    // defensive script preambles from failing as an unknown command.
    // extglob is special-cased as always reporting "on", since bish's
    // extglob support is unconditional rather than gated by this flag.
    fn run_shopt(&mut self, args: &[String]) -> i32 {
        let mut mode: Option<bool> = None; // Some(true)=-s, Some(false)=-u
        let mut quiet = false;
        let mut names: Vec<&str> = Vec::new();
        for a in args {
            match a.as_str() {
                "-s" => mode = Some(true),
                "-u" => mode = Some(false),
                "-q" => quiet = true,
                _ if a.starts_with('-') => {}
                other => names.push(other),
            }
        }
        let is_on = |shell: &Shell, n: &str| n == "extglob" || shell.shopt_options.contains(n);
        match mode {
            Some(on) => {
                for n in &names {
                    if on {
                        self.shopt_options.insert(n.to_string());
                    } else {
                        self.shopt_options.remove(*n);
                    }
                }
                0
            }
            None if quiet => {
                if names.iter().all(|n| is_on(self, n)) {
                    0
                } else {
                    1
                }
            }
            None => {
                for n in &names {
                    println!("{:<15}\t{}", n, if is_on(self, n) { "on" } else { "off" });
                }
                0
            }
        }
    }

    // `window`/`w`/`win` -- the window-manager builtin. This is an M1 stub:
    // there is no real multi-window session/ancestry model yet (that's the
    // in-process session-cloning primitive and the Window/Frame stack
    // planned for later), so next/previous/new/close just report the
    // single-window state honestly rather than pretending to manage
    // windows that don't exist yet. What IS real here: the promotion
    // trigger and the restricted-execution/command-mode plumbing around
    // it, which every later milestone builds on unchanged.
    fn run_window(&mut self, args: &[String]) -> i32 {
        self.promote_if_needed();
        match args.first().map(String::as_str) {
            Some("next") | Some("previous") | Some("prev") | Some("p") => {
                println!("window: only one window open");
                0
            }
            Some("new") => {
                println!("window: new not yet supported");
                0
            }
            Some("close") | Some("c") => {
                eprintln!("bish: window close: cannot close the last window -- exit the shell instead");
                1
            }
            Some(other) => {
                eprintln!("bish: window: unknown subcommand: {}", other);
                2
            }
            None => {
                eprintln!("bish: window: missing subcommand (next/previous/new/close)");
                2
            }
        }
    }

    // Draws the M1 promotion stub the first time any window-family command
    // runs: switches to the terminal's alternate screen buffer (mode 1049)
    // so whatever was on screen before promotion stays untouched in the
    // real terminal's own native scrollback, then draws a plain one-tab
    // bar. No terminal-size query here (that needs a TIOCGWINSZ ioctl,
    // planned for the real compositor) so this can't pin the bar to the
    // actual bottom row yet -- an acknowledged, temporary limitation of
    // the stub, not the final design. Superseded wholesale once the real
    // compositor lands.
    fn promote_if_needed(&mut self) {
        if self.promoted {
            return;
        }
        self.promoted = true;
        print!("\x1b[?1049h\x1b[2J\x1b[H");
        println!("bish window manager -- M1 stub, single window only\r");
        println!("\x1b[7m [1] {} \x1b[0m\r", self.cwd.display());
        println!("\r");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }

    // Moved here from a builtins.rs free function once cd needed to update
    // self.cwd (a Shell-owned field) rather than just the OS process's
    // cwd -- matches every other builtin that needs shell state (see
    // run_pushd/run_declare/etc, all Shell methods for the same reason).
    // Still delegates the actual directory change to
    // std::env::set_current_dir so path resolution/symlink/error behavior
    // is unchanged from before; self.cwd is read back from the OS
    // afterward rather than computed locally, so it stays exactly in sync
    // with what the OS considers the real cwd.
    fn run_cd(&mut self, args: &[String]) -> i32 {
        let old = self.cwd.to_string_lossy().into_owned();
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
                self.cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(&target));
                unsafe {
                    std::env::set_var("OLDPWD", old);
                    std::env::set_var("PWD", self.cwd.to_string_lossy().into_owned());
                }
                0
            }
            Err(e) => {
                eprintln!("cd: {}: {}", target, e);
                1
            }
        }
    }

    fn run_pushd(&mut self, args: &[String]) -> i32 {
        let target = match args.iter().find(|a| !a.starts_with('-')) {
            Some(d) => d.clone(),
            None => match self.dir_stack.first() {
                Some(d) => d.clone(),
                None => {
                    eprintln!("bish: pushd: no other directory");
                    return 1;
                }
            },
        };
        if !args.iter().any(|a| !a.starts_with('-')) {
            // Bare `pushd`: rotate -- cd into the current top-of-stack,
            // then push the old cwd back onto the front, net-swapping them.
            self.dir_stack.remove(0);
        }
        let old_cwd = self.cwd.to_string_lossy().into_owned();
        if self.run_cd(&[target]) != 0 {
            return 1;
        }
        self.dir_stack.insert(0, old_cwd);
        self.print_dirs(false);
        0
    }

    fn run_popd(&mut self, _args: &[String]) -> i32 {
        let target = match self.dir_stack.first() {
            Some(d) => d.clone(),
            None => {
                eprintln!("bish: popd: directory stack empty");
                return 1;
            }
        };
        if self.run_cd(&[target]) != 0 {
            return 1;
        }
        self.dir_stack.remove(0);
        self.print_dirs(false);
        0
    }

    fn run_dirs(&mut self, args: &[String]) -> i32 {
        if args.iter().any(|a| a == "-c") {
            self.dir_stack.clear();
            return 0;
        }
        self.print_dirs(args.iter().any(|a| a == "-v"));
        0
    }

    fn collapse_home(path: &str) -> String {
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                if path == home {
                    return "~".to_string();
                }
                if let Some(rest) = path.strip_prefix(&format!("{}/", home)) {
                    return format!("~/{}", rest);
                }
            }
        }
        path.to_string()
    }

    fn print_dirs(&self, vertical: bool) {
        let mut all = vec![self.cwd.to_string_lossy().into_owned()];
        all.extend(self.dir_stack.iter().cloned());
        if vertical {
            for (i, d) in all.iter().enumerate() {
                println!("{:2}  {}", i, Self::collapse_home(d));
            }
        } else {
            println!("{}", all.iter().map(|d| Self::collapse_home(d)).collect::<Vec<_>>().join(" "));
        }
    }

    // set [-euxo pipefail] [--] [args...]. Combined single-char flags
    // (-eu, -ex, -eux) work; `-o name` must be its own token (not combined
    // into a cluster with other short flags) -- matches real bash, which
    // also rejects e.g. `-euo pipefail` (it consumes `-o` with no argument
    // of its own, then tries to parse "pipefail"'s remaining letters as
    // further short flags and errors on the first invalid one).
    fn run_set(&mut self, args: &[String]) -> i32 {
        let mut idx = 0;
        let mut saw_dashdash = false;
        while idx < args.len() {
            let a = &args[idx];
            if a == "--" {
                saw_dashdash = true;
                idx += 1;
                break;
            }
            if let Some(rest) = a.strip_prefix('-').filter(|r| !r.is_empty()) {
                if rest == "o" {
                    if let Some(optname) = args.get(idx + 1) {
                        self.apply_shell_option(optname, true);
                        idx += 2;
                        continue;
                    }
                }
                for c in rest.chars() {
                    self.apply_shell_flag(c, true);
                }
                idx += 1;
                continue;
            }
            if let Some(rest) = a.strip_prefix('+').filter(|r| !r.is_empty()) {
                if rest == "o" {
                    if let Some(optname) = args.get(idx + 1) {
                        self.apply_shell_option(optname, false);
                        idx += 2;
                        continue;
                    }
                }
                for c in rest.chars() {
                    self.apply_shell_flag(c, false);
                }
                idx += 1;
                continue;
            }
            break;
        }
        if saw_dashdash || idx < args.len() {
            let new_args = args[idx..].to_vec();
            if let Some(frame) = self.arg_frames.last_mut() {
                *frame = new_args;
            }
        }
        0
    }

    fn apply_shell_flag(&mut self, c: char, on: bool) {
        match c {
            'e' => self.opt_errexit = on,
            'u' => self.opt_nounset = on,
            'x' => self.opt_xtrace = on,
            'f' => self.opt_noglob = on,
            'm' => self.opt_monitor = on,
            _ => {}
        }
    }

    fn apply_shell_option(&mut self, name: &str, on: bool) {
        match name {
            "pipefail" => self.opt_pipefail = on,
            "errexit" => self.opt_errexit = on,
            "nounset" => self.opt_nounset = on,
            "xtrace" => self.opt_xtrace = on,
            "noglob" => self.opt_noglob = on,
            "monitor" => self.opt_monitor = on,
            _ => {}
        }
    }

    pub fn set_script_args(&mut self, name: String, args: Vec<String>) {
        self.script_name = name;
        self.arg_frames = vec![args];
    }

    // Runs any trapped signal's handler code for signals that arrived
    // since the last check (see the comment on PENDING_SIGNALS for why
    // this poll-at-checkpoints approach exists instead of running trap
    // code directly from the signal handler). Called once per top-level
    // statement in run_program -- frequent enough to feel responsive for
    // real scripts, without needing signal-safety anywhere outside the
    // one-line handler itself.
    fn check_pending_signals(&mut self) {
        let pending = PENDING_SIGNALS.swap(0, std::sync::atomic::Ordering::SeqCst);
        if pending == 0 {
            return;
        }
        for sig in 0..32 {
            if pending & (1 << sig) == 0 {
                continue;
            }
            if let Some(TrapAction::Run(code)) = self.traps.get(&sig).cloned() {
                self.run_source_here(&code, "trap");
            }
        }
    }

    pub fn run_program(&mut self, prog: &Program) -> ExecResult {
        let mut result = ExecResult::Status(self.last_status);
        for item in prog {
            self.check_pending_signals();
            let background = matches!(item.sep, Sep::Background);
            result = self.run_and_or(&item.and_or, background);
            self.last_status = result.status();
            if result.is_signal() {
                return result;
            }
            // `set -e`: abort on any failing top-level statement, except
            // while suppressed (if/while/until conditions, negated
            // pipelines -- POSIX exempts those explicitly). Checking once
            // per ListItem here (the *overall* and-or result) rather than
            // per-pipeline also naturally exempts non-last commands in a
            // &&/|| chain, since only the chain's final status reaches here.
            if self.opt_errexit && self.suppress_errexit == 0 && result.status() != 0 {
                self.run_exit_trap();
                std::process::exit(result.status());
            }
        }
        result
    }

    fn run_and_or(&mut self, and_or: &AndOr, background: bool) -> ExecResult {
        let mut result = self.run_pipeline(&and_or.first, background);
        self.last_status = result.status();
        if result.is_signal() {
            return result;
        }
        let mut status = result.status();
        for (comb, pipeline) in &and_or.rest {
            let should_run = match comb {
                Combinator::And => status == 0,
                Combinator::Or => status != 0,
            };
            if should_run {
                result = self.run_pipeline(pipeline, background);
                self.last_status = result.status();
                if result.is_signal() {
                    return result;
                }
                status = result.status();
            }
        }
        ExecResult::Status(status)
    }

    fn run_pipeline(&mut self, pipeline: &Pipeline, background: bool) -> ExecResult {
        if pipeline.negate {
            // POSIX exempts a `!`-negated pipeline's own failure from -e
            // (that's usually the whole point of negating it).
            self.suppress_errexit += 1;
            let result = self.run_pipeline_inner(pipeline, background);
            self.suppress_errexit -= 1;
            return match result {
                ExecResult::Status(s) => ExecResult::Status(if s == 0 { 1 } else { 0 }),
                signal => signal,
            };
        }
        self.run_pipeline_inner(pipeline, background)
    }

    fn run_pipeline_inner(&mut self, pipeline: &Pipeline, background: bool) -> ExecResult {
        if pipeline.commands.len() == 1 {
            return self.run_command(&pipeline.commands[0], background);
        }
        ExecResult::Status(self.run_multi(&pipeline.commands, background))
    }

    fn run_command(&mut self, cmd: &parser::Command, background: bool) -> ExecResult {
        let redirects: &[Redirect] = command_own_redirects(cmd);
        if !redirects.is_empty() {
            return self.run_compound_redirected(cmd, redirects, background);
        }
        match cmd {
            parser::Command::Simple(sc) => self.run_single(sc, background),
            parser::Command::If { branches, else_branch, .. } => self.run_if(branches, else_branch),
            parser::Command::While { cond, body, until, .. } => self.run_while(cond, body, *until),
            parser::Command::For { var, words, body, .. } => {
                let var = var.clone();
                let words = words.clone();
                self.run_for(&var, words.as_deref(), body)
            }
            parser::Command::CFor { init, cond, step, body, .. } => {
                let init = init.clone();
                let cond = cond.clone();
                let step = step.clone();
                self.run_cfor(&init, &cond, &step, body)
            }
            parser::Command::Select { var, words, body, .. } => {
                let var = var.clone();
                let words = words.clone();
                self.run_select(&var, words.as_deref(), body)
            }
            parser::Command::Case { word, arms, .. } => self.run_case(word, arms),
            parser::Command::Group(prog, _redirects) => self.run_program(prog),
            parser::Command::FuncDef { name, body } => {
                self.functions.insert(name.clone(), (**body).clone());
                ExecResult::Status(0)
            }
            parser::Command::Subshell(raw, _redirects) => ExecResult::Status(self.run_subshell(raw, background)),
            parser::Command::Arith(raw, _redirects) => match arith::eval(raw, self) {
                Ok(v) => ExecResult::Status(if v != 0 { 0 } else { 1 }),
                Err(e) => {
                    eprintln!("bish: (({})): {}", raw, e);
                    ExecResult::Status(1)
                }
            },
            parser::Command::Test(atoms, _redirects) => self.run_test(atoms),
            parser::Command::Coproc { name, body } => {
                let name = name.clone();
                ExecResult::Status(self.run_coproc(name, body))
            }
        }
    }

    // `read` is a builtin, so it can't go through the normal Stdio-based
    // redirect machinery (that's built for handing stdio to a *child*
    // process, not reading in-process). Special-cased here since `read x
    // <<< "..."` is too common a pattern to leave broken.
    fn read_input_source(&mut self, cmd: &SimpleCommand) -> Box<dyn std::io::BufRead> {
        for r in cmd.redirects.iter().rev() {
            match r {
                Redirect::HereString(w) => {
                    let mut content = self.expand_word(w);
                    content.push('\n');
                    return Box::new(std::io::Cursor::new(content.into_bytes()));
                }
                Redirect::HereDoc(w) => {
                    let content = self.expand_word(w);
                    return Box::new(std::io::Cursor::new(content.into_bytes()));
                }
                Redirect::In(w) => {
                    let p = self.expand_word(w);
                    return match std::fs::File::open(&p) {
                        Ok(f) => Box::new(std::io::BufReader::new(f)),
                        Err(e) => {
                            eprintln!("bish: {}: {}", p, e);
                            Box::new(std::io::Cursor::new(Vec::new()))
                        }
                    };
                }
                _ => continue,
            }
        }
        // NOT `BufReader::new(stdin())` -- that wraps stdin in a fresh,
        // throwaway read-ahead buffer on every `read` call. A single
        // read_line() can pull far more than one line into that buffer in
        // one syscall; whatever it read past the first line is then lost
        // when the wrapper is dropped at the end of this call, so a `while
        // read` loop would silently only ever see its first line. Stdin's
        // own lock reuses the shared, persistent buffer behind
        // std::io::stdin() instead, so nothing already-read is discarded
        // between calls.
        Box::new(std::io::stdin().lock())
    }

    // Shared by `eval` and `source`/`.` -- both run source text in the
    // CURRENT shell (unlike command substitution/subshells, which self-exec
    // a child process), so `eval`/sourced scripts can set variables,
    // functions, or cwd in the calling shell.
    fn run_source_here(&mut self, src: &str, label: &str) -> ExecResult {
        match crate::lexer::Lexer::new(src).tokenize() {
            Ok(toks) => match crate::parser::Parser::new(toks).parse_program() {
                Ok(prog) => self.run_program(&prog),
                Err(e) => {
                    eprintln!("bish: {}: syntax error: {}", label, e);
                    ExecResult::Status(2)
                }
            },
            Err(e) => {
                eprintln!("bish: {}: syntax error: {}", label, e);
                ExecResult::Status(2)
            }
        }
    }

    // Functions and `local` variables live only in this process's memory, so
    // a self-exec'd child (see run_subshell/run_command_substitution below)
    // starts with no knowledge of them -- unlike a real fork, which
    // duplicates the whole process (this is exactly why e.g. `$(fib
    // $((n-1)))` works in bash even when `n` is a local var: the forked
    // child inherits it for free). Re-declaring visible locals and every
    // currently-known function at the top of the child's script closes that
    // gap without needing unsafe fork(2) or a separate IPC channel.
    fn functions_preamble(&self) -> String {
        let mut s = String::new();
        let mut flattened: HashMap<&str, &str> = HashMap::new();
        for scope in &self.var_scopes {
            for (k, v) in scope {
                flattened.insert(k.as_str(), v.as_str());
            }
        }
        for (k, v) in &flattened {
            s.push_str(k);
            s.push('=');
            s.push_str(&crate::serialize::quote_literal(v));
            s.push('\n');
        }
        for (name, items) in &self.arrays {
            s.push_str(name);
            s.push_str("=(");
            for item in items.values() {
                s.push_str(&crate::serialize::quote_literal(item));
                s.push(' ');
            }
            s.push_str(")\n");
        }
        for (name, map) in &self.assoc_arrays {
            s.push_str("declare -A ");
            s.push_str(name);
            s.push('\n');
            for (k, v) in map.iter() {
                s.push_str(name);
                s.push('[');
                s.push_str(&crate::serialize::quote_literal(k));
                s.push_str("]=");
                s.push_str(&crate::serialize::quote_literal(v));
                s.push('\n');
            }
        }
        if let Some(frame) = self.arg_frames.last() {
            s.push_str("set --");
            for a in frame {
                s.push(' ');
                s.push_str(&crate::serialize::quote_literal(a));
            }
            s.push('\n');
        }
        for name in &self.readonly_names {
            s.push_str("readonly ");
            s.push_str(name);
            s.push('\n');
        }
        for (name, body) in &self.functions {
            let def = parser::Command::FuncDef { name: name.clone(), body: Box::new(body.clone()) };
            s.push_str(&crate::serialize::serialize_program(&[ListItem {
                and_or: AndOr {
                    first: Pipeline { commands: vec![def], negate: false },
                    rest: Vec::new(),
                },
                sep: Sep::Seq,
            }]));
        }
        s
    }

    // (...) subshells self-exec the bish binary on the raw captured source
    // (plus the function preamble), inheriting env but not sharing process
    // state -- real bash subshells are forked children too, so mutations
    // inside (cd, variables) must not leak back into the parent. Spawning a
    // real child process gets that isolation for free instead of a separate
    // snapshot/restore mechanism.
    fn run_subshell(&mut self, raw: &str, background: bool) -> i32 {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bish: subshell: {}", e);
                return 1;
            }
        };
        let script = self.functions_preamble() + raw;
        if background {
            match Command::new(exe).arg("-c").arg(script).current_dir(&self.cwd).spawn() {
                Ok(child) => {
                    self.push_job(vec![child], format!("({})", raw));
                    0
                }
                Err(e) => {
                    eprintln!("bish: subshell: {}", e);
                    1
                }
            }
        } else {
            match Command::new(exe).arg("-c").arg(script).current_dir(&self.cwd).status() {
                Ok(status) => exit_code_from_status(status),
                Err(e) => {
                    eprintln!("bish: subshell: {}", e);
                    1
                }
            }
        }
    }

    // `coproc [NAME] command` -- see the Command::Coproc doc comment for
    // the (scoped) grammar this accepts. Runs `body` as a background
    // process wired to two pipes: NAME[0] is an fd the shell can read the
    // coprocess's stdout from, NAME[1] an fd it can write to the
    // coprocess's stdin. NAME_PID gets the coprocess's PID. The coprocess
    // is also registered as an ordinary background job (visible in
    // `jobs`/`wait`, same as real bash).
    fn run_coproc(&mut self, name: Option<String>, body: &parser::Command) -> i32 {
        use std::os::fd::AsRawFd;
        let name = name.unwrap_or_else(|| "COPROC".to_string());
        let (out_r, out_w) = match std::io::pipe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bish: coproc: {}", e);
                return 1;
            }
        };
        let (in_r, in_w) = match std::io::pipe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bish: coproc: {}", e);
                return 1;
            }
        };
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bish: coproc: {}", e);
                return 1;
            }
        };
        let script = self.functions_preamble() + &crate::serialize::serialize_command(body);
        let mut command = Command::new(exe);
        command.arg("-c").arg(script);
        command.current_dir(&self.cwd);
        command.stdin(Stdio::from(in_r));
        command.stdout(Stdio::from(out_w));
        match command.spawn() {
            Ok(child) => {
                let out_r_fd = out_r.as_raw_fd();
                let in_w_fd = in_w.as_raw_fd();
                self.coproc_fds.insert(out_r_fd, KeptFd::Read(std::io::BufReader::new(out_r)));
                self.coproc_fds.insert(in_w_fd, KeptFd::Write(in_w));
                let mut map = std::collections::BTreeMap::new();
                map.insert(0, out_r_fd.to_string());
                map.insert(1, in_w_fd.to_string());
                self.arrays.insert(name.clone(), map);
                self.assign_var(&format!("{}_PID", name), child.id().to_string());
                let cmd_text = crate::serialize::serialize_command(body);
                self.push_job(vec![child], cmd_text);
                0
            }
            Err(e) => {
                eprintln!("bish: coproc: {}", e);
                1
            }
        }
    }

    // Compound commands (if/while/for/case/group) with a trailing redirect
    // (`{ ...; } > file`, `done < file`) self-exec too, same as pipeline
    // stages -- avoids needing unsafe fd-dup2 to redirect this process's own
    // stdio for a nested block. serialize_command drops the redirects when
    // reconstructing the child's script (they're applied here, at spawn
    // time, instead), so the child doesn't re-trigger this same path.
    // Trade-off: unlike real bash, state changes inside (cd, variables)
    // won't propagate back to the parent in this specific case.
    fn run_compound_redirected(&mut self, cmd: &parser::Command, redirects: &[Redirect], background: bool) -> ExecResult {
        let redirs = match self.resolve_redirect_list(redirects) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("bish: {}", e);
                return ExecResult::Status(1);
            }
        };
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bish: {}", e);
                return ExecResult::Status(1);
            }
        };
        let script = self.functions_preamble() + &crate::serialize::serialize_command(cmd);
        let mut command = Command::new(exe);
        command.arg("-c").arg(script);
        command.current_dir(&self.cwd);
        command.stdin(redirs.stdin.unwrap_or_else(Stdio::inherit));
        command.stdout(redirs.stdout.unwrap_or_else(Stdio::inherit));
        command.stderr(redirs.stderr.unwrap_or_else(Stdio::inherit));
        apply_fd_redirects(&mut command, redirs.dup_stderr_to_stdout, redirs.extra_fds);
        match command.spawn() {
            Ok(child) => {
                if background {
                    let cmd_text = crate::serialize::serialize_command(cmd);
                    self.push_job(vec![child], cmd_text);
                    ExecResult::Status(0)
                } else {
                    let mut child = child;
                    match child.wait() {
                        Ok(status) => ExecResult::Status(exit_code_from_status(status)),
                        Err(e) => {
                            eprintln!("bish: {}", e);
                            ExecResult::Status(1)
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("bish: {}", e);
                ExecResult::Status(127)
            }
        }
    }

    fn run_command_substitution(&self, raw: &str) -> String {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => return String::new(),
        };
        let script = self.functions_preamble() + raw;
        match Command::new(exe).arg("-c").arg(script).current_dir(&self.cwd).output() {
            Ok(out) => {
                let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
                while s.ends_with('\n') {
                    s.pop();
                }
                s
            }
            Err(_) => String::new(),
        }
    }

    // `<(cmd)`: runs cmd to completion now, capturing its stdout into a
    // temp file, and substitutes that file's path. Real bash streams this
    // concurrently through a FIFO; see the ProcSubIn/ProcSubOut doc comment
    // in lexer.rs for why this shell uses a temp file instead. The path is
    // queued for cleanup (self.proc_sub_cleanup) once the enclosing command
    // has finished reading it.
    fn run_proc_sub_in(&mut self, raw: &str) -> String {
        let path = proc_sub_temp_path();
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bish: process substitution: {}", e);
                return String::new();
            }
        };
        let file = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("bish: process substitution: {}", e);
                return String::new();
            }
        };
        let script = self.functions_preamble() + raw;
        match Command::new(exe).arg("-c").arg(script).current_dir(&self.cwd).stdout(Stdio::from(file)).status() {
            Ok(_) => {}
            Err(e) => eprintln!("bish: process substitution: {}", e),
        }
        let path_str = path.to_string_lossy().into_owned();
        self.proc_sub_cleanup.push(path_str.clone());
        path_str
    }

    // `>(cmd)`: substitutes a temp file path immediately (so the enclosing
    // command can write to it like any other file), and queues cmd to run
    // reading that file back once the enclosing command finishes -- correct
    // data flow, but sequential rather than concurrent (see lexer.rs).
    fn run_proc_sub_out(&mut self, raw: &str) -> String {
        let path = proc_sub_temp_path();
        if let Err(e) = std::fs::File::create(&path) {
            eprintln!("bish: process substitution: {}", e);
            return String::new();
        }
        let path_str = path.to_string_lossy().into_owned();
        self.proc_sub_out_pending.push((path_str.clone(), raw.to_string()));
        path_str
    }

    // Runs any `>(cmd)` substitutions queued by the command that just
    // finished, then deletes every proc-sub temp file used this round.
    fn drain_proc_subs(&mut self) {
        if !self.proc_sub_out_pending.is_empty() {
            let pending = std::mem::take(&mut self.proc_sub_out_pending);
            for (path, raw) in pending {
                if let Ok(exe) = std::env::current_exe() {
                    let script = self.functions_preamble() + &raw;
                    if let Ok(file) = std::fs::File::open(&path) {
                        let _ = Command::new(exe).arg("-c").arg(script).current_dir(&self.cwd).stdin(Stdio::from(file)).status();
                    }
                }
                self.proc_sub_cleanup.push(path);
            }
        }
        for path in self.proc_sub_cleanup.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn call_function(&mut self, body: &parser::Command, call_args: Vec<String>) -> ExecResult {
        self.arg_frames.push(call_args);
        self.var_scopes.push(HashMap::new());
        self.array_local_stack.push(Vec::new());
        self.assoc_local_stack.push(Vec::new());
        self.nameref_local_stack.push(Vec::new());
        let result = self.run_command(body, false);
        self.var_scopes.pop();
        if let Some(frame) = self.nameref_local_stack.pop() {
            for (name, was_nameref) in frame.into_iter().rev() {
                if !was_nameref {
                    self.nameref_names.remove(&name);
                }
            }
        }
        if let Some(frame) = self.array_local_stack.pop() {
            for (name, prev) in frame.into_iter().rev() {
                match prev {
                    Some(v) => {
                        self.arrays.insert(name, v);
                    }
                    None => {
                        self.arrays.remove(&name);
                    }
                }
            }
        }
        if let Some(frame) = self.assoc_local_stack.pop() {
            for (name, prev) in frame.into_iter().rev() {
                match prev {
                    Some(v) => {
                        self.assoc_arrays.insert(name, v);
                    }
                    None => {
                        self.assoc_arrays.remove(&name);
                        self.assoc_names.remove(&name);
                    }
                }
            }
        }
        self.arg_frames.pop();
        match result {
            ExecResult::Return(code) => ExecResult::Status(code),
            other => other,
        }
    }

    fn run_if(&mut self, branches: &[(Program, Program)], else_branch: &Option<Program>) -> ExecResult {
        for (cond, body) in branches {
            self.suppress_errexit += 1;
            let cond_result = self.run_program(cond);
            self.suppress_errexit -= 1;
            if cond_result.is_signal() {
                return cond_result;
            }
            if cond_result.status() == 0 {
                return self.run_program(body);
            }
        }
        if let Some(else_body) = else_branch {
            return self.run_program(else_body);
        }
        ExecResult::Status(0)
    }

    fn run_while(&mut self, cond: &Program, body: &Program, until: bool) -> ExecResult {
        let mut ran_body = false;
        // Tracked separately from self.last_status: evaluating `cond` runs
        // a real command through run_program, which unconditionally
        // overwrites self.last_status as a side effect (needed so `$?`
        // reads right *during* that command) -- including on the final,
        // loop-ending check. Without this, a loop's reported exit status
        // would reflect that failing condition check instead of the body's
        // last status, which is what bash actually reports.
        let mut last_body_status = 0;
        loop {
            self.suppress_errexit += 1;
            let cond_result = self.run_program(cond);
            self.suppress_errexit -= 1;
            if cond_result.is_signal() {
                return cond_result;
            }
            let keep_going = if until { cond_result.status() != 0 } else { cond_result.status() == 0 };
            if !keep_going {
                break;
            }
            ran_body = true;
            match self.run_program(body) {
                ExecResult::Break(n) => {
                    if n > 1 {
                        return ExecResult::Break(n - 1);
                    }
                    break;
                }
                ExecResult::Continue(n) => {
                    if n > 1 {
                        return ExecResult::Continue(n - 1);
                    }
                    continue;
                }
                ExecResult::Status(s) => {
                    self.last_status = s;
                    last_body_status = s;
                }
                ret @ ExecResult::Return(_) => return ret,
            }
        }
        if ran_body {
            self.last_status = last_body_status;
            ExecResult::Status(last_body_status)
        } else {
            ExecResult::Status(0)
        }
    }

    fn run_for(&mut self, var: &str, words: Option<&[Word]>, body: &Program) -> ExecResult {
        let values = match words {
            Some(words) => self.expand_words(words),
            None => self.arg_frames.last().cloned().unwrap_or_default(),
        };
        let mut ran_body = false;
        for val in values {
            ran_body = true;
            self.assign_var(var, val);
            match self.run_program(body) {
                ExecResult::Break(n) => {
                    if n > 1 {
                        return ExecResult::Break(n - 1);
                    }
                    break;
                }
                ExecResult::Continue(n) => {
                    if n > 1 {
                        return ExecResult::Continue(n - 1);
                    }
                    continue;
                }
                ExecResult::Status(s) => self.last_status = s,
                ret @ ExecResult::Return(_) => return ret,
            }
        }
        if ran_body {
            ExecResult::Status(self.last_status)
        } else {
            ExecResult::Status(0)
        }
    }

    // `select var [in words]; do body; done`. Displays a numbered menu to
    // stderr (never stdout, matching real bash) and reads a choice from
    // stdin, looping until `break` or EOF. Real bash's exact behavior,
    // confirmed empirically:
    //  - the menu is (re)printed before the very first prompt, and again
    //    after any BLANK line of input, but not otherwise
    //  - a blank input line reprompts without running the body at all
    //  - a non-blank but out-of-range/non-numeric choice still runs the
    //    body, with `var` set to empty (REPLY still gets the raw text)
    //  - EOF on stdin ends the loop entirely (like an implicit break),
    //    without running the body, and the loop's status is 1
    fn run_select(&mut self, var: &str, words: Option<&[Word]>, body: &Program) -> ExecResult {
        let items: Vec<String> = match words {
            Some(words) => self.expand_words(words),
            None => self.arg_frames.last().cloned().unwrap_or_default(),
        };
        let print_menu = |items: &[String]| {
            for (i, item) in items.iter().enumerate() {
                eprintln!("{}) {}", i + 1, item);
            }
        };
        print_menu(&items);
        loop {
            let ps3 = {
                let v = self.lookup_var("PS3");
                if v.is_empty() { "#? ".to_string() } else { v }
            };
            eprint!("{}", ps3);
            let _ = std::io::Write::flush(&mut std::io::stderr());
            let mut line = String::new();
            match std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line) {
                // Confirmed against real bash: on EOF, `select` prints a
                // bare newline to *stdout* (not stderr, where the menu/
                // prompt otherwise live) before giving up -- reproducible
                // even non-interactively, not just an interactive-terminal
                // courtesy newline.
                Ok(0) => {
                    println!();
                    return ExecResult::Status(1);
                }
                Ok(_) => {}
                Err(_) => return ExecResult::Status(1),
            }
            let line = line.trim_end_matches(['\n', '\r']).to_string();
            self.assign_var("REPLY", line.clone());
            if line.trim().is_empty() {
                print_menu(&items);
                continue;
            }
            let choice = line.trim().parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|i| items.get(i));
            self.assign_var(var, choice.cloned().unwrap_or_default());
            match self.run_program(body) {
                ExecResult::Break(n) => {
                    if n > 1 {
                        return ExecResult::Break(n - 1);
                    }
                    return ExecResult::Status(self.last_status);
                }
                ExecResult::Continue(n) => {
                    if n > 1 {
                        return ExecResult::Continue(n - 1);
                    }
                    continue;
                }
                ExecResult::Status(s) => self.last_status = s,
                ret @ ExecResult::Return(_) => return ret,
            }
        }
    }

    // `for ((init; cond; step)); do body; done`. Unlike run_for/run_while,
    // `continue` must still run `step` before re-checking `cond` -- so the
    // Continue arm falls through to the step evaluation below instead of
    // looping straight back like a plain `while`'s `continue` does.
    fn run_cfor(&mut self, init: &str, cond: &str, step: &str, body: &Program) -> ExecResult {
        if !init.is_empty() {
            if let Err(e) = arith::eval(init, self) {
                eprintln!("bish: (({})): {}", init, e);
                return ExecResult::Status(1);
            }
        }
        let mut ran_body = false;
        loop {
            let keep_going = if cond.is_empty() {
                true
            } else {
                match arith::eval(cond, self) {
                    Ok(v) => v != 0,
                    Err(e) => {
                        eprintln!("bish: (({})): {}", cond, e);
                        return ExecResult::Status(1);
                    }
                }
            };
            if !keep_going {
                break;
            }
            ran_body = true;
            match self.run_program(body) {
                ExecResult::Break(n) => {
                    if n > 1 {
                        return ExecResult::Break(n - 1);
                    }
                    break;
                }
                ExecResult::Continue(n) => {
                    if n > 1 {
                        return ExecResult::Continue(n - 1);
                    }
                }
                ExecResult::Status(s) => self.last_status = s,
                ret @ ExecResult::Return(_) => return ret,
            }
            if !step.is_empty() {
                if let Err(e) = arith::eval(step, self) {
                    eprintln!("bish: (({})): {}", step, e);
                    return ExecResult::Status(1);
                }
            }
        }
        if ran_body {
            ExecResult::Status(self.last_status)
        } else {
            ExecResult::Status(0)
        }
    }

    fn run_case(&mut self, word: &Word, arms: &[(Vec<Word>, Program, parser::CaseTerm)]) -> ExecResult {
        let val = self.expand_word(word);
        let mut i = 0;
        // Set by `;&`: run the next arm's body unconditionally, skipping
        // its pattern test entirely (unlike `;;&`, which just resumes
        // normal pattern testing at the next arm without forcing a run).
        let mut force_run = false;
        let mut last_body_status = 0;
        while i < arms.len() {
            let (patterns, body, term) = &arms[i];
            let should_run = force_run
                || patterns.iter().any(|p| {
                    let pat = self.expand_word(p);
                    glob::matches(&pat, &val)
                });
            if should_run {
                match self.run_program(body) {
                    ExecResult::Status(s) => {
                        self.last_status = s;
                        last_body_status = s;
                    }
                    other => return other,
                }
                match term {
                    parser::CaseTerm::Stop => return ExecResult::Status(last_body_status),
                    parser::CaseTerm::FallThrough => force_run = true,
                    parser::CaseTerm::Continue => force_run = false,
                }
            } else {
                force_run = false;
            }
            i += 1;
        }
        ExecResult::Status(last_body_status)
    }

    // `[[ expr ]]`. Real recursive-descent precedence over the flat
    // TestAtom stream the parser built: NOT binds tightest, then simple
    // tests (unary/binary), then AND, then OR (loosest) -- matching bash.
    fn run_test(&mut self, atoms: &[parser::TestAtom]) -> ExecResult {
        let mut pos = 0;
        match self.eval_test_or(atoms, &mut pos) {
            Ok(b) => ExecResult::Status(if b { 0 } else { 1 }),
            Err(e) => {
                eprintln!("bish: [[: {}", e);
                ExecResult::Status(2)
            }
        }
    }

    fn eval_test_or(&mut self, atoms: &[parser::TestAtom], pos: &mut usize) -> Result<bool, String> {
        let mut result = self.eval_test_and(atoms, pos)?;
        while matches!(atoms.get(*pos), Some(parser::TestAtom::Or)) {
            *pos += 1;
            let rhs = self.eval_test_and(atoms, pos)?;
            result = result || rhs;
        }
        Ok(result)
    }

    fn eval_test_and(&mut self, atoms: &[parser::TestAtom], pos: &mut usize) -> Result<bool, String> {
        let mut result = self.eval_test_unary(atoms, pos)?;
        while matches!(atoms.get(*pos), Some(parser::TestAtom::And)) {
            *pos += 1;
            let rhs = self.eval_test_unary(atoms, pos)?;
            result = result && rhs;
        }
        Ok(result)
    }

    fn eval_test_unary(&mut self, atoms: &[parser::TestAtom], pos: &mut usize) -> Result<bool, String> {
        if matches!(atoms.get(*pos), Some(parser::TestAtom::Not)) {
            *pos += 1;
            return Ok(!self.eval_test_unary(atoms, pos)?);
        }
        self.eval_test_primary(atoms, pos)
    }

    fn eval_test_primary(&mut self, atoms: &[parser::TestAtom], pos: &mut usize) -> Result<bool, String> {
        match atoms.get(*pos) {
            Some(parser::TestAtom::Group(inner)) => {
                *pos += 1;
                let mut ipos = 0;
                self.eval_test_or(inner, &mut ipos)
            }
            Some(parser::TestAtom::Word(_)) => {
                let mut word_atoms: Vec<&Word> = Vec::new();
                while let Some(parser::TestAtom::Word(w)) = atoms.get(*pos) {
                    word_atoms.push(w);
                    *pos += 1;
                }
                Ok(self.eval_simple_test(&word_atoms))
            }
            other => Err(format!("syntax error near {:?}", other)),
        }
    }

    fn eval_simple_test(&mut self, words: &[&Word]) -> bool {
        match words {
            [] => false,
            [s] => !self.expand_word(s).is_empty(),
            [op, a] => {
                let op = self.expand_word(op);
                let a = self.expand_word(a);
                builtins::unary(&op, &a)
            }
            [a, op, b] => {
                let op = self.expand_word(op);
                let a = self.expand_word(a);
                if op == "=~" {
                    let pattern = self.expand_regex_operand(b);
                    match crate::regex::match_captures(&a, &pattern) {
                        Some(groups) => {
                            let map: std::collections::BTreeMap<usize, String> =
                                groups.into_iter().enumerate().collect();
                            self.arrays.insert("BASH_REMATCH".to_string(), map);
                            true
                        }
                        None => {
                            self.arrays.remove("BASH_REMATCH");
                            false
                        }
                    }
                } else {
                    let b = self.expand_word(b);
                    builtins::binary(&a, &op, &b, true)
                }
            }
            _ => !words.is_empty(),
        }
    }

    // The RHS of `[[ str =~ pattern ]]`. Quoting (or backslash-escaping)
    // any part of `pattern` in the source forces that part to match
    // literally instead of as regex syntax -- real bash's documented
    // behavior for `=~`. Mirrors expand_word's chunk loop, but escapes
    // each chunk's own text for the regex engine when that chunk is
    // individually quoted (Chunk::LiteralStr, or any other chunk's own
    // `quoted` flag), leaving unquoted chunks as raw regex syntax -- so
    // e.g. `^[0-9]+\.[0-9]+$` keeps `^`/`[0-9]`/`+`/`$` as regex metachars
    // while the backslash-escaped `.` matches only a literal dot.
    fn expand_regex_operand(&mut self, w: &Word) -> String {
        let mut out = String::new();
        for c in &w.chunks {
            match c {
                Chunk::Str(t) => out.push_str(t),
                Chunk::LiteralStr(t) => out.push_str(&crate::regex::escape(t)),
                Chunk::Var { name, quoted } => {
                    let name = name.clone();
                    self.check_nounset(&name);
                    let v = self.lookup_var(&name);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::Sub { raw, quoted } => {
                    let v = self.run_command_substitution(raw);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::Arith { raw, quoted } => match arith::eval(raw, self) {
                    Ok(v) => {
                        let v = v.to_string();
                        out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                    }
                    Err(e) => eprintln!("bish: (({})): {}", raw, e),
                },
                Chunk::VarExpand { name, op, quoted } => {
                    let name = name.clone();
                    let op = op.clone();
                    let v = self.eval_var_op(&name, &op);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::ArrayVar { name, index, quoted } => {
                    let name = name.clone();
                    let index = index.clone();
                    let v = self.array_element(&name, &index);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::ArrayLength { name, index } => {
                    let name = name.clone();
                    let index = index.clone();
                    out.push_str(&self.array_length(&name, &index).to_string());
                }
                Chunk::ArrayVarExpand { name, index, op, quoted } => {
                    let name = name.clone();
                    let index = index.clone();
                    let op = op.clone();
                    let v = self.eval_array_var_op(&name, &index, &op);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::Indirect { name, quoted } => {
                    let target = self.lookup_var(name);
                    let v = self.lookup_var(&target);
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::ArrayKeys { name, quoted } => {
                    let name = name.clone();
                    let v = self.array_keys(&name).join(" ");
                    out.push_str(&if *quoted { crate::regex::escape(&v) } else { v });
                }
                Chunk::ProcSubIn { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_in(&raw);
                    out.push_str(&crate::regex::escape(&v));
                }
                Chunk::ProcSubOut { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_out(&raw);
                    out.push_str(&crate::regex::escape(&v));
                }
            }
        }
        out
    }

    fn run_single(&mut self, cmd: &SimpleCommand, background: bool) -> ExecResult {
        if cmd.words.is_empty() {
            for (name, mode, val) in &cmd.assigns {
                let v = self.expand_word(val);
                match mode {
                    AssignMode::Set => self.assign_var(name, v),
                    AssignMode::Append => {
                        let mut cur = self.lookup_var(name);
                        cur.push_str(&v);
                        self.assign_var(name, cur);
                    }
                }
            }
            for (name, mode, items) in &cmd.array_assigns {
                let values: Vec<String> = items.iter().map(|w| self.expand_word(w)).collect();
                match mode {
                    AssignMode::Set => {
                        let map: std::collections::BTreeMap<usize, String> =
                            values.into_iter().enumerate().collect();
                        self.arrays.insert(name.clone(), map);
                    }
                    AssignMode::Append => {
                        let map = self.arrays.entry(name.clone()).or_default();
                        let mut next = map.keys().next_back().map(|k| k + 1).unwrap_or(0);
                        for v in values {
                            map.insert(next, v);
                            next += 1;
                        }
                    }
                }
            }
            for (name, index, val) in &cmd.index_assigns {
                let v = self.expand_word(val);
                self.array_set_index(name, index, v);
            }
            if !cmd.redirects.is_empty() {
                // side effect only: create/truncate/append the target files
                let _ = self.resolve_redirects(cmd);
            }
            return ExecResult::Status(0);
        }

        let saved_stderr_target = self.current_stderr_target.take();
        self.current_stderr_target = self.peek_stderr_target(&cmd.redirects);
        let first_word_literal = match cmd.words[0].chunks.as_slice() {
            // Quoting a builtin's own name doesn't stop bash from
            // recognizing it (`"export" FOO=bar` still runs export), so
            // this must match a fully-quoted name too, not just a bare one.
            [Chunk::Str(s)] | [Chunk::LiteralStr(s)] => Some(s.as_str()),
            _ => None,
        };
        let argv: Vec<String> = if matches!(
            first_word_literal,
            Some("local") | Some("export") | Some("declare") | Some("typeset") | Some("readonly")
        ) {
            // Assignment-builtins: `NAME=value` arguments must not be
            // word-split on the expanded value (bash treats them like any
            // other assignment), unlike a normal builtin's arguments.
            let mut v = vec![first_word_literal.unwrap().to_string()];
            for w in &cmd.words[1..] {
                if let Some((name, _mode, val_word)) = parser::word_as_assignment(w) {
                    v.push(format!("{}={}", name, self.expand_word(&val_word)));
                } else {
                    v.push(self.expand_word(w));
                }
            }
            v
        } else {
            self.expand_words(&cmd.words)
        };
        self.current_stderr_target = saved_stderr_target;
        if argv.is_empty() {
            // Every word vanished (e.g. the command was just an unquoted
            // empty/unset variable) -- matches bash: nothing runs.
            return ExecResult::Status(0);
        }
        if self.opt_xtrace {
            eprintln!("+ {}", argv.join(" "));
        }
        let name = argv[0].clone();

        // Builtins ignore per-command redirects for now: their output goes
        // straight to the shell's own stdio.
        match name.as_str() {
            // POSIX special builtin: does nothing, exits 0. Its arguments
            // are still expanded (already done via argv above) for side
            // effects like `: ${x:=default}`; this shell's builtins don't
            // yet honor their own redirects (see the comment on the
            // `Builtins ignore per-command redirects` note elsewhere in
            // this file), so `: > file` won't truncate `file` here, same
            // limitation as every other builtin.
            ":" => return ExecResult::Status(0),
            // command [-v|-V] name [args...]. -v/-V (by far the dominant
            // real-world use, e.g. `command -v git >/dev/null`) report
            // what `name` resolves to without running it. Bare `command
            // name args...` runs it, bypassing shell functions -- the
            // other thing `command` is for -- by spawning directly as an
            // external process; unlike real bash this also bypasses
            // builtins of the same name, a simplification accepted since
            // this shell doesn't have many commonly-shadowed builtins.
            "command" => {
                let mut i = 1;
                let mut mode_v = false;
                let mut mode_vv = false;
                while i < argv.len() {
                    match argv[i].as_str() {
                        "-v" => {
                            mode_v = true;
                            i += 1;
                        }
                        "-V" => {
                            mode_vv = true;
                            i += 1;
                        }
                        "-p" => i += 1,
                        _ => break,
                    }
                }
                if mode_v || mode_vv {
                    return ExecResult::Status(match argv.get(i) {
                        Some(n) => self.command_v(n, mode_vv),
                        None => 1,
                    });
                }
                if i >= argv.len() {
                    return ExecResult::Status(0);
                }
                let mut ext = Command::new(&argv[i]);
                ext.args(&argv[i + 1..]);
                ext.current_dir(&self.cwd);
                return match ext.status() {
                    Ok(status) => ExecResult::Status(exit_code_from_status(status)),
                    Err(e) => {
                        eprintln!("bish: command: {}: {}", argv[i], e);
                        ExecResult::Status(127)
                    }
                };
            }
            "type" => return ExecResult::Status(self.run_type(&argv[1..])),
            // No command-path cache exists to manage -- every exec
            // re-resolves PATH via Command::status() itself -- so this is
            // a documented no-op rather than a real cache: `hash -r`
            // (clear) and bare `hash cmd` (remember) both just succeed.
            // Bare `hash` with no args normally lists the cache; ours is
            // always empty, so that's the one output bash-compat requires.
            "hash" => {
                if argv.len() == 1 {
                    println!("hash: hash table empty");
                }
                return ExecResult::Status(0);
            }
            "cd" => return ExecResult::Status(self.run_cd(&argv[1..])),
            "umask" => return ExecResult::Status(builtins::umask(&argv[1..])),
            "ulimit" => return ExecResult::Status(builtins::ulimit(&argv[1..])),
            // alias/unalias: store and query only, no expansion when a
            // command runs -- see the comment on the `aliases` field for
            // why.
            "alias" => {
                if argv.len() == 1 {
                    let mut sorted: Vec<&(String, String)> = self.aliases.iter().collect();
                    sorted.sort_by(|a, b| a.0.cmp(&b.0));
                    for (n, v) in sorted {
                        println!("alias {}={}", n, crate::serialize::quote_literal(v));
                    }
                    return ExecResult::Status(0);
                }
                let mut status = 0;
                for a in &argv[1..] {
                    match a.find('=') {
                        Some(eq) => {
                            let name = a[..eq].to_string();
                            let val = a[eq + 1..].to_string();
                            match self.aliases.iter_mut().find(|(n, _)| *n == name) {
                                Some(existing) => existing.1 = val,
                                None => self.aliases.push((name, val)),
                            }
                        }
                        None => match self.aliases.iter().find(|(n, _)| n == a) {
                            Some((n, v)) => println!("alias {}={}", n, crate::serialize::quote_literal(v)),
                            None => {
                                eprintln!("bish: alias: {}: not found", a);
                                status = 1;
                            }
                        },
                    }
                }
                return ExecResult::Status(status);
            }
            "unalias" => {
                if argv[1..].iter().any(|a| a == "-a") {
                    self.aliases.clear();
                    return ExecResult::Status(0);
                }
                let mut status = 0;
                for a in &argv[1..] {
                    match self.aliases.iter().position(|(n, _)| n == a) {
                        Some(pos) => {
                            self.aliases.remove(pos);
                        }
                        None => {
                            eprintln!("bish: unalias: {}: not found", a);
                            status = 1;
                        }
                    }
                }
                return ExecResult::Status(status);
            }
            "shopt" => return ExecResult::Status(self.run_shopt(&argv[1..])),
            // Command-mode-exclusive: this extends the builtin set
            // available there specifically, rather than being a normal
            // shell builtin -- with the guard false, this arm doesn't
            // match and `window` falls through to the same external-
            // command lookup any other unrecognized name would hit.
            "window" | "w" | "win" if self.restrict_to_builtins => {
                return ExecResult::Status(self.run_window(&argv[1..]));
            }
            "pushd" => return ExecResult::Status(self.run_pushd(&argv[1..])),
            "popd" => return ExecResult::Status(self.run_popd(&argv[1..])),
            "dirs" => return ExecResult::Status(self.run_dirs(&argv[1..])),
            // `export` is equivalent to `declare -x` (real bash documents
            // it that way) -- routing through run_declare means it shares
            // -x's local-variable-mirroring behavior instead of the
            // simpler, env-only handling this had before.
            "export" => {
                let mut declare_args = vec!["-x".to_string()];
                declare_args.extend(argv[1..].iter().cloned());
                return ExecResult::Status(self.run_declare(&declare_args));
            }
            "let" => {
                let mut last = 0i64;
                for a in &argv[1..] {
                    match arith::eval(a, self) {
                        Ok(v) => last = v,
                        Err(e) => {
                            eprintln!("bish: let: {}", e);
                            return ExecResult::Status(2);
                        }
                    }
                }
                return ExecResult::Status(if last != 0 { 0 } else { 1 });
            }
            "break" => return builtins::break_loop(&argv[1..]),
            "continue" => return builtins::continue_loop(&argv[1..]),
            "test" => return ExecResult::Status(builtins::test(&argv[1..], false)),
            // `[[` is a keyword (see lexer.rs/parser.rs Command::Test), not
            // a plain command name, so it never reaches this dispatch --
            // only bracket-style `[ ... ]` (the `test` alias) does.
            "[" => {
                let mut a = argv[1..].to_vec();
                if a.last().map(|s| s.as_str()) == Some("]") {
                    a.pop();
                } else {
                    eprintln!("bish: [: missing closing ]");
                    return ExecResult::Status(2);
                }
                return ExecResult::Status(builtins::test(&a, false));
            }
            "return" => {
                let code = argv.get(1).and_then(|s| s.parse::<i32>().ok()).unwrap_or(self.last_status);
                if self.var_scopes.is_empty() {
                    eprintln!("bish: return: can only 'return' from a function");
                    return ExecResult::Status(code);
                }
                return ExecResult::Return(code);
            }
            "shift" => {
                let n = argv.get(1).and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                if let Some(frame) = self.arg_frames.last_mut() {
                    let drain = n.min(frame.len());
                    frame.drain(0..drain);
                }
                return ExecResult::Status(0);
            }
            "local" => {
                if self.var_scopes.is_empty() {
                    eprintln!("bish: local: can only be used inside a function");
                    return ExecResult::Status(1);
                }
                // `-a`/`-A` name an array rather than a scalar. `arrays`/
                // `assoc_arrays` themselves stay flat maps (see the
                // comments on those fields), but the pre-local value is
                // snapshotted onto array_local_stack/assoc_local_stack so
                // call_function can restore it when the function returns --
                // giving `local -a`/`-A` real scoping without needing every
                // array read/write site to walk a scope chain.
                let mut array_mode: Option<bool> = None;
                let mut integer_flag = false;
                let mut nameref_flag = false;
                // -u/-l/-x attribute membership (like -i above) isn't
                // unwound when the function returns -- a narrower, already-
                // accepted gap than the array/nameref leak this function
                // otherwise guards against, since getting a case-fold or
                // export attribute stuck past its function is cosmetic
                // rather than silently wrong data.
                let mut upper_flag = false;
                let mut lower_flag = false;
                let mut export_flag = false;
                for a in &argv[1..] {
                    match a.as_str() {
                        "-a" => {
                            array_mode = Some(false);
                            continue;
                        }
                        "-A" => {
                            array_mode = Some(true);
                            continue;
                        }
                        "-i" => {
                            integer_flag = true;
                            continue;
                        }
                        "-n" => {
                            nameref_flag = true;
                            continue;
                        }
                        "-u" => {
                            upper_flag = true;
                            continue;
                        }
                        "-l" => {
                            lower_flag = true;
                            continue;
                        }
                        "-x" => {
                            export_flag = true;
                            continue;
                        }
                        _ if a.starts_with('-') => continue,
                        _ => {}
                    }
                    let (n, v) = match a.find('=') {
                        Some(eq) => (a[..eq].to_string(), Some(a[eq + 1..].to_string())),
                        None => (a.clone(), None),
                    };
                    if nameref_flag {
                        let was_nameref = self.nameref_names.contains(&n);
                        self.nameref_local_stack.last_mut().unwrap().push((n.clone(), was_nameref));
                        self.nameref_names.insert(n.clone());
                        self.var_scopes.last_mut().unwrap().insert(n.clone(), v.unwrap_or_default());
                        continue;
                    }
                    if integer_flag {
                        self.integer_names.insert(n.clone());
                    }
                    if upper_flag {
                        self.upper_names.insert(n.clone());
                    }
                    if lower_flag {
                        self.lower_names.insert(n.clone());
                    }
                    if export_flag {
                        self.exported_names.insert(n.clone());
                    }
                    match array_mode {
                        Some(true) => {
                            let prev = self.assoc_arrays.remove(&n);
                            self.assoc_local_stack.last_mut().unwrap().push((n.clone(), prev));
                            self.assoc_names.insert(n.clone());
                            self.assoc_arrays.insert(n, OrderedMap::default());
                        }
                        Some(false) => {
                            let prev = self.arrays.remove(&n);
                            self.array_local_stack.last_mut().unwrap().push((n.clone(), prev));
                            self.arrays.insert(n, std::collections::BTreeMap::new());
                        }
                        None => {
                            let v = v.unwrap_or_default();
                            let v = if integer_flag { arith::eval(&v, self).unwrap_or(0).to_string() } else { v };
                            let v = if upper_flag {
                                v.to_uppercase()
                            } else if lower_flag {
                                v.to_lowercase()
                            } else {
                                v
                            };
                            if export_flag {
                                unsafe {
                                    std::env::set_var(&n, &v);
                                }
                            }
                            self.var_scopes.last_mut().unwrap().insert(n, v);
                        }
                    }
                }
                return ExecResult::Status(0);
            }
            "exit" => {
                let code = argv
                    .get(1)
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(self.last_status);
                self.run_exit_trap();
                std::process::exit(code);
            }
            "read" => {
                let mut array_name: Option<&str> = None;
                let mut names: Vec<&str> = Vec::new();
                let mut prompt: Option<&str> = None;
                let mut nchars: Option<usize> = None;
                let mut delim: u8 = b'\n';
                let mut read_u_flag: Option<&str> = None;
                // -s (silent/no-echo) needs raw termios manipulation to
                // suppress terminal echo, which isn't implementable without
                // hand-rolling the platform's struct termios layout (risky
                // to get right without the libc crate, and this project's
                // bash-diff test methodology has no real tty to exercise it
                // against anyway) -- accepted and parsed, but not yet
                // functional, same scoped-gap treatment as mapfile's -s/-u.
                let mut i = 1;
                while i < argv.len() {
                    match argv[i].as_str() {
                        "-r" | "-s" => i += 1,
                        "-a" => {
                            array_name = argv.get(i + 1).map(|s| s.as_str());
                            i += 2;
                        }
                        "-p" => {
                            prompt = argv.get(i + 1).map(|s| s.as_str());
                            i += 2;
                        }
                        "-n" | "-N" => {
                            nchars = argv.get(i + 1).and_then(|s| s.parse::<usize>().ok());
                            i += 2;
                        }
                        "-d" => {
                            delim = argv.get(i + 1).and_then(|s| s.bytes().next()).unwrap_or(b'\n');
                            i += 2;
                        }
                        "-t" => {
                            // Parsed and consumed for arg-shape compatibility;
                            // actual timeout enforcement happens below, but
                            // only against real stdin (see is_real_stdin).
                            i += 2;
                        }
                        "-u" => {
                            read_u_flag = argv.get(i + 1).map(|s| s.as_str());
                            i += 2;
                        }
                        other => {
                            names.push(other);
                            i += 1;
                        }
                    }
                }
                let timeout_secs = argv.iter().position(|a| a == "-t").and_then(|p| argv.get(p + 1)).and_then(|s| s.parse::<f64>().ok());
                let is_real_stdin = !cmd
                    .redirects
                    .iter()
                    .any(|r| matches!(r, Redirect::In(_) | Redirect::HereString(_) | Redirect::HereDoc(_)));
                if let Some(p) = prompt {
                    if is_real_stdin && stdin_is_tty() {
                        eprint!("{}", p);
                        let _ = std::io::Write::flush(&mut std::io::stderr());
                    }
                }
                if let Some(secs) = timeout_secs {
                    if is_real_stdin {
                        let ms = (secs * 1000.0).max(0.0) as i32;
                        if !stdin_ready(ms) {
                            return ExecResult::Status(1);
                        }
                    }
                    // A non-stdin source (file/heredoc/here-string redirect)
                    // has no pollable fd for a meaningful timeout check, so
                    // -t against those is treated as immediately ready.
                }

                // `-u FD` reads from a fd this shell already has open (most
                // commonly a coproc's read end, coproc_fds[NAME[0]]) instead
                // of the command's own stdin/redirect -- borrowed tightly in
                // its own block so the borrow of self.coproc_fds ends before
                // the assign_var calls below need `&mut self` again.
                let ufd = read_u_flag.and_then(|s| s.parse::<i32>().ok());
                let (got, clean): (Option<String>, bool) = if let Some(fd) = ufd {
                    match self.coproc_fds.get_mut(&fd) {
                        Some(KeptFd::Read(r)) => read_line_or_chars(r, nchars, delim),
                        _ => {
                            eprintln!("bish: read: {}: invalid file descriptor", fd);
                            return ExecResult::Status(1);
                        }
                    }
                } else {
                    let mut reader = self.read_input_source(cmd);
                    read_line_or_chars(&mut *reader, nchars, delim)
                };

                return match got {
                    None => ExecResult::Status(1),
                    Some(line) => {
                        let line = line.as_str();
                        let ifs = self.get_ifs();
                        if let Some(arr) = array_name {
                            let (parts, ..) = ifs_tokenize(line, &ifs);
                            let map: std::collections::BTreeMap<usize, String> =
                                parts.into_iter().enumerate().collect();
                            self.arrays.insert(arr.to_string(), map);
                        } else if names.is_empty() {
                            self.assign_var("REPLY", line.to_string());
                        } else {
                            let is_ifs_ws = |c: char| c.is_whitespace() && ifs.contains(c);
                            let mut rest = line.trim_start_matches(is_ifs_ws).to_string();
                            for (i, n) in names.iter().enumerate() {
                                if i == names.len() - 1 {
                                    self.assign_var(n, rest.trim_end_matches(is_ifs_ws).to_string());
                                } else {
                                    match ifs_next_field(&rest, &ifs) {
                                        Some((field, remainder)) => {
                                            self.assign_var(n, field);
                                            rest = remainder;
                                        }
                                        None => {
                                            self.assign_var(n, rest.clone());
                                            rest = String::new();
                                        }
                                    }
                                }
                            }
                        }
                        ExecResult::Status(if clean { 0 } else { 1 })
                    }
                };
            }
            // mapfile/readarray [-t] [name]. Reads lines from stdin (or
            // this command's own `<` redirect, via the same
            // read_input_source machinery `read` uses) into an indexed
            // array, one element per line -- default array name MAPFILE.
            // Only `-t` (strip trailing newlines) is recognized; other
            // real bash flags (-n/-O/-s/-C/-c/-u/-d) are accepted and
            // ignored rather than erroring, a scoped subset covering the
            // overwhelmingly common `mapfile -t arr < file` usage.
            "mapfile" | "readarray" => {
                let mut strip_newline = false;
                let mut array_name = "MAPFILE".to_string();
                for a in &argv[1..] {
                    match a.as_str() {
                        "-t" => strip_newline = true,
                        other if !other.starts_with('-') => array_name = other.to_string(),
                        _ => {}
                    }
                }
                let mut reader = self.read_input_source(cmd);
                let mut map = std::collections::BTreeMap::new();
                let mut idx = 0usize;
                loop {
                    let mut line = String::new();
                    match std::io::BufRead::read_line(&mut *reader, &mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if strip_newline {
                                line = line.trim_end_matches(['\n', '\r']).to_string();
                            }
                            map.insert(idx, line);
                            idx += 1;
                        }
                        Err(_) => break,
                    }
                }
                self.arrays.insert(array_name, map);
                return ExecResult::Status(0);
            }
            "eval" => {
                let src = argv[1..].join(" ");
                return self.run_source_here(&src, "eval");
            }
            "source" | "." => {
                let path = match argv.get(1) {
                    Some(p) => p.clone(),
                    None => {
                        eprintln!("bish: {}: filename argument required", name);
                        return ExecResult::Status(2);
                    }
                };
                match std::fs::read_to_string(&path) {
                    Ok(src) => return self.run_source_here(&src, &path),
                    Err(e) => {
                        eprintln!("bish: {}: {}", path, e);
                        return ExecResult::Status(1);
                    }
                }
            }
            "trap" => {
                if argv.len() == 1 || argv.get(1).map(|s| s == "-p").unwrap_or(false) {
                    if let Some(code) = &self.exit_trap {
                        println!("trap -- {} EXIT", crate::serialize::quote_literal(code));
                    }
                    let mut entries: Vec<(i32, TrapAction)> =
                        self.traps.iter().map(|(k, v)| (*k, v.clone())).collect();
                    entries.sort_by_key(|(n, _)| *n);
                    for (n, action) in entries {
                        match action {
                            TrapAction::Run(code) => {
                                println!("trap -- {} SIG{}", crate::serialize::quote_literal(&code), signal_name(n))
                            }
                            TrapAction::Ignore => println!("trap -- '' SIG{}", signal_name(n)),
                        }
                    }
                    return ExecResult::Status(0);
                }
                if argv.len() < 3 {
                    return ExecResult::Status(0);
                }
                let cmd_str = argv[1].clone();
                for sig in &argv[2..] {
                    if sig == "EXIT" || sig == "0" {
                        self.exit_trap = if cmd_str == "-" { None } else { Some(cmd_str.clone()) };
                        continue;
                    }
                    let num = match signal_number(sig) {
                        Some(n) => n,
                        None => {
                            eprintln!("bish: trap: {}: invalid signal specification", sig);
                            continue;
                        }
                    };
                    if num == 9 || num == 19 {
                        eprintln!("bish: trap: {}: cannot trap", sig);
                        continue;
                    }
                    if cmd_str == "-" {
                        self.traps.remove(&num);
                        sigaction_raw(num, SIG_DFL);
                    } else if cmd_str.is_empty() {
                        self.traps.insert(num, TrapAction::Ignore);
                        sigaction_raw(num, SIG_IGN);
                    } else {
                        self.traps.insert(num, TrapAction::Run(cmd_str.clone()));
                        sigaction_raw(num, record_pending_signal as *const () as usize);
                    }
                }
                return ExecResult::Status(0);
            }
            "jobs" => return ExecResult::Status(self.run_jobs(&argv[1..])),
            "fg" => return ExecResult::Status(self.run_fg(&argv[1..])),
            "bg" => return ExecResult::Status(self.run_bg(&argv[1..])),
            "wait" => return ExecResult::Status(self.run_wait(&argv[1..])),
            "kill" => return ExecResult::Status(self.run_kill(&argv[1..])),
            "getopts" => return self.run_getopts(&argv[1..]),
            "unset" => {
                let target = self.peek_stderr_target(&cmd.redirects);
                return ExecResult::Status(self.run_unset(&argv[1..], &target));
            }
            "set" => return ExecResult::Status(self.run_set(&argv[1..])),
            "declare" | "typeset" => return ExecResult::Status(self.run_declare(&argv[1..])),
            "readonly" => return ExecResult::Status(self.run_readonly(&argv[1..])),
            // exec CMD [args...] replaces this process image entirely (no
            // fork, no return on success) -- exactly what real bash does,
            // and available here as safe std (CommandExt::exec wraps
            // execvp, distinct from the fork() this shell avoids).
            "exec" if argv.len() > 1 => {
                let redirs = match self.resolve_redirects(cmd) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("bish: {}", e);
                        return ExecResult::Status(1);
                    }
                };
                let mut command = Command::new(&argv[1]);
                command.args(&argv[2..]);
                if let Some(s) = redirs.stdin {
                    command.stdin(s);
                }
                if let Some(s) = redirs.stdout {
                    command.stdout(s);
                }
                if let Some(s) = redirs.stderr {
                    command.stderr(s);
                }
                // Numbered-fd forms (`exec cmd 3>file`) can't go through
                // apply_fd_redirects' pre_exec hook -- CommandExt::exec
                // below is a direct execve of *this* process, no fork, so
                // there's no child to install a pre_exec closure into.
                if let Err(e) = apply_fds_to_self(redirs.dup_stderr_to_stdout, redirs.extra_fds) {
                    eprintln!("bish: exec: {}", e);
                    return ExecResult::Status(1);
                }
                let err = command.exec();
                eprintln!("bish: exec: {}: {}", argv[1], err);
                // Real bash: a non-interactive shell exits immediately when
                // exec fails to find/run the command, and (confirmed via a
                // clean probe against bash 5.0.17) leaves $? at whatever it
                // already was rather than setting 127 -- surprising, but
                // that's what it actually does.
                self.run_exit_trap();
                std::process::exit(self.last_status);
            }
            // Bare `exec` (no command word): persistently applies its
            // redirects -- numbered-fd (`exec 3>file`, `exec 3>&1`,
            // `exec 3>&-`) and plain fd 0/1/2 (`exec > file`,
            // `exec 2>>err.log`) alike -- to this shell's own process, so
            // subsequently spawned commands inherit them. Covers idioms
            // like `exec > logfile 2>&1` (whole-script output redirection)
            // and the `exec 3>&1 1>logfile; cmd; exec 1>&3 3>&-` fd-
            // juggling trick.
            "exec" => {
                let redirs = match self.resolve_redirects(cmd) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("bish: {}", e);
                        return ExecResult::Status(1);
                    }
                };
                let (stdin, stdout, stderr) = match self.resolve_plain_fd012(&cmd.redirects) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("bish: {}", e);
                        return ExecResult::Status(1);
                    }
                };
                if let Err(e) = apply_fd012_to_self(stdin, stdout, stderr) {
                    eprintln!("bish: exec: {}", e);
                    return ExecResult::Status(1);
                }
                if let Err(e) = apply_fds_to_self(redirs.dup_stderr_to_stdout, redirs.extra_fds) {
                    eprintln!("bish: exec: {}", e);
                    return ExecResult::Status(1);
                }
                return ExecResult::Status(0);
            }
            _ => {}
        }

        if self.restrict_to_builtins {
            eprintln!("bish: {}: command mode only allows builtins -- use `command {}` to run it", name, name);
            return ExecResult::Status(127);
        }

        if let Some(body) = self.functions.get(&name).cloned() {
            return self.call_function(&body, argv[1..].to_vec());
        }

        let redirs = match self.resolve_redirects(cmd) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("bish: {}", e);
                return ExecResult::Status(1);
            }
        };

        let mut command = Command::new(&name);
        command.args(&argv[1..]);
        command.current_dir(&self.cwd);
        for (k, mode, val) in &cmd.assigns {
            let v = self.expand_word(val);
            let v = match mode {
                AssignMode::Set => v,
                AssignMode::Append => self.lookup_var(k) + &v,
            };
            command.env(k, v);
        }
        command.stdin(redirs.stdin.unwrap_or_else(Stdio::inherit));
        command.stdout(redirs.stdout.unwrap_or_else(Stdio::inherit));
        command.stderr(redirs.stderr.unwrap_or_else(Stdio::inherit));
        apply_fd_redirects(&mut command, redirs.dup_stderr_to_stdout, redirs.extra_fds);

        match command.spawn() {
            Ok(child) => {
                if background {
                    let mut cmd_text = argv.join(" ");
                    for r in &cmd.redirects {
                        cmd_text.push(' ');
                        cmd_text.push_str(&crate::serialize::serialize_redirect(r));
                    }
                    self.push_job(vec![child], cmd_text);
                    ExecResult::Status(0)
                } else {
                    let mut child = child;
                    let result = match child.wait() {
                        Ok(status) => ExecResult::Status(exit_code_from_status(status)),
                        Err(e) => {
                            eprintln!("bish: {}", e);
                            ExecResult::Status(1)
                        }
                    };
                    self.drain_proc_subs();
                    result
                }
            }
            Err(e) => {
                let msg = format!("bish: {}: {}", name, e);
                self.write_command_error(cmd, &msg);
                self.drain_proc_subs();
                ExecResult::Status(127)
            }
        }
    }

    // Every pipeline stage is a separate process by necessity (that's what
    // makes piping possible at all), so compound-command stages self-exec
    // just like Subshell already does -- this is actually the *correct*
    // bash semantics too: piped stages always fork, even in real bash.
    fn run_multi(&mut self, commands: &[parser::Command], background: bool) -> i32 {
        let n = commands.len();
        let mut children: Vec<std::process::Child> = Vec::with_capacity(n);
        let mut prev_stdout: Option<Stdio> = None;

        for (i, cmd) in commands.iter().enumerate() {
            let is_last = i == n - 1;
            let default_stdin = prev_stdout.take().unwrap_or_else(Stdio::inherit);
            let default_stdout = if is_last { Stdio::inherit() } else { Stdio::piped() };

            let mut command = match cmd {
                parser::Command::Simple(sc) => {
                    if sc.words.is_empty() {
                        eprintln!("bish: syntax error in pipeline");
                        kill_all(children);
                        return 1;
                    }
                    let argv: Vec<String> = self.expand_words(&sc.words);
                    if argv.is_empty() {
                        continue;
                    }
                    let redirs = match self.resolve_redirects(sc) {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("bish: {}", e);
                            kill_all(children);
                            return 1;
                        }
                    };
                    // Builtins and shell functions aren't real executables,
                    // so they can't be Command::new'd directly. Route them
                    // through the same self-exec trick used for compound
                    // commands below, replaying the *already-expanded*
                    // argv as single-quoted literals so the child doesn't
                    // re-run any command substitution/glob/etc a second
                    // time.
                    let mut command = if is_known_builtin(&argv[0]) || self.functions.contains_key(&argv[0]) {
                        let exe = match std::env::current_exe() {
                            Ok(p) => p,
                            Err(e) => {
                                eprintln!("bish: {}", e);
                                kill_all(children);
                                return 1;
                            }
                        };
                        let script_line: String = argv
                            .iter()
                            .map(|a| crate::serialize::quote_literal(a))
                            .collect::<Vec<_>>()
                            .join(" ");
                        let script = self.functions_preamble() + &script_line;
                        let mut command = Command::new(exe);
                        command.arg("-c").arg(script);
                        command
                    } else {
                        let mut command = Command::new(&argv[0]);
                        command.args(&argv[1..]);
                        command
                    };
                    for (k, mode, val) in &sc.assigns {
                        let v = self.expand_word(val);
                        let v = match mode {
                            AssignMode::Set => v,
                            AssignMode::Append => self.lookup_var(k) + &v,
                        };
                        command.env(k, v);
                    }
                    command.stdin(redirs.stdin.unwrap_or(default_stdin));
                    command.stdout(redirs.stdout.unwrap_or(default_stdout));
                    command.stderr(redirs.stderr.unwrap_or_else(Stdio::inherit));
                    apply_fd_redirects(&mut command, redirs.dup_stderr_to_stdout, redirs.extra_fds);
                    command
                }
                other => {
                    let exe = match std::env::current_exe() {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("bish: {}", e);
                            kill_all(children);
                            return 1;
                        }
                    };
                    let own_redirects = command_own_redirects(other);
                    let redirs = if own_redirects.is_empty() {
                        ResolvedRedirs {
                            stdin: None,
                            stdout: None,
                            stderr: None,
                            dup_stderr_to_stdout: false,
                            extra_fds: Vec::new(),
                        }
                    } else {
                        match self.resolve_redirect_list(own_redirects) {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("bish: {}", e);
                                kill_all(children);
                                return 1;
                            }
                        }
                    };
                    let script = self.functions_preamble() + &crate::serialize::serialize_command(other);
                    let mut command = Command::new(exe);
                    command.arg("-c").arg(script);
                    command.stdin(redirs.stdin.unwrap_or(default_stdin));
                    command.stdout(redirs.stdout.unwrap_or(default_stdout));
                    command.stderr(redirs.stderr.unwrap_or_else(Stdio::inherit));
                    apply_fd_redirects(&mut command, redirs.dup_stderr_to_stdout, redirs.extra_fds);
                    command
                }
            };
            command.current_dir(&self.cwd);

            match command.spawn() {
                Ok(mut child) => {
                    if !is_last {
                        prev_stdout = child.stdout.take().map(Stdio::from);
                    }
                    children.push(child);
                }
                Err(e) => {
                    eprintln!("bish: {}", e);
                    kill_all(children);
                    return 127;
                }
            }
        }

        if background {
            let cmd_text =
                commands.iter().map(crate::serialize::serialize_command).collect::<Vec<_>>().join(" | ");
            self.push_job(children, cmd_text);
            return 0;
        }

        let mut status = 0;
        let mut pipefail_status = 0;
        for mut c in children {
            let code = match c.wait() {
                Ok(s) => exit_code_from_status(s),
                Err(e) => {
                    eprintln!("bish: {}", e);
                    1
                }
            };
            status = code;
            if code != 0 {
                pipefail_status = code;
            }
        }
        if self.opt_pipefail {
            pipefail_status
        } else {
            status
        }
    }

    fn expand_word(&mut self, w: &Word) -> String {
        let mut s = String::new();
        for c in &w.chunks {
            match c {
                Chunk::Str(t) | Chunk::LiteralStr(t) => s.push_str(t),
                Chunk::Var { name, .. } => {
                    let name = name.clone();
                    self.check_nounset(&name);
                    s.push_str(&self.lookup_var(&name));
                }
                Chunk::Sub { raw, .. } => s.push_str(&self.run_command_substitution(raw)),
                Chunk::Arith { raw, .. } => match arith::eval(raw, self) {
                    Ok(v) => s.push_str(&v.to_string()),
                    Err(e) => eprintln!("bish: (({})): {}", raw, e),
                },
                Chunk::VarExpand { name, op, .. } => {
                    let name = name.clone();
                    let op = op.clone();
                    s.push_str(&self.eval_var_op(&name, &op));
                }
                Chunk::ArrayVar { name, index, .. } => {
                    let name = name.clone();
                    let index = index.clone();
                    s.push_str(&self.array_element(&name, &index));
                }
                Chunk::ArrayLength { name, index } => {
                    let name = name.clone();
                    let index = index.clone();
                    s.push_str(&self.array_length(&name, &index).to_string());
                }
                Chunk::ArrayVarExpand { name, index, op, .. } => {
                    let name = name.clone();
                    let index = index.clone();
                    let op = op.clone();
                    s.push_str(&self.eval_array_var_op(&name, &index, &op));
                }
                Chunk::Indirect { name, .. } => {
                    let target = self.lookup_var(name);
                    s.push_str(&self.lookup_var(&target));
                }
                Chunk::ArrayKeys { name, .. } => {
                    let name = name.clone();
                    s.push_str(&self.array_keys(&name).join(" "));
                }
                Chunk::ProcSubIn { raw } => {
                    let raw = raw.clone();
                    s.push_str(&self.run_proc_sub_in(&raw));
                }
                Chunk::ProcSubOut { raw } => {
                    let raw = raw.clone();
                    s.push_str(&self.run_proc_sub_out(&raw));
                }
            }
        }
        s
    }

    // index "@"/"*" joins all elements (in index order) with a space (used
    // outside the splitting-aware path, where "@" vs "*" can't be
    // distinguished anyway); any other index is evaluated as an arithmetic
    // expression (so `${arr[i+1]}` works) and looked up 0-based. A gap
    // index (never set) reads back as empty, same as an unset scalar.
    // Associative-array indices are literal (expandable) strings, not
    // arithmetic expressions -- re-lex the raw index text and expand it,
    // same as expand_raw (an index like `with space` re-lexes as more than
    // one Word token; taking only the first would silently truncate it).
    fn expand_index_as_string(&mut self, index: &str) -> String {
        self.expand_raw(index)
    }

    // Negative indices count back from the end: bash defines them as
    // relative to one greater than the array's maximum set index, so -1 is
    // the last (highest-index) element. Only meaningful for indexed arrays
    // -- associative-array indices are plain string keys, never resolved
    // here.
    fn resolve_array_index(&self, name: &str, i: i64) -> Option<usize> {
        if i >= 0 {
            return Some(i as usize);
        }
        let max = *self.arrays.get(name)?.keys().next_back()?;
        let resolved = max as i64 + 1 + i;
        if resolved >= 0 {
            Some(resolved as usize)
        } else {
            None
        }
    }

    fn array_element(&mut self, name: &str, index: &str) -> String {
        if index == "@" || index == "*" {
            return self.array_all(name).join(" ");
        }
        if self.assoc_names.contains(name) {
            let key = self.expand_index_as_string(index);
            return self.assoc_arrays.get(name).and_then(|m| m.get(&key)).cloned().unwrap_or_default();
        }
        match arith::eval(index, self) {
            Ok(i) => match self.resolve_array_index(name, i) {
                Some(idx) => self.arrays.get(name).and_then(|m| m.get(&idx)).cloned().unwrap_or_default(),
                None => String::new(),
            },
            Err(_) => String::new(),
        }
    }

    // Array-element analog of var_is_set: is this specific index actually
    // present in the (sparse) array, not just reading back empty because
    // it was never set.
    fn array_element_is_set(&mut self, name: &str, index: &str) -> bool {
        if index == "@" || index == "*" {
            return !self.array_all(name).is_empty();
        }
        if self.assoc_names.contains(name) {
            let key = self.expand_index_as_string(index);
            return self.assoc_arrays.get(name).is_some_and(|m| m.contains_key(&key));
        }
        match arith::eval(index, self) {
            Ok(i) => match self.resolve_array_index(name, i) {
                Some(idx) => self.arrays.get(name).is_some_and(|m| m.contains_key(&idx)),
                None => false,
            },
            Err(_) => false,
        }
    }

    fn array_keys(&self, name: &str) -> Vec<String> {
        if let Some(m) = self.assoc_arrays.get(name) {
            return m.keys().cloned().collect();
        }
        self.arrays.get(name).map(|m| m.keys().map(|k| k.to_string()).collect()).unwrap_or_default()
    }

    fn array_all(&self, name: &str) -> Vec<String> {
        if let Some(m) = self.assoc_arrays.get(name) {
            return m.values().cloned().collect();
        }
        self.arrays.get(name).map(|m| m.values().cloned().collect()).unwrap_or_default()
    }

    // "@"/"*" counts only set elements (real bash arrays are sparse --
    // `arr[10]=x` alone gives a length of 1, not 11).
    fn array_length(&mut self, name: &str, index: &str) -> usize {
        if index == "@" || index == "*" {
            if let Some(m) = self.assoc_arrays.get(name) {
                return m.len();
            }
            return self.arrays.get(name).map(|m| m.len()).unwrap_or(0);
        }
        if self.assoc_names.contains(name) {
            let key = self.expand_index_as_string(index);
            return self
                .assoc_arrays
                .get(name)
                .and_then(|m| m.get(&key))
                .map(|s| s.chars().count())
                .unwrap_or(0);
        }
        match arith::eval(index, self) {
            Ok(i) => match self.resolve_array_index(name, i) {
                Some(idx) => self
                    .arrays
                    .get(name)
                    .and_then(|m| m.get(&idx))
                    .map(|s| s.chars().count())
                    .unwrap_or(0),
                None => 0,
            },
            Err(_) => 0,
        }
    }

    // `arr[i]=value`. Sets exactly that index -- no resizing/filling, since
    // the array is a sparse map, matching bash (gaps stay genuinely unset).
    fn array_set_index(&mut self, name: &str, index: &str, value: String) {
        if self.assoc_names.contains(name) {
            let key = self.expand_index_as_string(index);
            self.assoc_arrays.entry(name.to_string()).or_default().insert(key, value);
            return;
        }
        let i = match arith::eval(index, self) {
            Ok(i) => match self.resolve_array_index(name, i) {
                Some(idx) => idx,
                None => {
                    eprintln!("bish: {}: bad array index: {}", name, index);
                    return;
                }
            },
            Err(_) => {
                eprintln!("bish: {}: bad array index: {}", name, index);
                return;
            }
        };
        self.arrays.entry(name.to_string()).or_default().insert(i, value);
    }

    // Bash word-splitting: unquoted expansion results are split on
    // whitespace (IFS, hardcoded to the default here) into separate fields;
    // literal text (whether from quotes or plain source) never splits, since
    // unquoted literal whitespace would already have ended the word at the
    // lexer level. Only used where splitting actually applies (command
    // argv, `for` word-lists) -- assignment RHS, case words, redirect
    // targets, etc. still go through plain expand_word.
    // Returns (fields, patterns): `fields` is the word-split display text,
    // exactly as before; `patterns` is an index-aligned second copy of the
    // same fields but with every *quoted* chunk's contribution escaped via
    // glob::escape (unquoted chunks -- including their expansion results,
    // e.g. an unquoted `$var` whose value happens to contain `*` -- stay
    // raw glob syntax). expand_words glob-expands each field against its
    // paired pattern, falling back to the literal field text when the
    // pattern has no metachars or no filesystem matches. Building both in
    // one pass (rather than a second, expand_regex_operand-style walk)
    // matters here specifically because this function's expansions can
    // have side effects (command substitution, ${x:=default}, `$((x++))`)
    // that must not run twice.
    fn expand_word_split(&mut self, w: &Word) -> (Vec<String>, Vec<String>) {
        let ifs = self.get_ifs();
        let mut fields: Vec<String> = Vec::new();
        let mut current: Option<String> = None;
        let mut patterns: Vec<String> = Vec::new();
        let mut pattern_current: Option<String> = None;
        for c in &w.chunks {
            match c {
                Chunk::Str(t) => {
                    current.get_or_insert_with(String::new).push_str(t);
                    pattern_current.get_or_insert_with(String::new).push_str(t);
                }
                Chunk::LiteralStr(t) => {
                    // Quoted or backslash-escaped source text -- always
                    // escaped for the pattern copy, whatever characters it
                    // contains, so it can never itself act as a wildcard.
                    current.get_or_insert_with(String::new).push_str(t);
                    pattern_current.get_or_insert_with(String::new).push_str(&crate::glob::escape(t));
                }
                Chunk::Var { name, quoted } => {
                    // "$@" is a special case even when quoted: it expands
                    // to one field per positional parameter (as if each
                    // were individually double-quoted), not one joined
                    // string -- unlike "$*", which does join. Unquoted $@
                    // falls through to the normal joined-then-split path,
                    // matching bash (both $@ and $* behave the same
                    // unquoted).
                    if name == "@" && *quoted {
                        let parts = self.arg_frames.last().cloned().unwrap_or_default();
                        append_parts_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &parts);
                    } else {
                        let name = name.clone();
                        self.check_nounset(&name);
                        let v = self.lookup_var(&name);
                        append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                    }
                }
                Chunk::Sub { raw, quoted } => {
                    let v = self.run_command_substitution(raw);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::Arith { raw, quoted } => {
                    let v = match arith::eval(raw, self) {
                        Ok(n) => n.to_string(),
                        Err(e) => {
                            eprintln!("bish: (({})): {}", raw, e);
                            String::new()
                        }
                    };
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::VarExpand { name, op, quoted } => {
                    let name = name.clone();
                    let op = op.clone();
                    let v = self.eval_var_op(&name, &op);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::ArrayVar { name, index, quoted } => {
                    // "${arr[@]}" is the array analog of "$@": one field per
                    // element even though it's quoted. "${arr[*]}" (quoted
                    // or not) and unquoted "${arr[@]}" join with a space
                    // first, like $*.
                    if index == "@" && *quoted {
                        let items = self.array_all(name);
                        append_parts_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &items);
                    } else if index == "@" || index == "*" {
                        let joined = self.array_all(name).join(" ");
                        append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &joined, *quoted, &ifs);
                    } else {
                        let name = name.clone();
                        let index = index.clone();
                        let v = self.array_element(&name, &index);
                        append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                    }
                }
                Chunk::ArrayLength { name, index } => {
                    let name = name.clone();
                    let index = index.clone();
                    let v = self.array_length(&name, &index).to_string();
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, true, &ifs);
                }
                Chunk::ArrayVarExpand { name, index, op, quoted } => {
                    let name = name.clone();
                    let index = index.clone();
                    let op = op.clone();
                    let v = self.eval_array_var_op(&name, &index, &op);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::Indirect { name, quoted } => {
                    let target = self.lookup_var(name);
                    let v = self.lookup_var(&target);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, *quoted, &ifs);
                }
                Chunk::ArrayKeys { name, quoted } => {
                    // Same @-vs-* / quoted-vs-not splitting rules as
                    // ${arr[@]}: "@" quoted is one field per key.
                    if *quoted {
                        let items = self.array_keys(name);
                        append_parts_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &items);
                    } else {
                        let joined = self.array_keys(name).join(" ");
                        append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &joined, *quoted, &ifs);
                    }
                }
                Chunk::ProcSubIn { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_in(&raw);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, true, &ifs);
                }
                Chunk::ProcSubOut { raw } => {
                    let raw = raw.clone();
                    let v = self.run_proc_sub_out(&raw);
                    append_splittable_glob(&mut fields, &mut current, &mut patterns, &mut pattern_current, &v, true, &ifs);
                }
            }
        }
        if let Some(c) = current {
            fields.push(c);
        }
        if let Some(c) = pattern_current {
            patterns.push(c);
        }
        (fields, patterns)
    }

    // The default IFS (" \t\n") when the variable is truly unset; its
    // actual value (which may be empty, meaning "don't split at all")
    // whenever it's been assigned, even to "".
    fn get_ifs(&mut self) -> String {
        if self.var_is_set("IFS") {
            self.lookup_var("IFS")
        } else {
            " \t\n".to_string()
        }
    }

    // Re-lexes and expands a captured raw operand (the "word"/"pattern"
    // half of a ${...} expansion), so it can itself contain further $
    // expansions. Multiple resulting words are concatenated with no
    // separator, approximating "the operand as a single expandable value".
    fn expand_raw(&mut self, raw: &str) -> String {
        match crate::lexer::Lexer::new(raw).tokenize() {
            Ok(toks) => {
                let mut s = String::new();
                for t in toks {
                    if let crate::lexer::Tok::Word(chunks, _) = t {
                        s.push_str(&self.expand_word(&Word { chunks, globbable: false }));
                    }
                }
                s
            }
            Err(_) => raw.to_string(),
        }
    }

    fn eval_var_op(&mut self, name: &str, op: &VarOp) -> String {
        let cur = self.lookup_var(name);
        match op {
            VarOp::Length => cur.chars().count().to_string(),
            VarOp::Default { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.var_is_set(name) };
                if trigger {
                    self.expand_raw(word)
                } else {
                    cur
                }
            }
            VarOp::AssignDefault { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.var_is_set(name) };
                if trigger {
                    let v = self.expand_raw(word);
                    self.assign_var(name, v.clone());
                    v
                } else {
                    cur
                }
            }
            VarOp::ErrorIfUnset { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.var_is_set(name) };
                if trigger {
                    let msg = self.expand_raw(word);
                    eprintln!("bish: {}: {}", name, msg);
                    String::new()
                } else {
                    cur
                }
            }
            VarOp::AltIfSet { word, colon } => {
                let set_enough = if *colon { !cur.is_empty() } else { self.var_is_set(name) };
                if set_enough {
                    self.expand_raw(word)
                } else {
                    String::new()
                }
            }
            VarOp::RemovePrefix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_prefix_glob(&cur, &pattern, *longest)
            }
            VarOp::RemoveSuffix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_suffix_glob(&cur, &pattern, *longest)
            }
            VarOp::CaseConvert { pattern, upper, all } => {
                let pattern = self.expand_raw(pattern);
                apply_case_convert(&cur, &pattern, *upper, *all)
            }
            VarOp::Substring { offset, length } => {
                let off = arith::eval(offset, self).unwrap_or(0);
                let len = length.as_ref().and_then(|l| arith::eval(l, self).ok());
                substring_expand(&cur, off, len)
            }
            VarOp::Replace { pattern, repl, global, anchor } => {
                let pattern = self.expand_raw(pattern);
                let repl = self.expand_raw(repl);
                glob_replace(&cur, &pattern, &repl, *global, *anchor)
            }
        }
    }

    // Same operators as eval_var_op, but reading (and, for :=, writing)
    // one array element instead of a scalar variable. "@"/"*" indices are
    // treated as the joined-all-elements string, matching how they behave
    // as a plain (non-splitting-aware) expansion elsewhere.
    fn eval_array_var_op(&mut self, name: &str, index: &str, op: &VarOp) -> String {
        let cur = self.array_element(name, index);
        match op {
            VarOp::Length => cur.chars().count().to_string(),
            VarOp::Default { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.array_element_is_set(name, index) };
                if trigger {
                    self.expand_raw(word)
                } else {
                    cur
                }
            }
            VarOp::AssignDefault { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.array_element_is_set(name, index) };
                if trigger {
                    let v = self.expand_raw(word);
                    if index != "@" && index != "*" {
                        self.array_set_index(name, index, v.clone());
                    }
                    v
                } else {
                    cur
                }
            }
            VarOp::ErrorIfUnset { word, colon } => {
                let trigger = if *colon { cur.is_empty() } else { !self.array_element_is_set(name, index) };
                if trigger {
                    let msg = self.expand_raw(word);
                    eprintln!("bish: {}[{}]: {}", name, index, msg);
                    String::new()
                } else {
                    cur
                }
            }
            VarOp::AltIfSet { word, colon } => {
                let set_enough = if *colon { !cur.is_empty() } else { self.array_element_is_set(name, index) };
                if set_enough {
                    self.expand_raw(word)
                } else {
                    String::new()
                }
            }
            VarOp::RemovePrefix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_prefix_glob(&cur, &pattern, *longest)
            }
            VarOp::RemoveSuffix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_suffix_glob(&cur, &pattern, *longest)
            }
            VarOp::CaseConvert { pattern, upper, all } => {
                let pattern = self.expand_raw(pattern);
                apply_case_convert(&cur, &pattern, *upper, *all)
            }
            VarOp::Substring { offset, length } => {
                let off = arith::eval(offset, self).unwrap_or(0);
                let len = length.as_ref().and_then(|l| arith::eval(l, self).ok());
                substring_expand(&cur, off, len)
            }
            VarOp::Replace { pattern, repl, global, anchor } => {
                let pattern = self.expand_raw(pattern);
                let repl = self.expand_raw(repl);
                glob_replace(&cur, &pattern, &repl, *global, *anchor)
            }
        }
    }

    // Expands a simple-command's words into argv, applying filesystem
    // pathname (glob) expansion to any word that's both glob-eligible
    // (no quoting/escaping/expansion at all -- see Word::globbable) and
    // actually contains metacharacters. A pattern with no filesystem
    // matches is kept as its literal text, matching bash's default
    // (nullglob-off) behavior.
    fn expand_words(&mut self, words: &[Word]) -> Vec<String> {
        let mut out = Vec::new();
        for w in words {
            if w.globbable {
                // globbable implies no quoting/expansion at all in the word
                // (see Word::globbable), so splitting can't apply here --
                // glob-check the single literal value as before.
                let s = self.expand_word(w);
                if !self.opt_noglob {
                    if let Some(matches) = glob::expand(&s) {
                        out.extend(matches);
                        continue;
                    }
                }
                out.push(s);
            } else {
                let (fields, patterns) = self.expand_word_split(w);
                if self.opt_noglob {
                    out.extend(fields);
                } else {
                    for (f, p) in fields.into_iter().zip(patterns.into_iter()) {
                        match glob::expand(&p) {
                            Some(matches) => out.extend(matches),
                            None => out.push(f),
                        }
                    }
                }
            }
        }
        out
    }

    // Reads a name's own raw stored value, bypassing nameref redirection --
    // used to read a nameref's target-name string, and internally by
    // resolve_nameref while following a chain.
    fn raw_var_lookup(&self, name: &str) -> String {
        for scope in self.var_scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return v.clone();
            }
        }
        std::env::var(name).unwrap_or_default()
    }

    // Writes a name's own raw value, bypassing nameref redirection -- used
    // to set a nameref's target-name string itself (assign_var, by
    // contrast, is what a nameref's *reads/writes* get redirected through).
    fn raw_var_write(&mut self, name: &str, value: String) {
        for scope in self.var_scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), value);
                return;
            }
        }
        unsafe {
            std::env::set_var(name, value);
        }
    }

    // Follows a `declare -n`/`local -n` chain to the final target name,
    // capped to guard against a self-referential or circular nameref (bash
    // detects and errors on these; bish just stops following rather than
    // looping forever).
    fn resolve_nameref(&self, name: &str) -> String {
        let mut current = name.to_string();
        let mut hops = 0;
        while self.nameref_names.contains(&current) && hops < 16 {
            let target = self.raw_var_lookup(&current);
            if target.is_empty() || target == current {
                break;
            }
            current = target;
            hops += 1;
        }
        current
    }

    fn lookup_var(&mut self, name: &str) -> String {
        if self.nameref_names.contains(name) {
            let target = self.resolve_nameref(name);
            if target != name {
                return self.lookup_var(&target);
            }
        }
        match name {
            "?" => self.last_status.to_string(),
            "0" => self.script_name.clone(),
            "#" => self.arg_frames.last().map(|a| a.len()).unwrap_or(0).to_string(),
            "@" | "*" => self.arg_frames.last().map(|a| a.join(" ")).unwrap_or_default(),
            // $$/$!/$RANDOM/$SECONDS/$- are always computed live, never
            // read back from var_scopes/env, matching how bash treats them
            // as effectively magic rather than ordinary settable
            // variables (SECONDS is the one partial exception -- see
            // assign_var's special-case for `SECONDS=n`).
            "$" => std::process::id().to_string(),
            "!" => self.last_bg_pid.map(|p| p.to_string()).unwrap_or_default(),
            "RANDOM" => self.next_random().to_string(),
            "SECONDS" => (self.shell_start.elapsed().as_secs() as i64 + self.seconds_offset).to_string(),
            "-" => {
                let mut s = String::new();
                if self.opt_errexit {
                    s.push('e');
                }
                if self.opt_noglob {
                    s.push('f');
                }
                if self.opt_nounset {
                    s.push('u');
                }
                if self.opt_xtrace {
                    s.push('x');
                }
                s
            }
            _ if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) => {
                let idx: usize = name.parse().unwrap_or(0);
                idx.checked_sub(1)
                    .and_then(|i| self.arg_frames.last().and_then(|a| a.get(i)))
                    .cloned()
                    .unwrap_or_default()
            }
            _ => {
                for scope in self.var_scopes.iter().rev() {
                    if let Some(v) = scope.get(name) {
                        return v.clone();
                    }
                }
                if let Ok(v) = std::env::var(name) {
                    return v;
                }
                // Startup-populated-in-real-bash variables: computed on
                // demand here instead, but still overridable by a normal
                // assignment (checked above) since that's the common bash
                // behavior for all of these once actually set.
                match name {
                    "BASH_VERSION" => "5.2.21(1)-release".to_string(),
                    "PPID" => unsafe { getppid() }.to_string(),
                    "UID" => unsafe { getuid() }.to_string(),
                    "EUID" => unsafe { geteuid() }.to_string(),
                    "HOSTNAME" => get_hostname(),
                    _ => String::new(),
                }
            }
        }
    }

    // `set -u`: only a *bare* $VAR/${VAR} reference to a truly-unset name
    // triggers this -- ${VAR:-default}/${VAR-default}/${VAR?msg} etc are
    // explicitly exempt in bash (checking for unset is their whole point),
    // so this is only called from the plain Chunk::Var expansion sites, not
    // from eval_var_op/eval_array_var_op.
    // xorshift64* -- simple, fast, no external RNG crate. Bash's $RANDOM
    // range is 0..32767.
    // `command -v`/`-V name`: reports what `name` resolves to (function,
    // builtin, or a PATH-resolved executable) without running it. Prints
    // nothing and returns 1 if it doesn't resolve to anything, matching
    // bash -- this is what makes `command -v foo >/dev/null` work as an
    // existence check.
    fn command_v(&mut self, name: &str, verbose: bool) -> i32 {
        if self.functions.contains_key(name) {
            println!("{}", if verbose { format!("{} is a function", name) } else { name.to_string() });
            return 0;
        }
        if is_known_builtin(name) {
            println!("{}", if verbose { format!("{} is a shell builtin", name) } else { name.to_string() });
            return 0;
        }
        match resolve_in_path(name) {
            Some(p) => {
                println!("{}", if verbose { format!("{} is {}", name, p) } else { p });
                0
            }
            None => 1,
        }
    }

    // type [-p] name... A scoped subset of real bash's `type`: reports
    // function/builtin/PATH-resolved-executable, or "not found" (status
    // 1). `-a`/`-t` are accepted but not distinguished from the default.
    fn run_type(&mut self, args: &[String]) -> i32 {
        let mut path_only = false;
        let mut names: Vec<&String> = Vec::new();
        for a in args {
            match a.as_str() {
                "-p" | "-P" => path_only = true,
                "-a" | "-t" => {}
                _ => names.push(a),
            }
        }
        let mut status = 0;
        for name in names {
            if self.functions.contains_key(name.as_str()) {
                if !path_only {
                    println!("{} is a function", name);
                }
                continue;
            }
            if is_known_builtin(name) {
                if !path_only {
                    println!("{} is a shell builtin", name);
                }
                continue;
            }
            match resolve_in_path(name) {
                Some(p) => println!("{}", if path_only { p } else { format!("{} is {}", name, p) }),
                None => {
                    eprintln!("bish: type: {}: not found", name);
                    status = 1;
                }
            }
        }
        status
    }

    fn next_random(&mut self) -> u32 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        ((x >> 33) % 32768) as u32
    }

    fn check_nounset(&mut self, name: &str) {
        if !self.opt_nounset {
            return;
        }
        if self.var_is_set(name) {
            return;
        }
        eprintln!("bish: {}: unbound variable", name);
        self.run_exit_trap();
        std::process::exit(1);
    }

    // Whether `name` is a variable that's actually been assigned, as
    // opposed to merely evaluating to an empty string -- the distinction
    // `${V-x}`/`${V+x}` (unset only) need vs. `${V:-x}`/`${V:+x}` (unset OR
    // empty). Special/positional parameters always count as set.
    fn var_is_set(&self, name: &str) -> bool {
        let resolved;
        let name = if self.nameref_names.contains(name) {
            resolved = self.resolve_nameref(name);
            resolved.as_str()
        } else {
            name
        };
        // Special/positional parameters and the magic variables
        // lookup_var computes on demand (RANDOM, SECONDS, PPID, ...) are
        // always considered set, even under `set -u` -- matching bash,
        // which never treats these as unbound.
        let is_special = matches!(
            name,
            "?" | "0"
                | "#"
                | "@"
                | "*"
                | "$"
                | "!"
                | "-"
                | "RANDOM"
                | "SECONDS"
                | "BASH_VERSION"
                | "PPID"
                | "UID"
                | "EUID"
                | "HOSTNAME"
        ) || (!name.is_empty() && name.chars().all(|c| c.is_ascii_digit()));
        if is_special {
            return true;
        }
        for scope in &self.var_scopes {
            if scope.contains_key(name) {
                return true;
            }
        }
        std::env::var(name).is_ok()
    }

    // Plain assignment targets the global (process-env) variable, unless it
    // shadows an existing `local` of the same name in the current function
    // scope -- matching bash, where functions don't auto-localize vars.
    fn assign_var(&mut self, name: &str, value: String) {
        let resolved;
        let name = if self.nameref_names.contains(name) {
            resolved = self.resolve_nameref(name);
            resolved.as_str()
        } else {
            name
        };
        if self.readonly_names.contains(name) {
            eprintln!("bish: {}: readonly variable", name);
            return;
        }
        // `SECONDS=n` resets the elapsed-time counter to start counting
        // from n, matching bash -- lookup_var computes it live rather
        // than storing it, so the assignment records an offset instead of
        // writing a var_scopes/env entry.
        if name == "SECONDS" {
            if let Ok(n) = value.trim().parse::<i64>() {
                self.seconds_offset = n - self.shell_start.elapsed().as_secs() as i64;
            }
            return;
        }
        // `RANDOM=n` reseeds the generator, matching bash (rather than
        // making $RANDOM a static value forever).
        if name == "RANDOM" {
            if let Ok(n) = value.trim().parse::<u64>() {
                self.rng_state = if n == 0 { 0x2545F4914F6CDD1D } else { n };
            }
            return;
        }
        // `declare -i`/`local -i`: the assigned text is evaluated as an
        // arithmetic expression rather than stored literally (bash: `n="2+3"`
        // on an integer-attribute variable stores 5, not the string "2+3").
        let value =
            if self.integer_names.contains(name) { arith::eval(&value, self).unwrap_or(0).to_string() } else { value };
        // `declare -u`/`-l`: case-fold on every assignment.
        let value = if self.upper_names.contains(name) {
            value.to_uppercase()
        } else if self.lower_names.contains(name) {
            value.to_lowercase()
        } else {
            value
        };
        if self.exported_names.contains(name) {
            unsafe {
                std::env::set_var(name, &value);
            }
        }
        self.raw_var_write(name, value);
    }

    fn resolve_redirects(&mut self, cmd: &SimpleCommand) -> Result<ResolvedRedirs, String> {
        self.resolve_redirect_list(&cmd.redirects)
    }

    // Diagnostics for a command that never got to inherit its own stdio
    // (e.g. spawn() failing with "not found") would otherwise bypass a
    // `2>` redirect entirely -- real bash routes them through it too. Falls
    // back to the shell's real stderr when there's no stderr redirect.
    fn write_command_error(&mut self, cmd: &SimpleCommand, msg: &str) {
        let target = self.peek_stderr_target(&cmd.redirects);
        write_diagnostic(&target, msg);
    }

    fn peek_stderr_target(&mut self, redirects: &[Redirect]) -> Option<String> {
        let mut target: Option<String> = None;
        for r in redirects {
            match r {
                Redirect::Err { word, .. } | Redirect::Both { word, .. } => {
                    target = Some(self.expand_word(word));
                }
                Redirect::DupErrToOut => {
                    for r2 in redirects {
                        if let Redirect::Out { word, .. } = r2 {
                            target = Some(self.expand_word(word));
                        }
                    }
                }
                _ => {}
            }
        }
        target
    }

    // The plain-fd-0/1/2 half of `exec`'s redirect-only form (`exec >
    // file`, `exec 2>> file`, `exec < file`, `exec &> file`) -- a
    // dedicated resolver rather than reusing resolve_redirect_list's
    // Option<Stdio> output because Stdio doesn't expose its underlying fd
    // for this function's caller (apply_fds_to_self) to dup2 onto 0/1/2 of
    // *this* process; it needs the raw File instead. Mirrors
    // resolve_redirect_list's In/Out/Err/Both handling (last-one-wins per
    // fd, matching real bash), skipping the numbered-fd/DupErrToOut forms
    // since those are already covered by resolve_redirects' own
    // extra_fds/dup_stderr_to_stdout, computed separately by the caller
    // from the same redirect list.
    fn resolve_plain_fd012(
        &mut self,
        redirects: &[Redirect],
    ) -> Result<(Option<std::fs::File>, Option<std::fs::File>, Option<std::fs::File>), String> {
        let mut stdin_path: Option<String> = None;
        let mut here_string: Option<String> = None;
        let mut stdout_target: Option<(String, bool)> = None;
        let mut stderr_target: Option<(String, bool)> = None;
        for r in redirects {
            match r {
                Redirect::In(w) => {
                    stdin_path = Some(self.expand_word(w));
                    here_string = None;
                }
                Redirect::HereString(w) => {
                    let mut content = self.expand_word(w);
                    content.push('\n');
                    here_string = Some(content);
                    stdin_path = None;
                }
                Redirect::HereDoc(w) => {
                    here_string = Some(self.expand_word(w));
                    stdin_path = None;
                }
                Redirect::Out { word, append } => {
                    stdout_target = Some((self.expand_word(word), *append));
                }
                Redirect::Err { word, append } => {
                    stderr_target = Some((self.expand_word(word), *append));
                }
                Redirect::Both { word, append } => {
                    let p = self.expand_word(word);
                    stdout_target = Some((p.clone(), *append));
                    stderr_target = Some((p, *append));
                }
                _ => {}
            }
        }
        let stdin = if let Some(content) = here_string {
            Some(here_string_file(&content)?)
        } else {
            match stdin_path {
                Some(p) => Some(std::fs::File::open(&p).map_err(|e| format!("{}: {}", p, e))?),
                None => None,
            }
        };
        let stdout = match stdout_target {
            Some((p, append)) => Some(open_out(&p, append)?),
            None => None,
        };
        let stderr = match stderr_target {
            Some((p, append)) => Some(open_out(&p, append)?),
            None => None,
        };
        Ok((stdin, stdout, stderr))
    }

    fn resolve_redirect_list(&mut self, redirects: &[Redirect]) -> Result<ResolvedRedirs, String> {
        let mut stdout_target: Option<(String, bool)> = None;
        let mut stderr_target: Option<(String, bool)> = None;
        let mut stdin_path: Option<String> = None;
        let mut here_string: Option<String> = None;
        let mut dup_err_to_out = false;
        let mut extra_fds: Vec<ExtraFd> = Vec::new();

        for r in redirects {
            match r {
                Redirect::In(w) => {
                    stdin_path = Some(self.expand_word(w));
                    here_string = None;
                }
                Redirect::HereString(w) => {
                    let mut content = self.expand_word(w);
                    content.push('\n');
                    here_string = Some(content);
                    stdin_path = None;
                }
                Redirect::HereDoc(w) => {
                    // Body already ends in '\n' from capture_heredoc_body;
                    // reuses the same temp-file Stdio plumbing as <<<.
                    here_string = Some(self.expand_word(w));
                    stdin_path = None;
                }
                Redirect::Out { word, append } => {
                    stdout_target = Some((self.expand_word(word), *append));
                    dup_err_to_out = false;
                }
                Redirect::Err { word, append } => {
                    stderr_target = Some((self.expand_word(word), *append));
                    dup_err_to_out = false;
                }
                Redirect::Both { word, append } => {
                    let p = self.expand_word(word);
                    stdout_target = Some((p, *append));
                    dup_err_to_out = true;
                }
                Redirect::DupErrToOut => dup_err_to_out = true,
                Redirect::FdOut { fd, word, append } => {
                    let p = self.expand_word(word);
                    let file = open_out(&p, *append)?;
                    extra_fds.push(ExtraFd::Open { fd: *fd as i32, file });
                }
                Redirect::FdIn { fd, word } => {
                    let p = self.expand_word(word);
                    let file = std::fs::File::open(&p).map_err(|e| format!("{}: {}", p, e))?;
                    extra_fds.push(ExtraFd::Open { fd: *fd as i32, file });
                }
                Redirect::FdDup { fd, target } => {
                    extra_fds.push(ExtraFd::Dup { fd: *fd as i32, source: *target as i32 });
                }
                Redirect::FdDupWord { fd, word } => {
                    let target_str = self.expand_word(word);
                    match target_str.trim().parse::<i32>() {
                        Ok(source) => extra_fds.push(ExtraFd::Dup { fd: *fd as i32, source }),
                        Err(_) => return Err(format!("{}: ambiguous redirect", target_str)),
                    }
                }
                Redirect::FdClose { fd } => {
                    extra_fds.push(ExtraFd::Close(*fd as i32));
                }
            }
        }

        let stdin = if let Some(content) = here_string {
            Some(Stdio::from(here_string_file(&content)?))
        } else {
            match stdin_path {
                Some(p) => Some(Stdio::from(
                    std::fs::File::open(&p).map_err(|e| format!("{}: {}", p, e))?,
                )),
                None => None,
            }
        };
        let stdout_file: Option<std::fs::File> = match &stdout_target {
            Some((p, append)) => Some(open_out(p, *append)?),
            None => None,
        };
        // `2>&1`'s actual fd-dup happens via dup2_stderr_to_stdout at each
        // spawn site instead of resolving a Stdio here -- see
        // ResolvedRedirs::dup_stderr_to_stdout for why (a pipe destination
        // can't be "opened" the way a file can).
        let stderr_file: Option<std::fs::File> = if dup_err_to_out {
            None
        } else {
            match &stderr_target {
                Some((p, append)) => Some(open_out(p, *append)?),
                None => None,
            }
        };
        let stdout = stdout_file.map(Stdio::from);
        let stderr = stderr_file.map(Stdio::from);

        Ok(ResolvedRedirs { stdin, stdout, stderr, dup_stderr_to_stdout: dup_err_to_out, extra_fds })
    }
}

impl arith::VarContext for Shell {
    fn get(&mut self, name: &str) -> i64 {
        self.lookup_var(name).trim().parse().unwrap_or(0)
    }

    fn set(&mut self, name: &str, value: i64) {
        self.assign_var(name, value.to_string());
    }
}

struct ResolvedRedirs {
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
    // True fd-dup of whatever stdout ends up being -- a pipe, a file, or
    // inherited -- applied via a dup2 pre_exec hook at each spawn site
    // (see dup2_stderr_to_stdout). Stdio has no "duplicate of a sibling
    // fd" variant, so this is the only way `2>&1` can share stdout's
    // *actual* destination rather than a second, independently-opened
    // handle to it; it's what makes `cmd 2>&1 | other` correctly merge
    // stderr into the pipe, not just the `cmd > file 2>&1` shape.
    dup_stderr_to_stdout: bool,
    // Arbitrary-fd redirects (`3>file`, `4<&0`, ...), applied via the same
    // dup2-in-pre_exec approach as dup_stderr_to_stdout, in source order,
    // *after* dup_stderr_to_stdout -- see apply_fd_redirects. Scoped gap:
    // since these run after stdin/stdout/stderr's own Command-builder setup
    // rather than interleaved with it, a numbered-fd redirect that's meant
    // to capture fd 0/1/2's state *before* a later plain `>`/`<` redirect
    // in the same command changes it (the classic `3>&1 1>log 2>&3`
    // fd-juggling idiom) won't see the pre-redirect value. Every ordering
    // that doesn't depend on that interleaving works correctly.
    extra_fds: Vec<ExtraFd>,
}

enum ExtraFd {
    Open { fd: i32, file: std::fs::File },
    Dup { fd: i32, source: i32 },
    Close(i32),
}

// Associative-array storage (`declare -A`). Iterates in insertion order --
// confirmed via a clean probe against real bash that its own iteration
// order is neither insertion order nor alphabetical, but its internal
// hash-table bucket order (stable across updates/delete-then-reinsert on a
// given key, but not derivable without literally reimplementing bash's
// specific hash function and bucket-growth behavior). Insertion order is
// the more useful, predictable choice for a new implementation even though
// it won't byte-match bash's own output for a script that happens to
// depend on the exact order -- a genuinely rare thing to depend on, since
// bash's own order isn't something a script author could reliably predict
// either.
#[derive(Default)]
struct OrderedMap {
    order: Vec<String>,
    values: std::collections::HashMap<String, String>,
}

impl OrderedMap {
    fn get(&self, key: &str) -> Option<&String> {
        self.values.get(key)
    }

    fn contains_key(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    fn insert(&mut self, key: String, value: String) {
        if !self.values.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.values.insert(key, value);
    }

    fn remove(&mut self, key: &str) -> Option<String> {
        let v = self.values.remove(key);
        if v.is_some() {
            self.order.retain(|k| k != key);
        }
        v
    }

    fn keys(&self) -> impl Iterator<Item = &String> {
        self.order.iter()
    }

    fn values(&self) -> impl Iterator<Item = &String> {
        self.order.iter().map(move |k| self.values.get(k).unwrap())
    }

    fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.order.iter().map(move |k| (k, self.values.get(k).unwrap()))
    }

    fn len(&self) -> usize {
        self.order.len()
    }
}

// A background job (`cmd &`), one or more children (a whole pipeline when
// backgrounded together). `children` are kept so `wait`/`jobs`/`fg` can
// A coproc pipe half the shell keeps open. The read side is wrapped in a
// persistent BufReader (not a fresh one per `read -u` call) for the same
// reason read_input_source avoids `BufReader::new(stdin())`: a throwaway
// wrapper's internal read-ahead buffer would silently discard whatever it
// over-read past the first line every time it's dropped.
enum KeptFd {
    Read(std::io::BufReader<std::io::PipeReader>),
    Write(std::io::PipeWriter),
}

impl std::os::fd::AsRawFd for KeptFd {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            KeptFd::Read(r) => r.get_ref().as_raw_fd(),
            KeptFd::Write(w) => w.as_raw_fd(),
        }
    }
}

// poll/block on them directly -- earlier this shell just spawned a thread
// that blindly `.wait()`d and dropped the result, which meant nothing else
// could ever observe a background job's completion or exit status.
struct Job {
    id: u32,
    pids: Vec<u32>,
    children: Vec<std::process::Child>,
    cmd_text: String,
}

impl Job {
    // Non-blocking: reaps any children that have already exited without
    // blocking on ones that haven't. Returns the job's exit status once
    // *every* child has exited (bash: a job composed of a pipeline is
    // "done" when its last stage exits; this shell reports done once the
    // whole pipeline has, and uses the last child's code, matching the
    // pipeline exit-status convention used elsewhere in this file).
    fn poll(&mut self) -> Option<i32> {
        let mut all_done = true;
        let mut last_code = 0;
        for c in &mut self.children {
            match c.try_wait() {
                Ok(Some(status)) => last_code = exit_code_from_status(status),
                Ok(None) => all_done = false,
                Err(_) => last_code = 1,
            }
        }
        if all_done { Some(last_code) } else { None }
    }

    // Blocking wait for every child in the job.
    fn wait(&mut self) -> i32 {
        let mut last_code = 0;
        for c in &mut self.children {
            match c.wait() {
                Ok(status) => last_code = exit_code_from_status(status),
                Err(_) => last_code = 1,
            }
        }
        last_code
    }
}

// bash's exit-status convention for a process killed by a signal is
// 128+signum (ExitStatus::code() returns None in that case -- there's no
// normal exit code to report -- so this falls back to the signal via
// ExitStatusExt, matching what `$?`/`wait`/`fg` should actually show).
fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| status.signal().map(|s| 128 + s).unwrap_or(1))
}

// Shared by the `read` builtin's two sources (a Box<dyn BufRead> from
// read_input_source, or a borrowed coproc fd via `-u`) -- factored out
// specifically so the `-u` path's borrow of self.coproc_fds can end right
// after this call, before the caller needs `&mut self` again for
// assign_var. Real bash: a line/char-count read that runs into EOF before
// seeing its delimiter (or before nchars is reached) still populates the
// variable(s) with whatever partial data it got, but returns non-zero --
// `clean` (the bool) tracks which case this was.
fn read_line_or_chars(reader: &mut dyn std::io::BufRead, nchars: Option<usize>, delim: u8) -> (Option<String>, bool) {
    if let Some(n) = nchars {
        let mut buf = Vec::with_capacity(n);
        let mut hit_eof = false;
        for _ in 0..n {
            let mut b = [0u8; 1];
            match std::io::Read::read(reader, &mut b) {
                Ok(0) => {
                    hit_eof = true;
                    break;
                }
                Ok(_) => buf.push(b[0]),
                Err(_) => {
                    hit_eof = true;
                    break;
                }
            }
        }
        if buf.is_empty() && hit_eof { (None, false) } else { (Some(String::from_utf8_lossy(&buf).into_owned()), !hit_eof) }
    } else {
        let mut buf: Vec<u8> = Vec::new();
        match reader.read_until(delim, &mut buf) {
            Ok(0) => (None, false),
            Ok(_) => {
                let hit_delim = buf.last() == Some(&delim);
                if hit_delim {
                    buf.pop();
                    if delim == b'\n' && buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                }
                (Some(String::from_utf8_lossy(&buf).into_owned()), hit_delim)
            }
            Err(_) => (None, false),
        }
    }
}

#[derive(Clone)]
enum TrapAction {
    Ignore,
    Run(String),
}

// dup2(1, 2) in the child, after fork but before exec -- so fd 2 becomes a
// genuine duplicate of whatever fd 1 was just set up to be (Command's own
// stdio setup runs before pre_exec hooks). A single async-signal-safe
// syscall with no allocation, which is exactly what's safe to do in this
// narrow post-fork window; declared directly via extern "C" rather than
// pulling in the `libc` crate, since libc is already linked into any
// dynamically-linked Unix binary regardless.
// $PPID/$UID/$EUID: raw libc calls declared directly via extern "C" (same
// justification as dup2_stderr_to_stdout -- libc is already linked into
// any dynamically-linked Unix binary, no external crate needed) since std
// has no portable getppid/getuid/geteuid wrapper.
unsafe extern "C" {
    fn getppid() -> i32;
    fn getuid() -> u32;
    fn geteuid() -> u32;
}

// Signal traps (`trap CMD SIGNAL`). A signal handler can only safely do
// async-signal-safe work -- no allocation, no locks, nothing that could
// reenter a libc function the interrupted code was mid-call in -- so the
// handler itself does nothing but flag the signal number in this bitmask;
// the shell's own run_program loop checks it between top-level statements
// (see check_pending_signals) and runs the actual trap code there, in a
// normal, fully-safe context. One consequence: a trap won't fire truly
// asynchronously mid-syscall (e.g. while a script is blocked in `sleep` or
// `wait`) -- only at the next between-statements checkpoint. That's a
// real, deliberate scope boundary (not a bug): true async delivery mid-
// blocking-call needs EINTR-aware retry logic threaded through every
// blocking call site in this file, which is a much bigger change for a
// scripting-focused shell where "runs between statements" already covers
// the overwhelmingly common trap use (cleanup-on-signal, not low-latency
// signal response).
static PENDING_SIGNALS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

extern "C" fn record_pending_signal(sig: i32) {
    if (0..32).contains(&sig) {
        PENDING_SIGNALS.fetch_or(1 << sig, std::sync::atomic::Ordering::SeqCst);
    }
}

// Layout matches glibc's `struct sigaction` on Linux x86_64: handler
// pointer, then the 128-byte sigset_t (16 u64 words -- _NSIG/64 on this
// platform), then the int flags, then the restorer pointer (glibc's own
// sigaction() wrapper fills this in itself before the real syscall; it
// doesn't need to be set here).
#[repr(C)]
struct SigActionRaw {
    sa_handler: usize,
    sa_mask: [u64; 16],
    sa_flags: i32,
    sa_restorer: usize,
}

const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;

fn sigaction_raw(signum: i32, handler: usize) {
    unsafe extern "C" {
        fn sigaction(signum: i32, act: *const SigActionRaw, oldact: *mut SigActionRaw) -> i32;
    }
    let act = SigActionRaw { sa_handler: handler, sa_mask: [0; 16], sa_flags: 0, sa_restorer: 0 };
    unsafe {
        sigaction(signum, &act, std::ptr::null_mut());
    }
}

// Name <-> number for the signals scripts actually trap. KILL (9) and
// STOP (19) are intentionally absent -- neither can be caught or ignored,
// matching real bash's own refusal to let `trap` touch them.
const SIGNAL_NAMES: &[(&str, i32)] = &[
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("CHLD", 17),
    ("CONT", 18),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("URG", 23),
    ("XCPU", 24),
    ("XFSZ", 25),
    ("VTALRM", 26),
    ("PROF", 27),
    ("WINCH", 28),
    ("IO", 29),
    ("PWR", 30),
    ("SYS", 31),
];

// Accepts "INT", "SIGINT", or a bare number ("2"); "0"/"EXIT" is handled by
// the caller separately since it isn't a real signal.
fn signal_number(name: &str) -> Option<i32> {
    let bare = name.strip_prefix("SIG").unwrap_or(name);
    if let Some(&(_, n)) = SIGNAL_NAMES.iter().find(|(n, _)| *n == bare) {
        return Some(n);
    }
    bare.parse::<i32>().ok()
}

fn signal_name(num: i32) -> String {
    SIGNAL_NAMES.iter().find(|(_, n)| *n == num).map(|(name, _)| name.to_string()).unwrap_or_else(|| num.to_string())
}

fn send_signal(pid: u32, sig: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid as i32, sig) == 0 }
}

// $HOSTNAME: bash populates this at startup from uname(); Linux exposes
// the same value via this proc file, which avoids yet another raw
// syscall. Falls back to the HOSTNAME env var (some environments export
// it already) and then to empty.
fn get_hostname() -> String {
    if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        return s.trim_end().to_string();
    }
    std::env::var("HOSTNAME").unwrap_or_default()
}

// `read -p`'s prompt only displays when input is coming from a terminal
// (bash-documented behavior), and `read -t`'s timeout is only meaningfully
// pollable against a real fd (stdin), not a shell-internal here-doc/file
// Cursor -- both need to know if fd 0 is an actual tty/pollable descriptor.
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

fn stdin_is_tty() -> bool {
    unsafe extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(0) != 0 }
}

// Polls fd 0 for readability, waiting up to `timeout_ms` (0 = check without
// waiting). Used by `read -t` to implement its timeout without needing a
// second thread or signal handling.
fn stdin_ready(timeout_ms: i32) -> bool {
    const POLLIN: i16 = 0x0001;
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    }
    let mut pfd = PollFd { fd: 0, events: POLLIN, revents: 0 };
    let r = unsafe { poll(&mut pfd as *mut PollFd, 1, timeout_ms) };
    r > 0 && (pfd.revents & POLLIN) != 0
}

// Single pre_exec hook covering both dup_stderr_to_stdout and any
// numbered-fd redirects -- CommandExt::pre_exec isn't documented as safe to
// call more than once per Command, so every fd-dup need for a given spawn
// funnels through this one closure instead of stacking separate calls.
// Real bug, found by tracing a "3>file works for every fd except 3" report
// down to its root cause: `dup2(oldfd, newfd)` is a no-op per POSIX when
// oldfd == newfd -- it does *not* clear FD_CLOEXEC on that descriptor. Rust
// opens files (and pipes, e.g. for coproc) with O_CLOEXEC by default, so
// whenever the file/pipe's own already-assigned fd number happens to
// coincide with the redirect's target fd (a real possibility: both are
// small numbers chosen by "next available", so collisions aren't rare),
// dup2 silently no-ops and the fd -- despite looking open right up to the
// exec() call -- gets closed by the kernel as part of exec's own cloexec
// handling, instead of surviving into the child like every other target
// fd does. Explicitly clearing FD_CLOEXEC after every dup2 (redundant for
// the normal src != dst case, since dup2 already clears it there) makes
// this correct regardless of whether the dup2 was a genuine duplicate or
// this same-fd no-op.
// Applies numbered-fd redirects (dup2/close) directly to *this* process,
// for `exec`'s redirect-only form (`exec 3>file`, `exec 3>&1`, `exec
// 3>&-`) -- unlike every other spawn site in this file, there's no pre_exec
// hook here since nothing is being forked; this just runs the same dup2/
// close logic inline, before continuing (bare exec) or before calling
// CommandExt::exec (exec CMD). Scoped to the numbered-fd forms specifically
// (extra_fds/dup_stderr_to_stdout) -- plain `exec > file` (implicit fd
// 0/1/2) goes through a completely different Option<Stdio>-based resolution
// path elsewhere in this file that isn't wired up to apply to the current
// process, a separate, narrower remaining gap.
fn apply_fds_to_self(dup_stderr_to_stdout: bool, extra_fds: Vec<ExtraFd>) -> Result<(), String> {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    if dup_stderr_to_stdout {
        if unsafe { dup2(1, 2) } == -1 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        clear_cloexec(2);
    }
    for ef in extra_fds {
        match ef {
            ExtraFd::Open { fd, file } => {
                let srcfd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
                if unsafe { dup2(srcfd, fd) } == -1 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                clear_cloexec(fd);
                // Unlike apply_fd_redirects' pre_exec path (where the
                // process image gets replaced by execve right after,
                // bypassing Rust-level Drop entirely, .exec() failure
                // notwithstanding -- and even then via _exit, not normal
                // unwinding), this function returns normally and lets its
                // locals drop like any other Rust code. If `file`'s own fd
                // happened to already equal the target (a real
                // possibility, same as the pre_exec case), dropping it
                // here would close the very fd this function exists to
                // keep open -- forget it instead so it isn't closed out
                // from under the persisted redirect. When source != target
                // there's no such risk, and letting `file` drop normally
                // correctly closes the now-redundant source descriptor.
                if srcfd == fd {
                    std::mem::forget(file);
                }
            }
            ExtraFd::Dup { fd, source } => {
                if unsafe { dup2(source, fd) } == -1 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                clear_cloexec(fd);
            }
            ExtraFd::Close(fd) => {
                if unsafe { close(fd) } == -1 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
            }
        }
    }
    Ok(())
}

// Persists resolve_plain_fd012's raw Files onto this process's own fd
// 0/1/2, for `exec`'s redirect-only form. Same dup2 + clear_cloexec +
// forget-on-self-dup pattern as apply_fds_to_self, and for the same
// reasons (no execve immediately follows to bypass Rust's normal Drop
// glue the way the pre_exec-based spawn path is protected).
fn apply_fd012_to_self(
    stdin: Option<std::fs::File>,
    stdout: Option<std::fs::File>,
    stderr: Option<std::fs::File>,
) -> Result<(), String> {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }
    for (target, file) in [(0, stdin), (1, stdout), (2, stderr)] {
        let Some(file) = file else { continue };
        let srcfd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
        if unsafe { dup2(srcfd, target) } == -1 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        clear_cloexec(target);
        if srcfd == target {
            std::mem::forget(file);
        }
    }
    Ok(())
}

fn clear_cloexec(fd: i32) {
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    }
    const F_SETFD: i32 = 2;
    unsafe {
        fcntl(fd, F_SETFD, 0);
    }
}

fn apply_fd_redirects(command: &mut Command, dup_stderr_to_stdout: bool, extra_fds: Vec<ExtraFd>) {
    if !dup_stderr_to_stdout && extra_fds.is_empty() {
        return;
    }
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    unsafe {
        command.pre_exec(move || {
            if dup_stderr_to_stdout && dup2(1, 2) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            for ef in &extra_fds {
                match ef {
                    ExtraFd::Open { fd, file } => {
                        if dup2(std::os::unix::io::AsRawFd::as_raw_fd(file), *fd) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        clear_cloexec(*fd);
                    }
                    ExtraFd::Dup { fd, source } => {
                        if dup2(*source, *fd) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        clear_cloexec(*fd);
                    }
                    ExtraFd::Close(fd) => {
                        if close(*fd) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                }
            }
            Ok(())
        });
    }
}

// Here-strings need a real Stdio for the child process; simplest portable
// way to hand it literal content is a temp file, unlinked immediately after
// opening (the open fd keeps the data alive on unix even once unlinked).
fn proc_sub_temp_path() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("bish-procsub-{}-{}", std::process::id(), n))
}

fn here_string_file(content: &str) -> Result<std::fs::File, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("bish-herestring-{}-{}", std::process::id(), n));
    std::fs::write(&path, content).map_err(|e| format!("here-string: {}", e))?;
    let f = std::fs::File::open(&path).map_err(|e| format!("here-string: {}", e))?;
    let _ = std::fs::remove_file(&path);
    Ok(f)
}

// Appends an expansion's value `v` to the in-progress word-split state.
// Quoted values are never split (appended verbatim, like literal text).
// Unquoted values are split on whitespace: interior separators end the
// current field and start a new one; leading/trailing whitespace in `v`
// forces a boundary even when the adjacent side has zero-or-one resulting
// parts (e.g. `pre$x` where x=" y " must split into "pre" and "y", not
// "prey"). A purely empty-or-whitespace unquoted value contributes nothing
// (matching bash: an unquoted unset/empty variable standing alone
// contributes zero arguments, not one empty one).
// Real bash IFS semantics: a run of IFS-whitespace characters (IFS chars
// that are also whitespace) collapses into a single delimiter and is
// trimmed from the very start/end of the value; each occurrence of an
// IFS *non*-whitespace character (e.g. ':') is its own separate delimiter,
// significant even with nothing but another separator on either side (so
// "a::b" with IFS=":" is three fields: "a", "", "b"). A whitespace run
// immediately touching a non-whitespace separator on either side is
// absorbed into that same delimiter ("a : b" with IFS=" :" is "a","b" --
// not "a","","b"). Returns (parts, leading_sep, trailing_sep): the two
// bools tell the caller whether the first/last part is a complete,
// standalone field or should stitch onto whatever's pending from an
// adjacent chunk (see append_splittable).
fn ifs_tokenize(v: &str, ifs: &str) -> (Vec<String>, bool, bool) {
    let is_ws = |c: char| c.is_whitespace() && ifs.contains(c);
    let is_sep = |c: char| ifs.contains(c);
    let chars: Vec<char> = v.chars().collect();
    let n = chars.len();

    let mut i = 0;
    while i < n && is_ws(chars[i]) {
        i += 1;
    }
    let leading_sep = i > 0;
    if i >= n {
        return (Vec::new(), leading_sep, false);
    }

    let mut parts = Vec::new();
    let mut trailing_sep = false;
    loop {
        let start = i;
        while i < n && !is_sep(chars[i]) {
            i += 1;
        }
        parts.push(chars[start..i].iter().collect::<String>());
        if i >= n {
            break;
        }
        while i < n && is_ws(chars[i]) {
            i += 1;
        }
        if i < n && is_sep(chars[i]) {
            i += 1;
            while i < n && is_ws(chars[i]) {
                i += 1;
            }
        }
        if i >= n {
            // A delimiter at the very end of the string is dropped
            // without producing a trailing empty field -- unlike an
            // *embedded* separator (e.g. "a::b" is still "a","","b"),
            // confirmed against real bash: "a:" with IFS=":" splits to
            // exactly one field, "a", not ["a", ""].
            trailing_sep = true;
            break;
        }
    }
    (parts, leading_sep, trailing_sep)
}

// Finds the next IFS-delimited field in `s` (which is assumed to already
// have any leading separator trimmed), returning (field, remainder-with-
// its-own-leading-separator-trimmed). None if `s` contains no further
// delimiter at all. Used by `read NAME1 NAME2 ...`, which needs the
// untouched remainder text for its last variable rather than a fully
// re-tokenized-and-rejoined value.
fn ifs_next_field(s: &str, ifs: &str) -> Option<(String, String)> {
    let is_ws = |c: char| c.is_whitespace() && ifs.contains(c);
    let is_sep = |c: char| ifs.contains(c);
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n && !is_sep(chars[i]) {
        i += 1;
    }
    if i >= n {
        return None;
    }
    let field: String = chars[..i].iter().collect();
    while i < n && is_ws(chars[i]) {
        i += 1;
    }
    if i < n && is_sep(chars[i]) {
        i += 1;
        while i < n && is_ws(chars[i]) {
            i += 1;
        }
    }
    Some((field, chars[i..].iter().collect()))
}

fn append_splittable(fields: &mut Vec<String>, current: &mut Option<String>, v: &str, quoted: bool, ifs: &str) {
    if quoted {
        current.get_or_insert_with(String::new).push_str(v);
        return;
    }
    if v.is_empty() {
        return;
    }
    if ifs.is_empty() {
        // IFS set to the empty string: no splitting at all.
        current.get_or_insert_with(String::new).push_str(v);
        return;
    }
    let (parts, leading_sep, trailing_sep) = ifs_tokenize(v, ifs);
    if parts.is_empty() {
        if let Some(c) = current.take() {
            fields.push(c);
        }
        return;
    }
    if leading_sep {
        if let Some(c) = current.take() {
            fields.push(c);
        }
    }
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            fields.push(current.take().unwrap_or_default());
        }
        current.get_or_insert_with(String::new).push_str(part);
    }
    if trailing_sep {
        fields.push(current.take().unwrap_or_default());
    }
}

// Like append_splittable, but for "$@": the parts are already well-defined
// (one per positional parameter, never re-split even if a param contains
// whitespace) rather than derived by splitting a joined string.
fn append_parts(fields: &mut Vec<String>, current: &mut Option<String>, parts: &[String]) {
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            fields.push(current.take().unwrap_or_default());
        }
        current.get_or_insert_with(String::new).push_str(part);
    }
}

// Pairs append_splittable's field-boundary logic with a second, escaped
// copy of the same value for glob-pattern purposes (see expand_word_split).
// glob::escape only ever inserts backslashes before `*?[\@!+(`, none of
// which are whitespace, so splitting the escaped copy on the same IFS
// lands on the same boundaries as splitting `v` itself -- except in the
// pathological case of an IFS that itself contains one of those
// characters, an accepted, exceedingly rare edge case.
fn append_splittable_glob(
    fields: &mut Vec<String>,
    current: &mut Option<String>,
    patterns: &mut Vec<String>,
    pattern_current: &mut Option<String>,
    v: &str,
    quoted: bool,
    ifs: &str,
) {
    append_splittable(fields, current, v, quoted, ifs);
    let p = if quoted { crate::glob::escape(v) } else { v.to_string() };
    append_splittable(patterns, pattern_current, &p, quoted, ifs);
}

// append_parts' counterpart to append_splittable_glob. "$@"/array-keys
// parts always arrive already-quoted (that's why append_parts exists, as
// opposed to append_splittable), so every part is escaped for the pattern
// copy -- these fields are never glob-eligible.
fn append_parts_glob(
    fields: &mut Vec<String>,
    current: &mut Option<String>,
    patterns: &mut Vec<String>,
    pattern_current: &mut Option<String>,
    parts: &[String],
) {
    append_parts(fields, current, parts);
    let escaped: Vec<String> = parts.iter().map(|p| crate::glob::escape(p)).collect();
    append_parts(patterns, pattern_current, &escaped);
}

fn strip_prefix_glob(s: &str, pattern: &str, longest: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let lens: Vec<usize> = if longest { (0..=chars.len()).rev().collect() } else { (0..=chars.len()).collect() };
    for len in lens {
        let candidate: String = chars[..len].iter().collect();
        if glob::matches(pattern, &candidate) {
            return chars[len..].iter().collect();
        }
    }
    s.to_string()
}

fn strip_suffix_glob(s: &str, pattern: &str, longest: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let lens: Vec<usize> = if longest { (0..=n).rev().collect() } else { (0..=n).collect() };
    for len in lens {
        let candidate: String = chars[n - len..].iter().collect();
        if glob::matches(pattern, &candidate) {
            return chars[..n - len].iter().collect();
        }
    }
    s.to_string()
}

// `${V^pattern}` family. An empty pattern (the common `${V^^}` shape with
// nothing after it) matches every character; otherwise each candidate
// character is matched against the pattern with the same glob matcher
// `case` patterns use. `all` picks every matching char vs just the first.
fn apply_case_convert(cur: &str, pattern: &str, upper: bool, all: bool) -> String {
    let mut result = String::with_capacity(cur.len());
    let mut first = true;
    for ch in cur.chars() {
        let convert = (all || first) && (pattern.is_empty() || glob::matches(pattern, &ch.to_string()));
        if convert {
            if upper {
                result.extend(ch.to_uppercase());
            } else {
                result.extend(ch.to_lowercase());
            }
        } else {
            result.push(ch);
        }
        first = false;
    }
    result
}

// `${V:offset}` / `${V:offset:length}`. Character-indexed (not byte), same
// as everywhere else length/indexing happens in this shell. Negative
// offset counts back from the end; negative length is an end position
// counted from the end too (bash: "an offset from the end of the string"),
// not an error.
fn substring_expand(s: &str, offset: i64, length: Option<i64>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    let start = if offset < 0 { (n + offset).max(0) } else { offset.min(n) };
    let end = match length {
        None => n,
        Some(l) if l < 0 => (n + l).max(start),
        Some(l) => (start + l).min(n),
    };
    if end <= start {
        return String::new();
    }
    chars[start as usize..end as usize].iter().collect()
}

// Finds the leftmost-longest glob match of `pattern` in `chars` at or
// after `start_from`, honoring `anchor` (Start/End restrict the search to
// that specific boundary, matching how `#`/`%` pattern-stripping anchors
// its own search). Returns the matched (start, end) character range.
fn find_glob_match(chars: &[char], start_from: usize, pattern: &str, anchor: ReplaceAnchor) -> Option<(usize, usize)> {
    let n = chars.len();
    match anchor {
        ReplaceAnchor::Start => {
            if start_from > 0 {
                return None;
            }
            (start_from..=n).rev().find_map(|end| {
                let candidate: String = chars[start_from..end].iter().collect();
                glob::matches(pattern, &candidate).then_some((start_from, end))
            })
        }
        ReplaceAnchor::End => (start_from..=n).find_map(|s| {
            let candidate: String = chars[s..n].iter().collect();
            glob::matches(pattern, &candidate).then_some((s, n))
        }),
        ReplaceAnchor::None => {
            for s in start_from..=n {
                if let Some((s0, e0)) = (s..=n).rev().find_map(|end| {
                    let candidate: String = chars[s..end].iter().collect();
                    glob::matches(pattern, &candidate).then_some((s, end))
                }) {
                    return Some((s0, e0));
                }
            }
            None
        }
    }
}

// `${V/pat/repl}` (first match), `${V//pat/repl}` (all matches),
// `${V/#pat/repl}` / `${V/%pat/repl}` (anchored -- always a single check,
// `global` doesn't apply to them since an anchored match can only occur
// once).
fn glob_replace(s: &str, pattern: &str, repl: &str, global: bool, anchor: ReplaceAnchor) -> String {
    let chars: Vec<char> = s.chars().collect();
    if matches!(anchor, ReplaceAnchor::Start | ReplaceAnchor::End) {
        return match find_glob_match(&chars, 0, pattern, anchor) {
            Some((s0, e0)) => {
                let mut out: String = chars[..s0].iter().collect();
                out.push_str(repl);
                out.extend(&chars[e0..]);
                out
            }
            None => s.to_string(),
        };
    }
    let mut out = String::new();
    let mut pos = 0;
    loop {
        match find_glob_match(&chars, pos, pattern, ReplaceAnchor::None) {
            Some((s0, e0)) => {
                out.extend(&chars[pos..s0]);
                out.push_str(repl);
                // An empty match (a pattern like a bare "*" can't produce
                // one here since matching is greedy-longest, but stay
                // defensive) must still advance, or this loops forever.
                pos = if e0 > s0 { e0 } else { e0 + 1 };
                if !global || pos > chars.len() {
                    if pos <= chars.len() {
                        out.extend(&chars[pos..]);
                    }
                    break;
                }
            }
            None => {
                out.extend(&chars[pos..]);
                break;
            }
        }
    }
    out
}

// Kept in sync with run_single's builtin dispatch match by hand -- used
// only by `command -v`/`type` to classify a name, not to actually run
// anything, so a name missing from this list just means those two
// diagnostic builtins under-report it as an external/not-found rather
// than anything actually breaking.
const KNOWN_BUILTINS: &[&str] = &[
    ":",
    "cd",
    "export",
    "let",
    "break",
    "continue",
    "test",
    "[",
    "[[",
    "return",
    "shift",
    "local",
    "exit",
    "read",
    "mapfile",
    "readarray",
    "eval",
    "source",
    ".",
    "trap",
    "jobs",
    "fg",
    "bg",
    "wait",
    "kill",
    "getopts",
    "unset",
    "set",
    "declare",
    "typeset",
    "readonly",
    "exec",
    "command",
    "type",
    "hash",
    "shopt",
    "umask",
    "pushd",
    "popd",
    "dirs",
    "ulimit",
    "alias",
    "unalias",
    "window",
    "w",
    "win",
];

fn is_known_builtin(name: &str) -> bool {
    KNOWN_BUILTINS.contains(&name)
}

fn resolve_in_path(name: &str) -> Option<String> {
    if name.contains('/') {
        return if std::path::Path::new(name).is_file() { Some(name.to_string()) } else { None };
    }
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let candidate = std::path::Path::new(dir).join(name);
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(&candidate) {
                Ok(meta) if meta.permissions().mode() & 0o111 != 0 => {
                    return Some(candidate.to_string_lossy().into_owned());
                }
                _ => continue,
            }
        }
        #[cfg(not(unix))]
        {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn open_out(path: &str, append: bool) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .map_err(|e| format!("{}: {}", path, e))
}

fn command_own_redirects(cmd: &parser::Command) -> &[Redirect] {
    match cmd {
        parser::Command::If { redirects, .. } => redirects,
        parser::Command::While { redirects, .. } => redirects,
        parser::Command::For { redirects, .. } => redirects,
        parser::Command::CFor { redirects, .. } => redirects,
        parser::Command::Select { redirects, .. } => redirects,
        parser::Command::Case { redirects, .. } => redirects,
        parser::Command::Group(_, redirects) => redirects,
        parser::Command::Subshell(_, redirects) => redirects,
        parser::Command::Arith(_, redirects) => redirects,
        parser::Command::Test(_, redirects) => redirects,
        _ => &[],
    }
}

fn kill_all(children: Vec<std::process::Child>) {
    for mut c in children {
        let _ = c.kill();
        let _ = c.wait();
    }
}

// Shared by write_command_error and check_nounset: writes to `target`'s
// file (append) if set, else falls back to the shell's real stderr.
fn write_diagnostic(target: &Option<String>, msg: &str) {
    match target {
        Some(path) => {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                use std::io::Write;
                let _ = writeln!(f, "{}", msg);
            } else {
                eprintln!("{}", msg);
            }
        }
        None => eprintln!("{}", msg),
    }
}
