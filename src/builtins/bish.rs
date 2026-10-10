// `::bish` and its subcommands, plus `bishopt` and `abbr`: what bish has
// and bash doesn't.

use crate::bishedit::snippet::{self, Abbr};
use crate::exec::{
    BishOptDefault, BishOptValue, ExecResult, HOOK_EVENTS, Hook, LspServer, PaneDirection, Shell, Theme, WindowAction, WindowMove, hook_help,
    lsp_help, parse_size_spec, sh_eprintln, sh_println,
};

// The subcommands each `::bish` family answers to, used for both the
// "expected:" line and the "did you mean" beside it.
const BISH_SUBCOMMANDS: &[&str] = &["theme", "window", "hook", "hl", "lsp", "map", "trust"];
const HOOK_SUBCOMMANDS: &[&str] = &["ls", "add", "rm", "help"];
const LSP_SUBCOMMANDS: &[&str] = &["ls", "add", "rm", "status", "log", "restart", "help"];
// `pub(crate)` so `exec::bish_sub_subcommands` gives completion this list
// rather than a copy that can drift.
pub(crate) const THEME_SUBCOMMANDS: &[&str] = &["begin", "end", "list"];
// Long forms only: the one-letter aliases (`s`, `v`, `h`, `=`) would make
// almost any single mistyped character a near miss.
const WINDOW_SUBCOMMANDS: &[&str] = &[
    "next", "previous", "new", "create", "close", "quit", "split", "vsplit", "left", "below", "above", "right", "zoom", "swap", "break", "join",
    "vjoin", "layout", "move", "focus", "save", "restore", "balance", "minimize", "sizeup", "sizedown", "size", "fg",
];

fn listed(names: &[&str]) -> String {
    names.join(", ")
}

