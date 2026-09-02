// The builtins that are about the shell itself: what it has turned on,
// what a name means to it, and how a function reads its own arguments.
//
// Free functions taking `&mut Shell` -- see `builtins/mod.rs`.

use crate::exec::{
    BUILTIN_HELP, ExecResult, KNOWN_BUILTINS, KNOWN_SHOPT_OPTIONS, Shell, command_own_redirects, resolve_in_path, sh_eprintln, sh_println,
    shopt_default_on,
};
use crate::parser;
use crate::parser::Redirect;

// getopts optstring name [args...]. Options requiring an argument are
// marked with a trailing ':' in optstring (e.g. "ab:c"); a leading ':'
// switches to "silent" error mode (custom handling via OPTARG/'?'/':'
// instead of a printed message), matching bash.
pub(crate) fn run_getopts(sh: &mut Shell, args: &[String]) -> ExecResult {
    let optstring = args.first().cloned().unwrap_or_default();
    let varname = match args.get(1) {
        Some(v) => v.clone(),
        None => {
            sh_eprintln!(sh, "bish: getopts: usage: getopts optstring name [args]");
            return ExecResult::Status(2);
        }
    };
    let positional: Vec<String> = if args.len() > 2 { args[2..].to_vec() } else { sh.arg_frames.last().cloned().unwrap_or_default() };

    let optind: usize = sh.lookup_var("OPTIND").trim().parse().unwrap_or(1);
    let idx = optind.saturating_sub(1);

    if idx >= positional.len() {
        return ExecResult::Status(1);
    }
    let cur = positional[idx].clone();
    if !cur.starts_with('-') || cur == "-" {
        return ExecResult::Status(1);
    }
    if cur == "--" {
        sh.assign_var("OPTIND", (optind + 1).to_string());
        return ExecResult::Status(1);
    }

    let opt_char = cur.chars().nth(1).unwrap_or('?');
    let silent = optstring.starts_with(':');
    let spec = optstring.trim_start_matches(':');

    let Some(pos) = spec.find(opt_char) else {
        if silent {
            sh.assign_var(&varname, "?".to_string());
            sh.assign_var("OPTARG", opt_char.to_string());
        } else {
            sh_eprintln!(sh, "bish: getopts: illegal option -- '{}'", opt_char);
            sh.assign_var(&varname, "?".to_string());
        }
        sh.assign_var("OPTIND", (optind + 1).to_string());
        return ExecResult::Status(0);
    };

    let needs_arg = spec.as_bytes().get(pos + 1) == Some(&b':');
    if needs_arg {
        let rest: String = cur.chars().skip(2).collect();
        if !rest.is_empty() {
            sh.assign_var("OPTARG", rest);
            sh.assign_var("OPTIND", (optind + 1).to_string());
        } else if idx + 1 < positional.len() {
            sh.assign_var("OPTARG", positional[idx + 1].clone());
            sh.assign_var("OPTIND", (optind + 2).to_string());
        } else {
            if silent {
                sh.assign_var(&varname, ":".to_string());
                sh.assign_var("OPTARG", opt_char.to_string());
            } else {
                sh_eprintln!(sh, "bish: getopts: option requires an argument -- '{}'", opt_char);
                sh.assign_var(&varname, "?".to_string());
            }
            sh.assign_var("OPTIND", (optind + 1).to_string());
            return ExecResult::Status(0);
        }
    } else {
        sh.assign_var("OPTIND", (optind + 1).to_string());
    }
    sh.assign_var(&varname, opt_char.to_string());
    ExecResult::Status(0)
}

