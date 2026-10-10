// The builtins about the shell itself: its options, what a name means to
// it, and how a function reads its arguments.

use crate::exec::{
    BUILTIN_HELP, ExecResult, KNOWN_BUILTINS, KNOWN_SHOPT_OPTIONS, Shell, command_own_redirects, resolve_in_path, sh_eprintln, sh_println,
    shopt_default_on,
};
use crate::parser;
use crate::parser::Redirect;

// `getopts optstring name [args...]`. A trailing `:` in optstring marks an
// option with an argument (`ab:c`); a leading `:` selects silent error
// mode, reported through OPTARG and `?`/`:`, as in bash.
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

    // Every branch that doesn't set OPTARG leaves it unset, so it's cleared
    // once here.
    sh.remove_var("OPTARG");
    // `-ab` is two options in one word. OPTIND names the word, so the
    // position within it lives on the shell, 0 meaning not started. bash
    // also keeps it hidden, and resets it when a script assigns OPTIND (see
    // `Shell::getopts_offset`).
    let at = match sh.getopts_offset {
        0 => 1,
        n => n,
    };
    let word: Vec<char> = cur.chars().collect();
    let opt_char = word.get(at).copied().unwrap_or('?');
    // Whether the word has letters after this one. While it does, OPTIND
    // stays and only the offset moves.
    let more_in_word = at + 1 < word.len();
    let silent = optstring.starts_with(':');
    let spec = optstring.trim_start_matches(':');

    // Every exit done with this word advances OPTIND; the one that isn't
    // reassigns the same value and then records the offset. In that order,
    // since assigning OPTIND clears the offset.
    let step = |sh: &mut Shell, words: usize| {
        sh.assign_var("OPTIND", (optind + words).to_string());
    };
    let stay = |sh: &mut Shell| {
        sh.assign_var("OPTIND", optind.to_string());
        sh.getopts_offset = at + 1;
    };

    let Some(pos) = spec.find(opt_char) else {
        if silent {
            sh.assign_var(&varname, "?".to_string());
            sh.assign_var("OPTARG", opt_char.to_string());
        } else {
            // bash's wording, which names neither the builtin nor quotes
            // the letter.
            sh_eprintln!(sh, "bish: illegal option -- {}", opt_char);
            sh.assign_var(&varname, "?".to_string());
        }
        if more_in_word {
            stay(sh)
        } else {
            step(sh, 1)
        }
        return ExecResult::Status(0);
    };

    let needs_arg = spec.as_bytes().get(pos + 1) == Some(&b':');
    if needs_arg {
        // An argument glued to the option takes the rest of the word, however
        // deep in a cluster: `-ab v` gives `b` the `v`, and so does `-abv`.
        let rest: String = word[at + 1..].iter().collect();
        if !rest.is_empty() {
            sh.assign_var("OPTARG", rest);
            step(sh, 1);
        } else if idx + 1 < positional.len() {
            sh.assign_var("OPTARG", positional[idx + 1].clone());
            step(sh, 2);
        } else {
            if silent {
                sh.assign_var(&varname, ":".to_string());
                sh.assign_var("OPTARG", opt_char.to_string());
            } else {
                sh_eprintln!(sh, "bish: option requires an argument -- {}", opt_char);
                sh.assign_var(&varname, "?".to_string());
            }
            step(sh, 1);
            return ExecResult::Status(0);
        }
    } else if more_in_word {
        stay(sh);
    } else {
        step(sh, 1);
    }
    sh.assign_var(&varname, opt_char.to_string());
    ExecResult::Status(0)
}