pub(crate) fn run_bishopt(sh: &mut Shell, args: &[String], registry: &[(&str, BishOptDefault)]) -> i32 {
    enum Mode<'a> {
        List,
        Get(&'a str, bool), // bool: quiet
        // `--describe [NAME]`: what an option is for, what it accepts, and
        // its current value.
        Describe(Option<&'a str>),
        Set(&'a str, Option<&'a str>),
        Unset(&'a str),
    }
    let mode = match args {
        [] => Mode::List,
        [flag] if flag == "--describe" || flag == "-d" => Mode::Describe(None),
        [flag, name] if flag == "--describe" || flag == "-d" => Mode::Describe(Some(name.as_str())),
        [flag, name] if flag == "--set" || flag == "-s" => Mode::Set(name, None),
        [flag, name, value] if flag == "--set" || flag == "-s" => Mode::Set(name, Some(value)),
        [flag, name] if flag == "--unset" || flag == "-u" => Mode::Unset(name),
        [flag, name] if flag == "--quiet" || flag == "-q" => Mode::Get(name, true),
        [name] => Mode::Get(name, false),
        _ => {
            sh_eprintln!(
                sh,
                "bish: bishopt: usage: bishopt [--quiet|-q NAME | --set|-s NAME [VALUE] | --unset|-u NAME | --describe|-d [NAME] | NAME]"
            );
            return 2;
        }
    };
    match mode {
        Mode::List => {
            for (name, _) in registry {
                sh_println!(sh, "{name}");
            }
            0
        }
        Mode::Describe(which) => {
            if let Some(name) = which
                && !registry.iter().any(|(n, _)| *n == name)
            {
                sh_eprintln!(sh, "bish: bishopt: unknown option '{name}'");
                return 1;
            }
            for line in sh.describe_bishopts(registry, which) {
                sh_println!(sh, "{line}");
            }
            0
        }
        Mode::Get(name, quiet) => match sh.bishopt_value(registry, name) {
            Some(BishOptValue::Bool(on)) => {
                if !quiet {
                    sh_println!(sh, "{}", if on { "on" } else { "off" });
                }
                if on { 0 } else { 1 }
            }
            Some(BishOptValue::Int(n)) => {
                if !quiet {
                    sh_println!(sh, "{n}");
                }
                0
            }
            Some(BishOptValue::Str(s)) => {
                if !quiet {
                    sh_println!(sh, "{s}");
                }
                0
            }
            Some(BishOptValue::Color(text, _)) => {
                if !quiet {
                    sh_println!(sh, "{text}");
                }
                0
            }
            None => {
                {
                    let hint = crate::suggest::did_you_mean(name, registry.iter().map(|(n, _)| *n));
                    sh_eprintln!(sh, "bish: bishopt: {name}: no such option{hint}");
                }
                1
            }
        },
        Mode::Set(name, value) => match (registry.iter().find(|(n, _)| *n == name).map(|(_, d)| d.clone()), value) {
            (None, _) => {
                {
                    let hint = crate::suggest::did_you_mean(name, registry.iter().map(|(n, _)| *n));
                    sh_eprintln!(sh, "bish: bishopt: {name}: no such option{hint}");
                }
                1
            }
            (Some(BishOptDefault::Bool(_)), None | Some("on")) => {
                sh.store_bishopt(name, BishOptValue::Bool(true));
                0
            }
            (Some(BishOptDefault::Bool(_)), Some("off")) => {
                sh.store_bishopt(name, BishOptValue::Bool(false));
                0
            }
            (Some(BishOptDefault::Bool(_)), Some(_)) => {
                sh_eprintln!(sh, "bish: bishopt: --set: {name}: a boolean option only accepts 'on' or 'off'");
                2
            }
            (Some(BishOptDefault::Int(_, _)), None) => {
                sh_eprintln!(sh, "bish: bishopt: --set: {name}: requires a VALUE");
                2
            }
            (Some(BishOptDefault::Int(_, range)), Some(v)) => match v.parse::<i64>() {
                Ok(n) if range.contains(&n) => {
                    sh.store_bishopt(name, BishOptValue::Int(n));
                    0
                }
                Ok(n) => {
                    sh_eprintln!(sh, "bish: bishopt: --set: {name}: {n} is outside {}..{}", range.start(), range.end());
                    2
                }
                Err(_) => {
                    sh_eprintln!(sh, "bish: bishopt: --set: {name}: {v:?} is not a whole number");
                    2
                }
            },
            (Some(BishOptDefault::Str(_)), None) => {
                sh_eprintln!(sh, "bish: bishopt: --set: {name}: requires a VALUE");
                2
            }
            (Some(BishOptDefault::Str(_)), Some(v)) => {
                sh.store_bishopt(name, BishOptValue::Str(v.to_string()));
                0
            }
            (Some(BishOptDefault::Color(_)), None) => {
                sh_eprintln!(sh, "bish: bishopt: --set: {name}: requires a VALUE");
                2
            }
            (Some(BishOptDefault::Color(_)), Some(v)) => match crate::csscolor::parse_terminal_list(v) {
                Ok(c) => {
                    sh.store_bishopt(name, BishOptValue::Color(v.to_string(), c));
                    0
                }
                Err(e) => {
                    sh_eprintln!(sh, "bish: bishopt: --set: {name}: invalid color '{v}': {e}");
                    2
                }
            },
        },
        Mode::Unset(name) => {
            if !registry.iter().any(|(n, _)| *n == name) {
                {
                    let hint = crate::suggest::did_you_mean(name, registry.iter().map(|(n, _)| *n));
                    sh_eprintln!(sh, "bish: bishopt: {name}: no such option{hint}");
                }
                return 1;
            }
            sh.bishopts.remove(name);
            0
        }
    }
}

// `::bish SUBCOMMAND...`: a namespace for bish's own commands, so they
// don't take common words (`theme`, `window`) from the global command
// namespace, where they'd shadow scripts' functions and real programs.
// `::` never starts an ordinary command word.
pub(crate) fn run_bish(sh: &mut Shell, args: &[String]) -> ExecResult {
    match args {
        [sub, rest @ ..] if sub == "theme" => ExecResult::Status(run_bish_theme(sh, rest)),
        // `window` and `win` remain only as command-mode aliases (see
        // `run_single`): a builtin called `window` would shadow a real
        // `window` on `$PATH` in every script.
        [sub, rest @ ..] if sub == "window" || sub == "win" => run_window(sh, rest),
        [sub, rest @ ..] if sub == "hook" => ExecResult::Status(run_hook(sh, rest)),
        [sub, rest @ ..] if sub == "hl" => ExecResult::Status(run_hl(sh, rest)),
        [sub, rest @ ..] if sub == "lsp" => ExecResult::Status(run_lsp(sh, rest)),
        [sub, rest @ ..] if sub == "map" => ExecResult::Status(run_map(sh, rest)),
        [sub, rest @ ..] if sub == "trust" => ExecResult::Status(run_trust(sh, rest)),
        [] => {
            sh_eprintln!(sh, "bish: ::bish: missing subcommand (expected: {})", listed(BISH_SUBCOMMANDS));
            ExecResult::Status(2)
        }
        [other, ..] => {
            let hint = crate::suggest::did_you_mean(other, BISH_SUBCOMMANDS.iter().copied());
            sh_eprintln!(sh, "bish: ::bish: unknown subcommand '{other}'{hint} (expected: {})", listed(BISH_SUBCOMMANDS));
            ExecResult::Status(2)
        }
    }
}

pub(crate) fn run_trust(sh: &mut Shell, args: &[String]) -> i32 {
    let resolve = |sh: &Shell, dir: &str| -> std::path::PathBuf {
        let p = std::path::Path::new(dir);
        if p.is_absolute() { p.to_path_buf() } else { sh.cwd.join(p) }
    };

    match args.first().map(String::as_str) {
        Some("--list") | Some("-l") => {
            for entry in crate::trust::load() {
                sh_println!(sh, "{}\t{}", entry.capabilities, entry.dir.display());
            }
            0
        }
        Some("--check") => {
            let [_, capability, rest @ ..] = args else {
                sh_eprintln!(sh, "bish: ::bish trust: --check: usage: ::bish trust --check CAPABILITY [DIR]");
                return 2;
            };
            if rest.len() > 1 {
                sh_eprintln!(sh, "bish: ::bish trust: --check takes one directory");
                return 2;
            }
            let dir = resolve(sh, rest.first().map(String::as_str).unwrap_or("."));
            // A predicate: silent, with the exit status as the answer, like
            // `test`. Honours `trust_owned`, as bish's git integration
            // does.
            i32::from(!crate::trust::is_trusted(&dir, capability, &sh.bishopt_str("trust_owned")))
        }
        Some("--remove") | Some("-r") => {
            let default = [".".to_string()];
            let dirs: &[String] = if args.len() > 1 { &args[1..] } else { &default };
            let mut status = 0;
            for dir in dirs {
                let path = resolve(sh, dir);
                match crate::trust::untrust(&path) {
                    Ok(true) => {}
                    Ok(false) => {
                        sh_eprintln!(sh, "bish: ::bish trust: {dir}: was not trusted");
                        status = 1;
                    }
                    Err(e) => {
                        sh_eprintln!(sh, "bish: ::bish trust: {e}");
                        return 1;
                    }
                }
            }
            status
        }
        _ => {
            // The default form and `-c GLOB [DIR...]`.
            let mut capabilities = "*".to_string();
            let mut dirs: Vec<String> = Vec::new();
            let mut rest = args;
            while let [head, tail @ ..] = rest {
                match head.as_str() {
                    "-c" | "--capabilities" => {
                        let Some(glob) = tail.first() else {
                            sh_eprintln!(sh, "bish: ::bish trust: {head}: requires a capability glob");
                            return 2;
                        };
                        capabilities = glob.clone();
                        rest = &tail[1..];
                    }
                    other if other.starts_with("--capabilities=") => {
                        capabilities = other["--capabilities=".len()..].to_string();
                        rest = tail;
                    }
                    other if other.starts_with('-') && other != "-" => {
                        sh_eprintln!(sh, "bish: ::bish trust: unknown option '{other}'");
                        return 2;
                    }
                    other => {
                        dirs.push(other.to_string());
                        rest = tail;
                    }
                }
            }
            if dirs.is_empty() {
                dirs.push(".".to_string());
            }
            for dir in &dirs {
                let path = resolve(sh, dir);
                // Trusting a directory that doesn't exist is almost always
                // a typo, and recording it would trust whatever later takes
                // the name.
                if !path.is_dir() {
                    sh_eprintln!(sh, "bish: ::bish trust: {dir}: not a directory");
                    return 1;
                }
                if let Err(e) = crate::trust::trust(&path, &capabilities) {
                    sh_eprintln!(sh, "bish: ::bish trust: {e}");
                    return 1;
                }
            }
            0
        }
    }
}

pub(crate) fn run_hook(sh: &mut Shell, args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("ls") | Some("list") | None => {
            let lang = match sh.hook_lang_flag("ls", &args[1.min(args.len())..]) {
                Ok((lang, [])) => lang,
                Ok(_) => {
                    sh_eprintln!(sh, "bish: ::bish hook: ls: usage: ::bish hook ls [--lang=GLOB]");
                    return 2;
                }
                Err(status) => return status,
            };
            for hook in sh.hooks.clone() {
                // Listing by language asks what would fire for that
                // language, so the glob is matched against the language, as
                // firing does, not glob against glob.
                if let Some(lang) = lang.as_deref()
                    && !crate::glob::matches(&hook.lang, lang)
                {
                    continue;
                }
                sh_println!(sh, "{}\t{}\t{}\t{}", hook.id, hook.event, hook.lang, hook.command);
            }
            0
        }
        Some("add") => {
            let (lang, rest) = match sh.hook_lang_flag("add", &args[1..]) {
                Ok(parsed) => parsed,
                Err(status) => return status,
            };
            let [event, command @ ..] = rest else {
                sh_eprintln!(sh, "bish: ::bish hook: add: usage: ::bish hook add [--lang=GLOB] EVENT COMMAND...");
                return 2;
            };
            if !HOOK_EVENTS.contains(&event.as_str()) {
                sh_eprintln!(sh, "bish: ::bish hook: add: unknown event '{event}' (try `::bish hook help`)");
                return 2;
            }
            if command.is_empty() {
                sh_eprintln!(sh, "bish: ::bish hook: add: no command given");
                return 2;
            }
            let id = sh.next_hook_id;
            sh.next_hook_id += 1;
            sh.hooks.push(Hook { id, event: event.clone(), lang: lang.unwrap_or_else(|| "*".to_string()), command: command.join(" ") });
            // The id is printed: a config that adds a hook may want to
            // remove it.
            sh_println!(sh, "{id}");
            0
        }
        Some("rm") | Some("remove") => {
            let Some(id) = args.get(1).and_then(|a| a.parse::<u64>().ok()) else {
                sh_eprintln!(sh, "bish: ::bish hook: rm: usage: ::bish hook rm <id>");
                return 2;
            };
            let before = sh.hooks.len();
            sh.hooks.retain(|h| h.id != id);
            if sh.hooks.len() == before {
                sh_eprintln!(sh, "bish: ::bish hook: rm: no hook with id {id}");
                return 1;
            }
            0
        }
        Some("help") | Some("--help") | Some("-h") | Some("events") => {
            for line in hook_help() {
                sh_println!(sh, "{line}");
            }
            0
        }
        Some(other) => {
            let hint = crate::suggest::did_you_mean(other, HOOK_SUBCOMMANDS.iter().copied());
            sh_eprintln!(sh, "bish: ::bish hook: unknown subcommand '{other}'{hint} (expected: {})", listed(HOOK_SUBCOMMANDS));
            2
        }
    }
}

// `::bish lsp ls|add|rm|status`: which language servers exist and which
// run. Shaped like `::bish hook`: per-shell ids, `--lang` as a glob, `rm`
// by the id `add` printed.
pub(crate) fn run_lsp(sh: &mut Shell, args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("ls") | Some("list") | None => {
            let rest = &args[1.min(args.len())..];
            let lang = match sh.lsp_lang_flag("ls", rest) {
                Ok((lang, [])) => lang,
                Ok(_) => {
                    sh_eprintln!(sh, "bish: ::bish lsp: ls: usage: ::bish lsp ls [--lang=GLOB]");
                    return 2;
                }
                Err(status) => return status,
            };
            for server in sh.lsp_servers.clone() {
                // As `hook ls --lang`: the glob against the language given.
                if let Some(lang) = lang.as_deref()
                    && !crate::glob::matches(&server.lang, lang)
                {
                    continue;
                }
                let root = if server.root_cmd.is_empty() { server.root_markers.join(",") } else { server.root_cmd.clone() };
                sh_println!(sh, "{}\t{}\t{}\t{}", server.id, server.lang, root, server.command_line());
            }
            0
        }
        Some("add") => {
            // Flags in any order: with four of them, a fixed order would
            // make `--apply-edits=always --lang=rust rust-analyzer` try to
            // run `--apply-edits=always`. Each helper leaves the slice
            // alone when its flag isn't first, so a pass that consumes
            // nothing means the command remains.
            let mut rest = &args[1..];
            let mut lang = None;
            let mut root_markers = None;
            let mut root_cmd = String::new();
            let mut apply_edits = "scoped".to_string();
            let mut settings: Vec<(String, String)> = Vec::new();
            loop {
                let before = rest.len();
                match sh.lsp_lang_flag("add", rest) {
                    Ok((found, after)) => {
                        if found.is_some() {
                            lang = found;
                        }
                        rest = after;
                    }
                    Err(status) => return status,
                }
                let mark = rest.len();
                match sh.lsp_root_flag(rest) {
                    Ok((found, after)) => {
                        if after.len() != mark {
                            root_markers = Some(found);
                        }
                        rest = after;
                    }
                    Err(status) => return status,
                }
                match sh.lsp_root_cmd_flag(rest) {
                    Ok((found, after)) => {
                        if !found.is_empty() {
                            root_cmd = found;
                        }
                        rest = after;
                    }
                    Err(status) => return status,
                }
                let mark = rest.len();
                match sh.lsp_apply_edits_flag(rest) {
                    Ok((found, after)) => {
                        if after.len() != mark {
                            apply_edits = found;
                        }
                        rest = after;
                    }
                    Err(status) => return status,
                }
                match sh.lsp_setting_flag(rest) {
                    Ok((found, after)) => {
                        if let Some(pair) = found {
                            // The last one wins for a repeated key.
                            settings.retain(|(k, _)| k != &pair.0);
                            settings.push(pair);
                        }
                        rest = after;
                    }
                    Err(status) => return status,
                }
                if rest.len() == before {
                    break;
                }
            }
            let root_markers = root_markers.unwrap_or_else(|| vec![".git".to_string()]);
            if rest.is_empty() {
                sh_eprintln!(sh, "bish: ::bish lsp: add: usage: ::bish lsp add [--lang=GLOB] [--root=NAME,...] COMMAND...");
                return 2;
            }
            // The server starts later, from code with no `Shell`, so a
            // restricted shell's rule on program names is applied here.
            if sh.restricted_program_refused("::bish lsp: add", &rest[0]) {
                return 1;
            }
            let id = sh.next_lsp_id;
            sh.next_lsp_id += 1;
            sh.lsp_servers.push(LspServer {
                id,
                lang: lang.unwrap_or_else(|| "*".to_string()),
                command: rest.to_vec(),
                root_markers,
                root_cmd,
                apply_edits,
                settings,
            });
            // The id is printed, as for `hook add`.
            sh_println!(sh, "{id}");
            0
        }
        Some("rm") | Some("remove") => {
            let Some(id) = args.get(1).and_then(|a| a.parse::<u64>().ok()) else {
                sh_eprintln!(sh, "bish: ::bish lsp: rm: usage: ::bish lsp rm <id>");
                return 2;
            };
            let before = sh.lsp_servers.len();
            sh.lsp_servers.retain(|s| s.id != id);
            if sh.lsp_servers.len() == before {
                sh_eprintln!(sh, "bish: ::bish lsp: rm: no language server with id {id}");
                return 1;
            }
            0
        }
        Some("status") => {
            // Collected first: `sh_println!` needs the shell mutably.
            let rows: Vec<String> = sh.lsp.borrow().rows().iter().map(|fields| fields.join("\t")).collect();
            for row in rows {
                sh_println!(sh, "{row}");
            }
            0
        }
        Some("log") => {
            // A server's stderr is kept because when it fails to start,
            // what it said is the only explanation.
            let Some(id) = args.get(1).and_then(|a| a.parse::<u64>().ok()) else {
                sh_eprintln!(sh, "bish: ::bish lsp: log: usage: ::bish lsp log <id>");
                return 2;
            };
            let lines = sh.lsp.borrow().logs(id);
            if lines.is_empty() {
                sh_eprintln!(sh, "bish: ::bish lsp: log: nothing recorded for id {id}");
                return 1;
            }
            for line in lines {
                sh_println!(sh, "{line}");
            }
            0
        }
        Some("restart") => {
            // A dead server stays dead rather than respawning on every file
            // open, which would turn one bad config line into a spawn per
            // keystroke. This is how to say the line is fixed.
            let Some(id) = args.get(1).and_then(|a| a.parse::<u64>().ok()) else {
                sh_eprintln!(sh, "bish: ::bish lsp: restart: usage: ::bish lsp restart <id>");
                return 2;
            };
            let dropped = sh.lsp.borrow_mut().forget(id);
            if dropped == 0 {
                sh_eprintln!(sh, "bish: ::bish lsp: restart: nothing running or failed for id {id}");
                return 1;
            }
            0
        }
        Some("help") | Some("--help") | Some("-h") => {
            for line in lsp_help() {
                sh_println!(sh, "{line}");
            }
            0
        }
        Some(other) => {
            let hint = crate::suggest::did_you_mean(other, LSP_SUBCOMMANDS.iter().copied());
            sh_eprintln!(sh, "bish: ::bish lsp: unknown subcommand '{other}'{hint} (expected: {})", listed(LSP_SUBCOMMANDS));
            2
        }
    }
}