// shopt [-su] [-q] [-p] [NAME ...]. Bare `shopt` lists every known
// option's on/off state; `shopt -s`/`shopt -u` alone list only the
// ones currently on/off (respectively); either with NAMEs given
// toggles just those. `-p` prints in the same `shopt -s/-u NAME` form
// that can be fed back in, instead of the plain "NAME\ton/off" table.
// A NAME not in KNOWN_SHOPT_OPTIONS is rejected up front, matching
// real bash's own "invalid shell option name" error -- see that
// list's own doc comment for what most of these names actually do (or
// don't do) in bish.
pub(crate) fn run_shopt(sh: &mut Shell, args: &[String]) -> i32 {
    let mut mode: Option<bool> = None; // Some(true)=-s, Some(false)=-u
    let mut quiet = false;
    let mut reusable = false;
    let mut names: Vec<&str> = Vec::new();
    if let Some(bad) = crate::exec::first_unknown_option(args, "supqo") {
        return crate::exec::bad_option_status(sh, "shopt", &bad, "shopt [-pqsu] [-o] [optname ...]");
    }
    for a in args {
        match a.as_str() {
            "-s" => mode = Some(true),
            "-u" => mode = Some(false),
            "-q" => quiet = true,
            "-p" => reusable = true,
            _ if a.starts_with('-') => {}
            other => names.push(other),
        }
    }
    for n in &names {
        if shopt_default_on(n).is_none() {
            sh_eprintln!(sh, "bish: shopt: {n}: invalid shell option name");
            return 1;
        }
    }
    match mode {
        Some(on) if names.is_empty() => {
            let matching: Vec<&str> = KNOWN_SHOPT_OPTIONS.iter().map(|(n, _)| *n).filter(|n| sh.shopt_is_on(n) == on).collect();
            for n in matching {
                sh.print_shopt_line(n, reusable);
            }
            0
        }
        Some(on) => {
            for n in &names {
                sh.shopt_options.insert(n.to_string(), on);
            }
            0
        }
        None if quiet => {
            if names.iter().all(|n| sh.shopt_is_on(n)) {
                0
            } else {
                1
            }
        }
        None => {
            let listing_everything = names.is_empty();
            let targets: Vec<&str> = if listing_everything { KNOWN_SHOPT_OPTIONS.iter().map(|(n, _)| *n).collect() } else { names };
            // With names, the status answers the question as well as
            // the output does -- 0 only when every one of them is on,
            // which is what makes `shopt -q NAME` and plain
            // `shopt NAME` interchangeable in a condition. The bare
            // listing is not a question, so it is always 0.
            let all_on = targets.iter().all(|n| sh.shopt_is_on(n));
            for n in targets {
                sh.print_shopt_line(n, reusable);
            }
            i32::from(!listing_everything && !all_on)
        }
    }
}

// `caller [N]` -- where the function you are in was called from.
//
// `caller 0` is the innermost call: the line it sits on, the
// function containing that line, and the file. `caller 1` is the
// call to *that* function, and so on outwards. A depth past the top
// of the stack prints nothing and fails, which is what makes
// `while caller $i; do ...` terminate.
//
// Bare `caller` is the short form: line and file only, no function
// name -- bash's own shape, and the reason this is not just `caller
// 0` with a default.
//
// Who made a call is the `called` of the frame below rather than
// anything stored: at the bottom of the stack nothing called it, and
// bash names that `main`.
pub(crate) fn run_caller(sh: &mut Shell, args: &[String]) -> i32 {
    if sh.call_stack.is_empty() {
        return 1;
    }
    let depth = match args.first() {
        None => {
            let idx = sh.call_stack.len() - 1;
            let source = match idx.checked_sub(1) {
                Some(below) => {
                    let name = &sh.call_stack[below].called;
                    sh.function_sources.get(name).cloned().unwrap_or_else(|| sh.call_stack[idx].source.clone())
                }
                None => sh.call_stack[idx].source.clone(),
            };
            sh_println!(sh, "{} {}", sh.call_stack[idx].call_line, source);
            return 0;
        }
        Some(a) => match a.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                // bash's own shape here: the complaint, then the
                // usage, then 2 -- a malformed argument is a usage
                // error, not "no such frame", which is what 1 means
                // and is what `while caller $i` stops on.
                sh_eprintln!(sh, "bish: caller: {a}: invalid number");
                sh_eprintln!(sh, "caller: usage: caller [expr]");
                return 2;
            }
        },
    };
    let Some(idx) = sh.call_stack.len().checked_sub(depth + 1) else {
        return 1;
    };
    let frame = &sh.call_stack[idx];
    // The enclosing function, and the file its body lives in -- both
    // come from the frame *below*, because that is whose code made
    // this call. They are only the same file as `frame.source` until
    // something is sourced: a function defined in a library and
    // called from there reports the library, not the script that
    // sourced it. Same index for both, since they are two halves of
    // one answer.
    let (enclosing, source) = match idx.checked_sub(1) {
        Some(below) => {
            let name = sh.call_stack[below].called.clone();
            let file = sh.function_sources.get(&name).cloned().unwrap_or_else(|| frame.source.clone());
            (name, file)
        }
        // Nothing below: the call was made by the script itself,
        // which bash names `main`, and the frame already recorded
        // which file that was.
        None => ("main".to_string(), frame.source.clone()),
    };
    sh_println!(sh, "{} {} {}", frame.call_line, enclosing, source);
    0
}