// `shopt [-su] [-q] [-p] [NAME ...]`. Bare, it lists every option; `-s`
// or `-u` alone list those on or off; with NAMEs they set those. `-p`
// prints `shopt -s/-u NAME` lines that can be fed back in. A NAME not in
// KNOWN_SHOPT_OPTIONS is "invalid shell option name", as in bash; see that
// list for what each does in bish.
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
                // A fact about how the shell started, not a setting: bash
                // accepts `-s` and `-u` with status 0 and changes nothing.
                if *n == "login_shell" {
                    continue;
                }
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
            // With names, the status answers too: 0 only when all are on,
            // so `shopt -q NAME` and `shopt NAME` work alike in a
            // condition. The bare listing always returns 0.
            let all_on = targets.iter().all(|n| sh.shopt_is_on(n));
            for n in targets {
                sh.print_shopt_line(n, reusable);
            }
            i32::from(!listing_everything && !all_on)
        }
    }
}

// `caller [N]`: where the current function was called from. `caller 0`
// is the innermost call: its line, the function containing it, and the
// file. `caller 1` is the call to that function, and so on. Past the top
// it prints nothing and fails, which ends `while caller $i; do ...`. Bare
// `caller` is line and file only, bash's short form.
//
// Who made a call comes from the frame below; at the bottom, bash names
// it `main`.
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
                // bash's shape: the complaint, the usage, then 2. A
                // malformed argument is a usage error, not 1, which means
                // "no such frame".
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
    // The enclosing function and its file both come from the frame below,
    // whose code made the call. That file differs from `frame.source` once
    // something is sourced: a function in a library reports the library.
    let (enclosing, source) = match idx.checked_sub(1) {
        Some(below) => {
            let name = sh.call_stack[below].called.clone();
            let file = sh.function_sources.get(&name).cloned().unwrap_or_else(|| frame.source.clone());
            (name, file)
        }
        // Nothing below: the script itself made the call, which bash names
        // `main`.
        None => ("main".to_string(), frame.source.clone()),
    };
    sh_println!(sh, "{} {} {}", frame.call_line, enclosing, source);
    0
}

// `help [NAME...]`: an index of the builtins. A name may be a glob, as
// in bash (`help comp*`); one that matches nothing is an error.
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

// `enable [-n] [-a] [NAME...]`: which builtins are in service. `-n NAME`
// takes one out (`enable -n echo; echo hi` runs /bin/echo); bare `NAME`
// puts it back. With no names it lists the enabled ones, or all with
// `-a`. bash's `-f`/`-d` load builtins from a shared object, which bish
// doesn't do, so they're refused by name.
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
        crate::sort::sort(&mut listed);
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

// Every short flag bash's `set` accepts. Most do nothing here (see
// `apply_shell_flag`), but accepting exactly bash's set makes `set -Z` a
// reported typo.
const SET_FLAGS: &str = "abefhkmnptuvxBCEHPTro";
const SET_USAGE: &str = "set [-abefhkmnptuvxBCEHPT] [-o option-name] [--] [-] [arg ...]";

// `set -o` (a padded name/on-off table) and `set +o` (the same as
// `set -o`/`+o NAME` commands). Only options this shell acts on are listed
// (see SET_O_OPTIONS).
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