// `::bish hl`: the syntax-highlighting palette. Shaped like `bishopt`
// (`--set`, `--unset`, a bare name to read) for the same job, but separate
// because its names are open: a language server's semantic token types
// can't be known in advance, while bishopt is a closed registry with a
// default and description per entry. The chrome colours (`ui_col_*`) stay
// bishopts.
pub(crate) fn run_hl(sh: &mut Shell, args: &[String]) -> i32 {
    match args {
        [] => {
            for (name, value) in sh.hl_colors() {
                sh_println!(sh, "{name}\t{value}");
            }
            0
        }
        [flag, name] if flag == "--unset" || flag == "-u" => {
            // Even mid-declaration, like `bishopt --unset`: unsetting isn't
            // declaring.
            if sh.hl.remove(name.as_str()).is_none() {
                sh_eprintln!(sh, "bish: ::bish hl: {name} is not set");
                return 1;
            }
            0
        }
        [flag, name, value] if flag == "--set" || flag == "-s" => {
            if let Err(e) = crate::csscolor::parse_terminal_list(value) {
                sh_eprintln!(sh, "bish: ::bish hl: {name}: {e}");
                return 2;
            }
            sh.store_hl(name, value.clone());
            0
        }
        [name] if !name.starts_with('-') => {
            match sh.hl_colors().into_iter().find(|(n, _)| n == name) {
                Some((_, value)) => {
                    sh_println!(sh, "{value}");
                    0
                }
                // Not an error: an open namespace has no unknown names,
                // only unset ones.
                None => 1,
            }
        }
        _ => {
            sh_eprintln!(sh, "bish: ::bish hl: usage: ::bish hl [NAME | --set|-s NAME COLOUR | --unset|-u NAME]");
            2
        }
    }
}