// `help [NAME...]` -- an index of the builtins.
//
// A name may be a glob, as bash's is, so `help comp*` works. A name
// that matches nothing is an error naming it, since silence would
// look like a builtin with nothing to say about it.
pub(crate) fn run_help(sh: &mut Shell, args: &[String]) -> i32 {
    let names: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if names.is_empty() {
        sh_println!(sh, "bish's builtins. `help NAME` for one of them; NAME may be a glob.");
        sh_println!(sh, "Anything not listed here is an ordinary command -- try its own --help.");
        sh_println!(sh, "");
        for (name, summary) in BUILTIN_HELP {
            sh_println!(sh, "  {name:<10} {summary}");
        }
        return 0;
    }
    let mut status = 0;
    for name in names {
        let matched: Vec<&(&str, &str)> = BUILTIN_HELP.iter().filter(|(b, _)| *b == name.as_str() || crate::glob::matches(name, b)).collect();
        if matched.is_empty() {
            sh_eprintln!(sh, "bish: help: no help topics match `{name}'");
            status = 1;
            continue;
        }
        for (b, summary) in matched {
            sh_println!(sh, "  {b:<10} {summary}");
        }
    }
    status
}

// `enable [-n] [-a] [NAME...]` -- which builtins are in service.
//
// `-n NAME` takes one out, so `enable -n echo; echo hi` runs
// /bin/echo. Bare `NAME` puts it back. With no names it lists: the
// enabled ones by default, all of them with `-a`.
//
// bash also has `-f`/`-d` for loading builtins from a shared object.
// bish has no dynamic loading and no plans for any, so those are
// refused by name rather than silently ignored -- this shell's own
// convention for something it does not do.
pub(crate) fn run_enable(sh: &mut Shell, args: &[String]) -> i32 {
    let (mut disable, mut list_all, mut names) = (false, false, Vec::new());
    for arg in args {
        match arg.as_str() {
            "-n" => disable = true,
            "-a" => list_all = true,
            "-p" => {}
            "-f" | "-d" => {
                sh_eprintln!(sh, "bish: enable: {arg}: dynamic loading is not supported");
                return 2;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                sh_eprintln!(sh, "bish: enable: {other}: invalid option");
                return 2;
            }
            other => names.push(other.to_string()),
        }
    }
    if names.is_empty() {
        let mut listed: Vec<&str> = KNOWN_BUILTINS.iter().copied().filter(|b| list_all || !sh.disabled_builtins.contains(*b)).collect();
        listed.sort_unstable();
        for b in listed {
            let state = if sh.disabled_builtins.contains(b) { "enable -n" } else { "enable" };
            sh_println!(sh, "{state} {b}");
        }
        return 0;
    }
    let mut status = 0;
    for name in names {
        if !KNOWN_BUILTINS.contains(&name.as_str()) {
            sh_eprintln!(sh, "bish: enable: {name}: not a shell builtin");
            status = 1;
            continue;
        }
        if disable {
            sh.disabled_builtins.insert(name);
        } else {
            sh.disabled_builtins.remove(&name);
        }
    }
    status
}

// set [-euxo pipefail] [--] [args...]. Combined single-char flags
// (-eu, -ex, -eux) work; `-o name` must be its own token (not combined
// into a cluster with other short flags) -- matches real bash, which
// also rejects e.g. `-euo pipefail` (it consumes `-o` with no argument
// of its own, then tries to parse "pipefail"'s remaining letters as
// further short flags and errors on the first invalid one).
// Every short flag real bash's `set` accepts. Most of them do nothing
// here (see apply_shell_flag for the ones that do) -- but accepting
// exactly bash's set, no more, is what makes `set -Z` a reported typo
// instead of a silent no-op.
const SET_FLAGS: &str = "abefhkmnptuvxBCEHPTro";
const SET_USAGE: &str = "set [-abefhkmnptuvxBCEHPT] [-o option-name] [--] [-] [arg ...]";

// `set -o` (a padded name/on-off table) and `set +o` (the same as
// `set -o NAME`/`set +o NAME` commands). Only the options this shell
// actually gates behaviour on are listed -- see SET_O_OPTIONS.
fn list_shell_options(sh: &mut Shell, as_commands: bool) {
    for name in crate::exec::SET_O_OPTIONS {
        let on = sh.shell_option_enabled(name).unwrap_or(false);
        if as_commands {
            sh_println!(sh, "set {}o {}", if on { "-" } else { "+" }, name);
        } else {
            sh_println!(sh, "{:<15}\t{}", name, if on { "on" } else { "off" });
        }
    }
}