// `set [-euxo pipefail] [--] [args...]`. Flags cluster, including a
// cluster ending in `o`: `set -euo pipefail` sets e, u, then
// `-o pipefail`.
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
            if let Some(flags) = rest.strip_suffix('o') {
                for c in flags.chars() {
                    sh.apply_shell_flag(c, true);
                }
                if let Some(optname) = args.get(idx + 1) {
                    if sh.shell_option_enabled(optname).is_none() {
                        sh_eprintln!(sh, "bish: set: {optname}: invalid option name");
                        return 2;
                    }
                    sh.apply_shell_option(optname, true);
                    idx += 2;
                    continue;
                }
                // Bare `set -o`: the table.
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
            if let Some(flags) = rest.strip_suffix('o') {
                for c in flags.chars() {
                    sh.apply_shell_flag(c, false);
                }
                if let Some(optname) = args.get(idx + 1) {
                    if sh.shell_option_enabled(optname).is_none() {
                        sh_eprintln!(sh, "bish: set: {optname}: invalid option name");
                        return 2;
                    }
                    sh.apply_shell_option(optname, false);
                    idx += 2;
                    continue;
                }
                // Bare `set +o`: the state as commands that restore it, for
                // `eval "$(set +o)"`.
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
    // `&` on a compound makes a job, so a child: `{ ...; } &` doesn't finish
    // before the next command, sets `$!`, can be waited for, and doesn't
    // change this shell (`v=0; { v=1; } &` leaves `v` 0, and an `exit` in it
    // can't end the shell). Simple commands are handled in
    // `dispatch_builtin_or_external`, and a subshell and a coproc background
    // themselves.
    let backgrounds_itself = matches!(cmd, parser::Command::Simple(_) | parser::Command::Subshell(..) | parser::Command::Coproc { .. });
    if background && !backgrounds_itself {
        return sh.run_compound_redirected(cmd, &[], true);
    }
    sh.run_command_body(cmd, background)
}

// `type [-a] [-p] [-P] [-t] name...`: reports alias, keyword, function,
// builtin or file, or "not found" (status 1). `-t` prints just the kind,
// and nothing for an unknown name: `[ "$(type -t x)" = function ]` is how a
// script asks.
pub(crate) fn run_type(sh: &mut Shell, args: &[String]) -> i32 {
    let mut path_only = false;
    // `-P` forces the PATH search, ignoring other resolutions, so a script
    // can reach `/usr/bin/echo` past the builtin. `-p` prints a path only
    // where `type` would say "file".
    let mut force_path = false;
    let mut kind_only = false;
    // `-a` reports every way the name resolves, not just the first.
    let mut all = false;
    let mut names: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "-p" => path_only = true,
            "-P" => {
                path_only = true;
                force_path = true;
            }
            "-t" => kind_only = true,
            "-a" => all = true,
            _ => names.push(a),
        }
    }
    let mut status = 0;
    for name in names {
        let mut found = false;
        if force_path {
            match resolve_in_path(name, &sh.lookup_var("PATH")) {
                Some(p) => sh_println!(sh, "{}", p),
                None => status = 1,
            }
            continue;
        }
        // bash's order: alias, keyword, function, builtin, file. An alias
        // counts only when alias expansion is on, since `type` says what
        // the shell would do; bash reports one interactively but not under
        // `-c`.
        let alias = sh.shopt_is_on("expand_aliases").then(|| sh.aliases.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())).flatten();
        if let Some(target) = alias {
            if kind_only {
                sh_println!(sh, "alias");
            } else if !path_only {
                sh_println!(sh, "{} is aliased to `{}'", name, target);
            }
            found = true;
            if !all {
                continue;
            }
        }
        if crate::compgen::KEYWORDS.contains(&name.as_str()) {
            if kind_only {
                sh_println!(sh, "keyword");
            } else if !path_only {
                sh_println!(sh, "{} is a shell keyword", name);
            }
            found = true;
            if !all {
                continue;
            }
        }
        if sh.functions.contains_key(name.as_str()) {
            if kind_only {
                sh_println!(sh, "function");
            } else if !path_only {
                sh_println!(sh, "{} is a function", name);
            }
            found = true;
            if !all {
                continue;
            }
        }
        if sh.is_active_builtin(name) {
            if kind_only {
                sh_println!(sh, "builtin");
            } else if !path_only {
                sh_println!(sh, "{} is a shell builtin", name);
            }
            found = true;
            if !all {
                continue;
            }
        }
        match resolve_in_path(name, &sh.lookup_var("PATH")) {
            Some(p) => {
                if kind_only {
                    sh_println!(sh, "file");
                } else {
                    sh_println!(sh, "{}", if path_only { p } else { format!("{} is {}", name, p) });
                }
            }
            None if found => {}
            None => {
                // `-t` prints nothing for an unknown name, but keeps the
                // failing status, as bash does.
                if !kind_only {
                    sh_eprintln!(sh, "bish: type: {}: not found", name);
                }
                status = 1;
            }
        }
    }
    status
}