pub(crate) fn run_bish_theme(sh: &mut Shell, args: &[String]) -> i32 {
    match args {
        [sub] if sub == "begin" => run_bish_theme_begin(sh),
        [sub] if sub == "end" => run_bish_theme_end(sh),
        [sub] if sub == "list" => run_bish_theme_list(sh),
        [] => {
            sh_eprintln!(sh, "bish: ::bish theme: missing subcommand (expected: {})", listed(THEME_SUBCOMMANDS));
            2
        }
        [other, ..] => {
            let hint = crate::suggest::did_you_mean(other, THEME_SUBCOMMANDS.iter().copied());
            sh_eprintln!(sh, "bish: ::bish theme: unknown subcommand '{other}'{hint} (expected: {})", listed(THEME_SUBCOMMANDS));
            2
        }
    }
}

// `::bish map [-m GLOB] LHS RHS`: remap a key sequence. Always
// non-recursive (vim's `noremap`), with no recursive form: the right-hand
// side resolves against the default bindings, so a mapping never changes
// meaning because another was defined, and a listing can name its action.
// Flags follow `abbr`, the same add/erase/list shape.
pub(crate) fn run_map(sh: &mut Shell, args: &[String]) -> i32 {
    let (args, mode) = crate::keymap::take_mode_flag(args);
    let mode = mode.unwrap_or_else(|| crate::keymap::DEFAULT_MODE.to_string());
    if !crate::keymap::mode_glob_is_known(&mode) {
        // A glob matching no mode would be stored and never fire, so it's
        // refused here.
        sh_eprintln!(sh, "bish: ::bish map: --mode '{mode}' matches no mode (have: {})", crate::keymap::MODES.join(", "));
        return 2;
    }
    let rest: &[String] = &args;
    match rest.first().map(String::as_str) {
        Some("help") => {
            for line in crate::keymap::usage() {
                sh_println!(sh, "{line}");
            }
            0
        }
        None | Some("-l") | Some("--list") => {
            let listing: Vec<String> = sh
                .mappings
                .iter()
                .filter(|m| crate::keymap::modes_matching(&mode).iter().any(|one| m.applies_to(one)))
                .map(|m| {
                    // The fourth column is the mapping's action, which
                    // exists only in normal and visual modes; elsewhere the
                    // keys are the whole story, shown as a dash.
                    let action = if crate::keymap::has_vim_mode(&m.modes) {
                        crate::keymap::describe_rhs(&m.rhs).unwrap_or_else(|why| format!("({why})"))
                    } else {
                        "-".to_string()
                    };
                    format!("{}\t{}\t{}\t{}", m.modes, crate::keymap::format_keys(&m.lhs), crate::keymap::format_rhs(&m.rhs), action)
                })
                .collect();
            for line in listing {
                sh_println!(sh, "{line}");
            }
            0
        }
        Some("-e") | Some("--erase") => {
            let Some(lhs_text) = rest.get(1) else {
                sh_eprintln!(sh, "bish: ::bish map: --erase: requires a KEY");
                return 2;
            };
            let lhs = match crate::keymap::parse_keys(lhs_text) {
                Ok(keys) => keys,
                Err(why) => {
                    sh_eprintln!(sh, "bish: ::bish map: {why}");
                    return 2;
                }
            };
            let before = sh.mappings.len();
            sh.mappings.retain(|m| !(m.lhs == lhs && m.modes == mode));
            if sh.mappings.len() == before {
                sh_eprintln!(sh, "bish: ::bish map: no mapping for {lhs_text} in mode '{mode}'");
                return 1;
            }
            0
        }
        Some(_) => {
            let (Some(lhs_text), Some(rhs_text)) = (rest.first(), rest.get(1)) else {
                sh_eprintln!(sh, "bish: ::bish map: requires a KEY and what it should do");
                for line in crate::keymap::usage() {
                    sh_eprintln!(sh, "{line}");
                }
                return 2;
            };
            if rest.len() > 2 {
                // Two halves: a right-hand side over several words would
                // make a space ambiguous, which is what `<Space>` is for.
                sh_eprintln!(sh, "bish: ::bish map: too many arguments (a space in a mapping is <Space>)");
                return 2;
            }
            if lhs_text == "<Nop>" {
                sh_eprintln!(sh, "bish: ::bish map: <Nop> is a right-hand side, not a key to press");
                return 2;
            }
            let (lhs, rhs) = match (crate::keymap::parse_keys(lhs_text), crate::keymap::parse_keys(rhs_text)) {
                (Ok(lhs), Ok(rhs)) => (lhs, rhs),
                (Err(why), _) | (_, Err(why)) => {
                    sh_eprintln!(sh, "bish: ::bish map: {why}");
                    return 2;
                }
            };
            if let Some(bad) = lhs.iter().chain(rhs.iter()).find(|k| !crate::keymap::is_mappable(**k)) {
                sh_eprintln!(sh, "bish: ::bish map: {} cannot carry a mapping", crate::keymap::format_keys(&[*bad]));
                return 2;
            }
            // Resolved now, so a right-hand side that means nothing fails
            // where it was written. Only in normal and visual mode, which
            // have named actions: in insert, command and terminal mode keys
            // are keys, and `<Esc>` is a fine insert-mode right-hand side.
            if crate::keymap::has_vim_mode(&mode)
                && let Err(why) = crate::keymap::describe_rhs(&rhs)
            {
                sh_eprintln!(sh, "bish: ::bish map: '{rhs_text}' is {why} as a normal-mode action");
                sh_eprintln!(sh, "bish: ::bish map: if it was meant for another mode, scope it with -m");
                return 2;
            }
            match sh.mappings.iter_mut().find(|m| m.lhs == lhs && m.modes == mode) {
                Some(existing) => existing.rhs = rhs,
                None => sh.mappings.push(crate::keymap::Mapping { modes: mode, lhs, rhs }),
            }
            0
        }
    }
}