pub(crate) fn run_set(sh: &mut Shell, args: &[String]) -> i32 {
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
            if let Some(bad) = rest.chars().find(|c| !SET_FLAGS.contains(*c)) {
                return crate::exec::bad_option_status(sh, "set", &format!("-{bad}"), SET_USAGE);
            }
            if rest == "o" {
                if let Some(optname) = args.get(idx + 1) {
                    if sh.shell_option_enabled(optname).is_none() {
                        sh_eprintln!(sh, "bish: set: {optname}: invalid option name");
                        return 2;
                    }
                    sh.apply_shell_option(optname, true);
                    idx += 2;
                    continue;
                }
                // Bare `set -o`: the on/off table.
                list_shell_options(sh, false);
                return 0;
            }
            for c in rest.chars() {
                sh.apply_shell_flag(c, true);
            }
            idx += 1;
            continue;
        }
        if let Some(rest) = a.strip_prefix('+').filter(|r| !r.is_empty()) {
            if let Some(bad) = rest.chars().find(|c| !SET_FLAGS.contains(*c)) {
                return crate::exec::bad_option_status(sh, "set", &format!("+{bad}"), SET_USAGE);
            }
            if rest == "o" {
                if let Some(optname) = args.get(idx + 1) {
                    if sh.shell_option_enabled(optname).is_none() {
                        sh_eprintln!(sh, "bish: set: {optname}: invalid option name");
                        return 2;
                    }
                    sh.apply_shell_option(optname, false);
                    idx += 2;
                    continue;
                }
                // Bare `set +o`: the same state, but as commands that
                // reproduce it -- the form `eval "$(set +o)"` restores.
                list_shell_options(sh, true);
                return 0;
            }
            for c in rest.chars() {
                sh.apply_shell_flag(c, false);
            }
            idx += 1;
            continue;
        }
        break;
    }
    if saw_dashdash || idx < args.len() {
        let new_args = args[idx..].to_vec();
        if let Some(frame) = sh.arg_frames.last_mut() {
            *frame = new_args;
        }
    }
    0
}

pub(crate) fn run_command(sh: &mut Shell, cmd: &parser::Command, background: bool) -> ExecResult {
    let redirects: &[Redirect] = command_own_redirects(cmd);
    if !redirects.is_empty() {
        return sh.run_compound_redirected(cmd, redirects, background);
    }
    sh.run_command_body(cmd, background)
}

// type [-p] [-t] name... A scoped subset of real bash's `type`:
// reports function/builtin/PATH-resolved-executable, or "not found"
// (status 1). `-a` is accepted but not distinguished from the
// default.
//
// `-t` prints just the kind -- `function`, `builtin`, `file` -- and
// nothing at all for a name that is none of them. That last part is
// the point of it: `[ "$(type -t x)" = function ]` is how a script
// asks, and a sentence like "x is a shell builtin" fails that test
// however true it reads.
pub(crate) fn run_type(sh: &mut Shell, args: &[String]) -> i32 {
    let mut path_only = false;
    let mut kind_only = false;
    let mut names: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "-p" | "-P" => path_only = true,
            "-t" => kind_only = true,
            "-a" => {}
            _ => names.push(a),
        }
    }
    let mut status = 0;
    for name in names {
        if sh.functions.contains_key(name.as_str()) {
            if kind_only {
                sh_println!(sh, "function");
            } else if !path_only {
                sh_println!(sh, "{} is a function", name);
            }
            continue;
        }
        if sh.is_active_builtin(name) {
            if kind_only {
                sh_println!(sh, "builtin");
            } else if !path_only {
                sh_println!(sh, "{} is a shell builtin", name);
            }
            continue;
        }
        match resolve_in_path(name) {
            Some(p) => {
                if kind_only {
                    sh_println!(sh, "file");
                } else {
                    sh_println!(sh, "{}", if path_only { p } else { format!("{} is {}", name, p) });
                }
            }
            None => {
                // `-t` says nothing about a name it does not know:
                // the empty answer is the answer, and bash keeps the
                // failing status without the message.
                if !kind_only {
                    sh_eprintln!(sh, "bish: type: {}: not found", name);
                }
                status = 1;
            }
        }
    }
    status
}