pub(crate) fn run_abbr(sh: &mut Shell, args: &[String]) -> i32 {
    enum Mode {
        Add,
        Erase,
        List,
        Show,
        Query,
    }
    let (args, lang) = snippet::take_lang_flag(args);
    let args: &[String] = &args;
    let (mode, rest) = match args.first().map(String::as_str) {
        Some("-a") | Some("--add") => (Mode::Add, &args[1..]),
        Some("-e") | Some("--erase") => (Mode::Erase, &args[1..]),
        Some("-l") | Some("--list") => (Mode::List, &args[1..]),
        Some("-s") | Some("--show") => (Mode::Show, &args[1..]),
        Some("-q") | Some("--query") => (Mode::Query, &args[1..]),
        None => (Mode::Show, args),
        Some(_) => (Mode::Add, args),
    };
    match mode {
        Mode::Add => {
            let Some((name, expansion_words)) = rest.split_first() else {
                sh_eprintln!(sh, "bish: abbr: -a: requires a NAME and an EXPANSION");
                return 2;
            };
            if expansion_words.is_empty() {
                sh_eprintln!(sh, "bish: abbr: -a: requires an EXPANSION for '{name}'");
                return 2;
            }
            let expansion = expansion_words.join(" ");
            let lang = lang.unwrap_or_else(|| snippet::DEFAULT_LANG.to_string());
            // Keyed on name and language: the same name under another
            // `--lang=` is a different abbreviation.
            match sh.abbrs.iter_mut().find(|a| a.name == *name && a.lang == lang) {
                Some(existing) => existing.expansion = expansion,
                None => sh.abbrs.push(Abbr { name: name.clone(), expansion, lang }),
            }
            0
        }
        Mode::Erase => {
            if rest.is_empty() {
                sh_eprintln!(sh, "bish: abbr: -e: requires a NAME");
                return 2;
            }
            let mut status = 0;
            for name in rest {
                // Without `--lang=`, erasing a name erases it in every
                // language; with one, only that entry.
                let before = sh.abbrs.len();
                sh.abbrs.retain(|a| a.name != *name || lang.as_ref().is_some_and(|l| a.lang != *l));
                if sh.abbrs.len() == before {
                    sh_eprintln!(sh, "bish: abbr: -e: {}: no such abbreviation", name);
                    status = 1;
                }
            }
            status
        }
        Mode::List => {
            for abbr in &sh.abbrs {
                sh_println!(sh, "{}", abbr.name);
            }
            0
        }
        Mode::Show => {
            for abbr in &sh.abbrs {
                // A non-default language is printed as `--lang=`, so
                // `abbr -s` output pastes back in.
                let lang = if abbr.lang == snippet::DEFAULT_LANG {
                    String::new()
                } else {
                    format!("--lang={} ", crate::serialize::quote_literal(&abbr.lang))
                };
                sh_println!(
                    sh,
                    "abbr -a {}{} {}",
                    lang,
                    crate::serialize::quote_literal(&abbr.name),
                    crate::serialize::quote_literal(&abbr.expansion)
                );
            }
            0
        }
        Mode::Query => {
            if rest.is_empty() {
                sh_eprintln!(sh, "bish: abbr: -q: requires at least one NAME");
                return 2;
            }
            // `--lang=` narrows to that language; without it any counts.
            let hit = |a: &Abbr, name: &String| a.name == *name && lang.as_ref().is_none_or(|l| a.lang == *l);
            if rest.iter().all(|name| sh.abbrs.iter().any(|a| hit(a, name))) { 0 } else { 1 }
        }
    }
}

// `window create [--name NAME] [--] [COMMAND...]`: an optional name and an
// optional command. `--name` takes everything to the end or to `--`, so
// `window create --name my project` names "my project"; without it, the
// arguments are the command.
fn parse_new_args(rest: &[String]) -> Result<(Option<String>, Option<String>), &'static str> {
    match rest.first().map(String::as_str) {
        None => Ok((None, None)),
        Some("--") => Ok((None, joined(&rest[1..]))),
        Some("--name") | Some("-n") => match rest[1..].iter().position(|a| a == "--") {
            Some(at) => match joined(&rest[1..1 + at]) {
                Some(name) => Ok((Some(name), joined(&rest[2 + at..]))),
                None => Err("--name needs a name"),
            },
            None => match joined(&rest[1..]) {
                Some(name) => Ok((Some(name), None)),
                None => Err("--name needs a name"),
            },
        },
        Some(_) => Ok((None, joined(rest))),
    }
}

// `window split`/`vsplit [--] [COMMAND...]`. `--` lets a command start
// with a dash.
fn parse_split_command(rest: &[String]) -> Option<String> {
    match rest.first().map(String::as_str) {
        Some("--") => joined(&rest[1..]),
        _ => joined(rest),
    }
}

// Arguments back into one command line, as `window rename` joins a name.
// Quoting is lost (`split echo "a  b"` runs `echo a b`); quote the whole
// command to keep it.
fn joined(parts: &[String]) -> Option<String> {
    match parts.is_empty() {
        true => None,
        false => Some(parts.join(" ")),
    }
}

// `::bish window ...` and command mode's `window`. The read-only
// subcommands answer here; the rest return an `ExecResult::Window` for
// repl.rs, but only where something will act on it: `run_program` stops
// on that signal, so one escaping from `bish script.sh` would end the
// script.
pub(crate) fn run_window(sh: &mut Shell, args: &[String]) -> ExecResult {
    let result = run_window_inner(sh, args);
    if matches!(result, ExecResult::Window(_)) && !sh.windows_available && !sh.restrict_to_builtins {
        sh_eprintln!(sh, "bish: ::bish window: no window manager here (this needs an interactive bish)");
        return ExecResult::Status(1);
    }
    result
}

pub(crate) fn run_window_inner(sh: &mut Shell, args: &[String]) -> ExecResult {
    sh.promote_if_needed();
    match args.first().map(String::as_str) {
        Some("next") | Some("n") => ExecResult::Window(WindowAction::Next),
        Some("previous") | Some("prev") | Some("p") => ExecResult::Window(WindowAction::Previous),
        Some("new") | Some("c") | Some("create") => match parse_new_args(&args[1..]) {
            Ok((name, command)) => ExecResult::Window(WindowAction::New { name, command }),
            Err(msg) => {
                sh_eprintln!(sh, "bish: window: create: {msg}");
                ExecResult::Status(2)
            }
        },
        Some("rename") | Some("ren") => match args.get(1) {
            // A bare `window rename` clears the name; otherwise the words
            // are joined.
            None => ExecResult::Window(WindowAction::Rename(None)),
            Some(_) => ExecResult::Window(WindowAction::Rename(Some(args[1..].join(" ")))),
        },
        // The two read-only subcommands answer from `Shell::windows` like
        // any builtin: `$(window ls)` captures, and `select` fails
        // synchronously, so `select || create` works anywhere.
        Some("ls") | Some("list") => {
            for w in &sh.windows.clone() {
                let name = w.name.clone().unwrap_or_default();
                let current = if w.current { "*" } else { "" };
                sh_println!(sh, "{}\t{name}\t{}\t{}\t{current}", w.id, w.cwd, w.panes);
            }
            ExecResult::Status(0)
        }
        Some("select") | Some("sel") => match args.get(1) {
            Some(target) => {
                // By name first, then by id.
                let found = sh
                    .windows
                    .iter()
                    .position(|w| w.name.as_deref() == Some(target.as_str()))
                    .or_else(|| sh.windows.iter().position(|w| w.id.to_string() == *target));
                match found {
                    Some(index) => ExecResult::Window(WindowAction::Select(index)),
                    None => {
                        sh_eprintln!(sh, "bish: window: select: no window named '{target}'");
                        ExecResult::Status(1)
                    }
                }
            }
            None => {
                sh_eprintln!(sh, "bish: window: select: usage: window select <name>|<id>");
                ExecResult::Status(2)
            }
        },
        Some("close") | Some("q") | Some("quit") => ExecResult::Window(WindowAction::Close),
        // `WindowAction::Split`'s `horizontal` is the divider line's
        // orientation, as in vim's `:split`. People name a split by how the
        // panes are arranged, the opposite pairing, so `split` (side by
        // side) is `horizontal: false` and `vsplit` (stacked) is
        // `horizontal: true`.
        Some("split") | Some("s") => ExecResult::Window(WindowAction::Split { horizontal: false, command: parse_split_command(&args[1..]) }),
        Some("vsplit") | Some("v") => ExecResult::Window(WindowAction::Split { horizontal: true, command: parse_split_command(&args[1..]) }),
        Some("h") | Some("left") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Left)),
        Some("j") | Some("below") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Down)),
        Some("k") | Some("above") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Up)),
        Some("l") | Some("right") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Right)),
        Some("zoom") | Some("z") => ExecResult::Window(WindowAction::Zoom),
        Some("focus") => match args.get(1).and_then(|a| a.parse::<usize>().ok()) {
            Some(position) if position >= 1 => ExecResult::Window(WindowAction::FocusPosition(position)),
            _ => {
                sh_eprintln!(sh, "bish: window: focus: usage: window focus <n> (panes count from 1)");
                ExecResult::Status(2)
            }
        },
        Some("save") => ExecResult::Window(WindowAction::Save(args.get(1).cloned())),
        Some("restore") => match args.get(1) {
            Some(path) => ExecResult::Window(WindowAction::Restore(path.clone())),
            None => {
                sh_eprintln!(sh, "bish: window: restore: usage: window restore <file>");
                ExecResult::Status(2)
            }
        },
        Some("move") | Some("mv") => match args.get(1).map(String::as_str) {
            Some("left") | Some("l") => ExecResult::Window(WindowAction::MoveWindow(WindowMove::Left)),
            Some("right") | Some("r") => ExecResult::Window(WindowAction::MoveWindow(WindowMove::Right)),
            Some(n) => match n.parse::<usize>() {
                // 1-based, as a tab bar is counted: `move 1` makes this the
                // first tab.
                Ok(position) if position >= 1 => ExecResult::Window(WindowAction::MoveWindow(WindowMove::To(position))),
                _ => {
                    sh_eprintln!(sh, "bish: window: move: usage: window move <left|right|N> (N counts from 1)");
                    ExecResult::Status(2)
                }
            },
            None => {
                sh_eprintln!(sh, "bish: window: move: usage: window move <left|right|N>");
                ExecResult::Status(2)
            }
        },
        Some("layout") => match args.get(1).map(String::as_str) {
            // No name cycles too, so the command works before the names are
            // learned.
            None | Some("next") => ExecResult::Window(WindowAction::SetLayout(None)),
            Some(name) => match crate::window::NamedLayout::parse(name) {
                Some(layout) => ExecResult::Window(WindowAction::SetLayout(Some(layout))),
                None => {
                    let names: Vec<&str> = crate::window::NamedLayout::ALL.iter().map(|l| l.name()).collect();
                    let hint = crate::suggest::did_you_mean(name, names.iter().copied());
                    sh_eprintln!(sh, "bish: window: layout: unknown layout: {name}{hint}");
                    sh_eprintln!(sh, "bish: window: layout: known layouts: {}", listed(&names));
                    ExecResult::Status(2)
                }
            },
        },
        Some("swap") | Some("x") => ExecResult::Window(WindowAction::SwapPane),
        Some("break") => ExecResult::Window(WindowAction::BreakPane),
        // `join` puts the pane beside this one and `vjoin` stacks it
        // beneath, as `split`/`vsplit` do.
        Some(sub @ ("join" | "vjoin")) => match args.get(1).and_then(|a| a.parse::<u32>().ok()) {
            Some(from) => ExecResult::Window(WindowAction::JoinPane { from, horizontal: sub == "vjoin" }),
            None => {
                sh_eprintln!(sh, "bish: window: {sub}: usage: window {sub} <window-id>");
                ExecResult::Status(2)
            }
        },
        Some("=") | Some("balance") => ExecResult::Window(WindowAction::Balance),
        Some("_") | Some("minimize") => ExecResult::Window(WindowAction::Minimize),
        Some("+") | Some("sizeup") => ExecResult::Window(WindowAction::SizeUp),
        Some("-") | Some("sizedown") => ExecResult::Window(WindowAction::SizeDown),
        Some("size") => match args.get(1).and_then(|a| parse_size_spec(a)) {
            Some(spec) => ExecResult::Window(WindowAction::SetSize(spec)),
            None => {
                sh_eprintln!(sh, "bish: window: size: usage: window size <N>|<N>%|<N>/<M>");
                ExecResult::Status(2)
            }
        },
        Some("fg") => match args.get(1).and_then(|a| a.parse::<u32>().ok()) {
            Some(id) => ExecResult::Window(WindowAction::FgSession(id)),
            None => {
                sh_eprintln!(sh, "bish: window: fg: usage: window fg <window-id>");
                ExecResult::Status(2)
            }
        },
        Some(other) => {
            let hint = crate::suggest::did_you_mean(other, WINDOW_SUBCOMMANDS.iter().copied());
            sh_eprintln!(sh, "bish: window: unknown subcommand: {other}{hint}");
            ExecResult::Status(2)
        }
        None => {
            sh_eprintln!(
                sh,
                "bish: window: missing subcommand (next(n)/previous/new(c,create) [--name NAME] [-- CMD]/close(q,quit)/split(s) [CMD]/vsplit(v) [CMD]/h(left)/j(below)/k(above)/l(right)/zoom(z)/swap(x)/break/join <id>/vjoin <id>/layout [name|next]/move <left|right|N>/focus <n>/save [file]/restore <file>/=(balance)/_(minimize)/+(sizeup)/-(sizedown)/size <N|N%,N/M>/fg <id>)"
            );
            ExecResult::Status(2)
        }
    }
}

// `::bish theme list`: the names `bishopt --set theme` accepts, shipped
// and declared together, with the active one marked. A declared theme
// with a shipped theme's name appears once; the shipped colours it doesn't
// set still show through (see `Shell::bishopt_value`).
pub(crate) fn run_bish_theme_list(sh: &mut Shell) -> i32 {
    // Empty when no theme is active, which matches none of these.
    let active = sh.bishopt_str("theme");
    let mut declared: Vec<&String> = sh.themes.keys().collect();
    crate::sort::sort(&mut declared);
    let mut rows: Vec<(String, String)> = Vec::new();
    for theme in crate::theme::BUILTIN_THEMES {
        let about = match declared.iter().any(|n| n.as_str() == theme.name) {
            true => format!("declared, over the one bish ships -- {}", theme.about),
            false => theme.about.to_string(),
        };
        rows.push((theme.name.to_string(), about));
    }
    for name in declared {
        if crate::theme::builtin(name).is_none() {
            rows.push((name.clone(), "declared".to_string()));
        }
    }
    for (name, about) in rows {
        let mark = if name == active { "*" } else { " " };
        sh_println!(sh, "{mark} {name}\t{about}");
    }
    0
}

// Starts a theme declaration: every `bishopt --set` until `::bish theme
// end` goes into `pending_theme` instead of applying (see
// `store_bishopt`). Doesn't nest: a theme is a flat set of options, and a
// nested `begin` would lose what the outer one captured.
pub(crate) fn run_bish_theme_begin(sh: &mut Shell) -> i32 {
    if sh.pending_theme.is_some() {
        sh_eprintln!(sh, "bish: ::bish theme: a theme declaration is already in progress -- `::bish theme end` it first");
        return 1;
    }
    sh.pending_theme = Some(Theme::default());
    0
}

// Ends a theme declaration. The `theme` entry captured inside it (from
// `bishopt --set theme NAME`) names the theme the rest are registered
// under, and is removed so a theme doesn't name itself. Without one, the
// batch is discarded. Registering never activates the theme; that's a
// separate `bishopt --set theme NAME`.
pub(crate) fn run_bish_theme_end(sh: &mut Shell) -> i32 {
    let Some(mut pending) = sh.pending_theme.take() else {
        sh_eprintln!(sh, "bish: ::bish theme: no theme declaration in progress");
        return 1;
    };
    let Some(BishOptValue::Str(name)) = pending.opts.remove("theme") else {
        return 0;
    };
    sh.themes.insert(name, pending);
    0
}

#[cfg(test)]
mod window_argument_tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn a_bare_argument_is_the_command_to_run() {
        assert_eq!(parse_new_args(&args("make -j8")), Ok((None, Some("make -j8".to_string()))));
        assert_eq!(parse_split_command(&args("make -j8")), Some("make -j8".to_string()));
    }

    #[test]
    fn nothing_at_all_is_still_a_plain_new_window() {
        assert_eq!(parse_new_args(&[]), Ok((None, None)));
        assert_eq!(parse_split_command(&[]), None);
    }

    #[test]
    fn name_stays_greedy_so_an_unquoted_name_still_works() {
        // `--name` still takes the rest, so existing scripts keep naming
        // windows.
        assert_eq!(parse_new_args(&args("--name my project")), Ok((Some("my project".to_string()), None)));
    }

    #[test]
    fn a_double_dash_is_what_ends_the_name() {
        assert_eq!(parse_new_args(&args("--name build -- make -j8")), Ok((Some("build".to_string()), Some("make -j8".to_string()))));
        assert_eq!(parse_new_args(&args("-n build -- make")), Ok((Some("build".to_string()), Some("make".to_string()))));
    }

    #[test]
    fn a_leading_double_dash_means_the_rest_is_all_command() {
        // A command starting with a dash, after `--`.
        assert_eq!(parse_new_args(&args("-- ls -d /etc")), Ok((None, Some("ls -d /etc".to_string()))));
        assert_eq!(parse_split_command(&args("-- ls -d /etc")), Some("ls -d /etc".to_string()));
    }

    #[test]
    fn a_name_flag_with_no_name_is_an_error() {
        assert_eq!(parse_new_args(&args("--name")), Err("--name needs a name"));
        assert_eq!(parse_new_args(&args("--name -- make")), Err("--name needs a name"));
    }
}

#[cfg(test)]
mod trust_command_tests {
    use super::*;
    use crate::exec::Shell;

    fn strs(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    // The builtin over a store of its own, keyed off the environment, so it
    // serializes on its own lock and restores what it changed (as
    // session.rs's env tests do).
    fn with_shell(body: impl FnOnce(&mut Shell, &std::path::Path)) {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let cfg = crate::tempdir::TempDir::new("trust-cmd");
        let work = crate::tempdir::TempDir::new("trust-work");
        let (saved_xdg, saved_home) = (std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("HOME"));
        // SAFETY: under the lock, restored below.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", cfg.path());
            std::env::remove_var("HOME");
        }
        let mut shell = Shell::new();
        shell.cwd = work.path().to_path_buf();
        body(&mut shell, work.path());
        unsafe {
            match saved_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match saved_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    // `--check` is the hook contract: silent, the exit status the answer.
    #[test]
    fn check_is_a_silent_predicate_over_the_exit_status() {
        with_shell(|sh, work| {
            std::fs::create_dir_all(work.join("sub")).unwrap();
            // Nothing trusted yet.
            assert_eq!(run_trust(sh, &strs(&["--check", "git"])), 1);
            // The default form trusts `.` (bish's cwd) for everything.
            assert_eq!(run_trust(sh, &[]), 0);
            assert_eq!(run_trust(sh, &strs(&["--check", "git"])), 0);
            assert_eq!(run_trust(sh, &strs(&["--check", "anything"])), 0);
            // And it descends: a relative subdirectory resolves against cwd.
            assert_eq!(run_trust(sh, &strs(&["--check", "git", "sub"])), 0);
        });
    }

    // A capability glob narrows a trust. Shown on a directory with no
    // broader ancestor, since one trusted for `*` would cover everything
    // below.
    #[test]
    fn a_capability_glob_scopes_what_the_trust_covers() {
        with_shell(|sh, work| {
            std::fs::create_dir_all(work.join("tool")).unwrap();
            assert_eq!(run_trust(sh, &strs(&["--capabilities=mise:*", "tool"])), 0);
            assert_eq!(run_trust(sh, &strs(&["--check", "mise:env", "tool"])), 0);
            assert_eq!(run_trust(sh, &strs(&["--check", "git", "tool"])), 1, "mise:* does not match git");
        });
    }

    #[test]
    fn trusting_a_nonexistent_directory_is_refused() {
        with_shell(|sh, _work| {
            assert_eq!(run_trust(sh, &strs(&["does-not-exist"])), 1);
            // And nothing was recorded.
            assert_eq!(run_trust(sh, &strs(&["--check", "git", "does-not-exist"])), 1);
        });
    }
}
