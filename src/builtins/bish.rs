// `::bish` and its subcommands, plus `bishopt` and `abbr`: everything
// bish has that bash does not.
//
// Free functions taking `&mut Shell` -- see `builtins/mod.rs`.

use crate::bishedit::snippet::{self, Abbr};
use crate::exec::{
    BishOptDefault, BishOptValue, ExecResult, HOOK_EVENTS, Hook, LspServer, PaneDirection, Shell, Theme, WindowAction, hook_help, lsp_help,
    parse_size_spec, sh_eprintln, sh_println,
};

// The subcommand names each `::bish` family answers to. One list per
// family, used both for the "expected:" line and for the "did you
// mean" beside it -- so a name added to a `match` below and forgotten
// here shows up as a wrong error message rather than as a silently
// unsuggestable command.
const BISH_SUBCOMMANDS: &[&str] = &["theme", "window", "hook", "hl", "lsp", "map"];
const HOOK_SUBCOMMANDS: &[&str] = &["ls", "add", "rm", "help"];
const LSP_SUBCOMMANDS: &[&str] = &["ls", "add", "rm", "status", "log", "restart", "help"];
const THEME_SUBCOMMANDS: &[&str] = &["begin", "end"];
// Long forms only. The one-letter aliases (`s`, `v`, `h`, `=`) are real
// spellings but useless as suggestions: at a one-edit threshold every
// mistyped single character is a near miss for most of them, so the
// answer would be arbitrary.
const WINDOW_SUBCOMMANDS: &[&str] = &[
    "next", "previous", "new", "create", "close", "quit", "split", "vsplit", "left", "below", "above", "right", "balance", "minimize", "sizeup",
    "sizedown", "size", "fg",
];

// "theme, window, hook, hl, lsp, map" -- the list as an error message
// says it.
fn listed(names: &[&str]) -> String {
    names.join(", ")
}

pub(crate) fn run_bishopt(sh: &mut Shell, args: &[String], registry: &[(&str, BishOptDefault)]) -> i32 {
    enum Mode<'a> {
        List,
        Get(&'a str, bool), // bool: quiet
        // `--describe [NAME]`: what an option is for, what it
        // accepts, and what it is set to. Everything `bishopt` could
        // already tell you was the *value*; this is the half that
        // makes an option findable rather than merely settable.
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

// `::bish SUBCOMMAND...`: a small namespace of its own for bish-
// specific commands (`theme begin`/`theme end` today) that don't
// read naturally as an ordinary top-level builtin name -- `theme` on
// its own would either collide with a real bash script's own
// variable/function of that name, or need its own awkward "begin"/
// "end" builtins polluting the global command namespace for
// something this narrow. `::` is never a valid start of an ordinary
// bash command word in practice, so `::bish` reads unambiguously as
// "this is bish's own thing," the same spirit as `set -o` bundling
// bash's own less-common toggles under one name instead of each
// getting its own builtin.
pub(crate) fn run_bish(sh: &mut Shell, args: &[String]) -> ExecResult {
    match args {
        [sub, rest @ ..] if sub == "theme" => ExecResult::Status(run_bish_theme(sh, rest)),
        // The canonical spelling of the window manager. `window`/
        // `win` survive only as *command-mode* aliases (see
        // run_single's own arm): a top-level builtin called `window`
        // shadows any real `window` on `$PATH` for every script that
        // runs under bish, and this namespace exists precisely for
        // bish-specific commands that shouldn't spend a common word.
        [sub, rest @ ..] if sub == "window" || sub == "win" => run_window(sh, rest),
        [sub, rest @ ..] if sub == "hook" => ExecResult::Status(run_hook(sh, rest)),
        [sub, rest @ ..] if sub == "hl" => ExecResult::Status(run_hl(sh, rest)),
        [sub, rest @ ..] if sub == "lsp" => ExecResult::Status(run_lsp(sh, rest)),
        [sub, rest @ ..] if sub == "map" => ExecResult::Status(run_map(sh, rest)),
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
                // Listing by language asks "what would fire for a
                // file of this language", so it matches the *glob*
                // against the language given, exactly as firing
                // does -- not the two globs against each other.
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
            // The id is the return value: a config that adds a hook
            // is usually the thing that will want to remove it.
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

// `::bish lsp ls|add|rm|status` -- which language servers exist and
// which are running. Deliberately the same shape as `::bish hook`
// right above: a per-shell counter for ids, `--lang` as a glob, `rm`
// by the id `add` printed. Two registries that worked differently
// would be two things to learn.
//
// Canonical under `::bish` rather than a bare `lsp` builtin, for the
// reason `window` was moved there: this namespace exists so
// bish-specific commands don't shadow real ones in scripts.
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
                // Same question `hook ls --lang` answers: "what
                // would be used for a file of this language" -- so
                // the glob is matched against the language given,
                // not the two globs against each other.
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
            // Round-robin rather than a fixed order: with four
            // flags, an order that only reads correctly one way
            // means `--apply-edits=always --lang=rust rust-analyzer`
            // silently tries to *run* `--apply-edits=always`. Each
            // helper leaves the slice alone when its flag isn't at
            // the front, so a pass that consumes nothing is the
            // signal that what remains is the command.
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
                            // Last one wins for a repeated key, so a
                            // config that overrides an earlier line
                            // reads the way it looks.
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
            // The id is the return value, same as `hook add`: a
            // config that registers something usually wants to be
            // able to take it back.
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
            // Collected before printing: `sh_println!` needs the
            // shell mutably, and the table is reached through it.
            let rows: Vec<String> = sh.lsp.borrow().rows().iter().map(|fields| fields.join("\t")).collect();
            for row in rows {
                sh_println!(sh, "{row}");
            }
            0
        }
        Some("log") => {
            // The whole reason a server's stderr is captured rather
            // than discarded: when one fails to start, what it said
            // on the way out is the only explanation there is.
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
            // A server that died, or never started, stays that way
            // on purpose -- retrying on every file open would turn
            // one bad line of config into a spawn per keystroke of
            // navigation. This is how someone who has *fixed* that
            // line says so.
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

// `::bish hl` -- the syntax-highlighting palette.
//
// Shaped like `bishopt` (`--set`, `--unset`, a bare name to read,
// nothing to list) because it does the same job, and two commands
// that behave differently for no reason are two things to learn.
// It is a *separate* command because the names are open: bishopt is
// a closed registry with a default and a description for each
// entry, and a highlight colour cannot be, since a language
// server's semantic token types are not knowable in advance.
//
// Only colours. The chrome colours (`ui_col_*`) stay bishopts --
// those really are a fixed set of things bish draws.
pub(crate) fn run_hl(sh: &mut Shell, args: &[String]) -> i32 {
    match args {
        [] => {
            for (name, value) in sh.hl_colors() {
                sh_println!(sh, "{name}\t{value}");
            }
            0
        }
        [flag, name] if flag == "--unset" || flag == "-u" => {
            // Live state even mid-declaration, exactly as
            // `bishopt --unset` is: unsetting is not declaring.
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
                // Nothing said about this name, which is not an
                // error: an open namespace has no unknown names,
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

// `::bish map [-m GLOB] LHS RHS` -- remap a key sequence.
//
// Always non-recursive, the way vim's `noremap` is, and there is no
// recursive form: a mapping's right-hand side is resolved against
// the *default* bindings and can never chain through another
// mapping. That removes the whole class of surprise vim's `map` is
// famous for (a mapping that changes meaning because an unrelated
// one was defined later), and it is why a listing can name the
// action a mapping performs rather than just echoing keys.
//
// The flags follow `abbr`'s, not `::bish lsp`'s: this is a small
// user table with add/erase/list, the same shape abbreviations
// already have, rather than a subcommand family.
pub(crate) fn run_map(sh: &mut Shell, args: &[String]) -> i32 {
    let (args, mode) = crate::keymap::take_mode_flag(args);
    let mode = mode.unwrap_or_else(|| crate::keymap::DEFAULT_MODE.to_string());
    if !crate::keymap::mode_glob_is_known(&mode) {
        // A glob matching no mode would be stored, listed, and never
        // fire, with nothing anywhere saying why -- so it is refused
        // where the typo was made.
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
                    // The fourth column is what the mapping *does*,
                    // which only exists where keys resolve to named
                    // actions. For an insert/command/terminal-only
                    // mapping the keys are the whole story, and a
                    // dash says so rather than inventing a
                    // normal-mode reading of them.
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
                // Two halves, always. A right-hand side split over
                // several words would be ambiguous about whether the
                // space is a key or a separator, which is the exact
                // ambiguity `<Space>` exists to remove.
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
            // Resolved here rather than at the keystroke: a
            // right-hand side that means nothing should fail where
            // it was written, not silently swallow keys later.
            //
            // Only where it *has* a meaning to check, though. Normal
            // and visual resolve keys to named actions, so a
            // right-hand side that resolves to nothing there is a
            // mistake. Insert, command and terminal have no such
            // vocabulary -- keys are keys -- and `<Esc>` is a
            // perfectly good insert-mode right-hand side while being
            // no normal-mode action at all.
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
            // Redefinition is keyed on both name *and* language: the
            // same name under a different `--lang=` is a different
            // abbreviation, not a replacement for this one.
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
                // With no `--lang=`, erasing a name erases it in
                // every language it was defined for -- "get rid of
                // `foo`" is what someone typing that means. With one,
                // only the exact `(name, lang)` entry goes.
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
                // A non-default language is printed back as the same
                // `--lang=`, so `abbr -s` stays something you can
                // paste straight back in.
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
            // `--lang=` narrows the question to that one language;
            // without it, any language counts.
            let hit = |a: &Abbr, name: &String| a.name == *name && lang.as_ref().is_none_or(|l| a.lang == *l);
            if rest.iter().all(|name| sh.abbrs.iter().any(|a| hit(a, name))) { 0 } else { 1 }
        }
    }
}

// `window`/`w`/`win` -- the window-manager builtin. Only validates the
// subcommand and triggers promotion; the actual session/window
// mutation happens in repl.rs, reached via the bubbled-up
// ExecResult::Window signal (see that variant's doc comment for why
// this can't just mutate shared state directly from here).
// `::bish window ...` (and command mode's `window` alias). The
// read-only subcommands answer from `Shell::windows` right here; the
// rest bubble an action up to repl.rs -- and only where something is
// actually there to act on it, since `ExecResult::Window` is a
// signal `run_program` stops on and letting one escape from `bish
// script.sh` would end the script for no reason.
pub(crate) fn run_window(sh: &mut Shell, args: &[String]) -> ExecResult {
    let result = run_window_inner(sh, args);
    if matches!(result, ExecResult::Window(_)) && !sh.windows_available && !sh.restrict_to_builtins {
        sh_eprintln!(sh, "bish: ::bish window: no window manager here (this needs an interactive bish)");
        return ExecResult::Status(1);
    }
    result
}

pub(crate) fn run_window_inner(sh: &mut Shell, args: &[String]) -> ExecResult {
    fn parse_window_name(shell: &mut Shell, subcommand: &str, rest: &[String]) -> Result<Option<String>, i32> {
        match rest.first().map(String::as_str) {
            None => Ok(None),
            Some("--name") | Some("-n") => match rest.len() {
                1 => {
                    sh_eprintln!(shell, "bish: window: {subcommand}: --name needs a name");
                    Err(2)
                }
                _ => Ok(Some(rest[1..].join(" "))),
            },
            Some(other) => {
                sh_eprintln!(shell, "bish: window: {subcommand}: unexpected argument '{other}' (expected --name NAME)");
                Err(2)
            }
        }
    }

    sh.promote_if_needed();
    match args.first().map(String::as_str) {
        Some("next") | Some("n") => ExecResult::Window(WindowAction::Next),
        Some("previous") | Some("prev") | Some("p") => ExecResult::Window(WindowAction::Previous),
        Some("new") | Some("c") | Some("create") => match parse_window_name(sh, "create", &args[1..]) {
            Ok(name) => ExecResult::Window(WindowAction::New { name }),
            Err(status) => ExecResult::Status(status),
        },
        Some("rename") | Some("ren") => match args.get(1) {
            // A bare `window rename` clears the name; anything else
            // is the new one, joined so `window rename my project`
            // means what it looks like.
            None => ExecResult::Window(WindowAction::Rename(None)),
            Some(_) => ExecResult::Window(WindowAction::Rename(Some(args[1..].join(" ")))),
        },
        // The two that only *read*. Both answer from
        // `Shell::windows`, which means both behave like any other
        // builtin: `ls` writes to whatever sink it has (so
        // `$(window ls)` captures it) and `select` fails
        // synchronously (so `select || create` works in a function,
        // a subshell or an `if`).
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
                // A name first, an id second: a name is what a config
                // function knows, an id is what it falls back to.
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
        // WindowAction::Split's own `horizontal` names the divider
        // LINE's orientation (true = a horizontal dividing line,
        // panes stacked top/bottom), matching vim's :split/:vsplit
        // convention. Users read "vertical"/"horizontal" by the
        // panes' own arrangement axis instead (stacked = panes
        // arranged *vertically*, side by side = arranged
        // *horizontally*) -- the opposite pairing -- so `split`/`s`
        // maps to horizontal:false (side by side) and `vsplit`/`v`
        // to horizontal:true (stacked), even though that looks
        // inverted next to the field's own name.
        Some("split") | Some("s") => ExecResult::Window(WindowAction::Split { horizontal: false }),
        Some("vsplit") | Some("v") => ExecResult::Window(WindowAction::Split { horizontal: true }),
        Some("h") | Some("left") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Left)),
        Some("j") | Some("below") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Down)),
        Some("k") | Some("above") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Up)),
        Some("l") | Some("right") => ExecResult::Window(WindowAction::FocusPane(PaneDirection::Right)),
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
                "bish: window: missing subcommand (next(n)/previous/new(c,create)/close(q,quit)/split(s)/vsplit(v)/h(left)/j(below)/k(above)/l(right)/=(balance)/_(minimize)/+(sizeup)/-(sizedown)/size <N|N%,N/M>/fg <id>)"
            );
            ExecResult::Status(2)
        }
    }
}

// Starts a new theme declaration -- every `bishopt --set` from here
// until the matching `::bish theme end` is captured into
// `pending_theme` instead of applying live (see store_bishopt's own
// doc comment). Refuses to nest: a `begin` while one is already in
// progress would otherwise silently discard whatever the outer one
// had captured so far the moment `end` ran, with no way back --
// there's no real use for nesting this anyway (a theme is a flat set
// of opts, not something that composes from an inner declaration).
pub(crate) fn run_bish_theme_begin(sh: &mut Shell) -> i32 {
    if sh.pending_theme.is_some() {
        sh_eprintln!(sh, "bish: ::bish theme: a theme declaration is already in progress -- `::bish theme end` it first");
        return 1;
    }
    sh.pending_theme = Some(Theme::default());
    0
}

// Ends the current theme declaration. The captured "theme" entry (if
// any -- set via an ordinary `bishopt --set theme NAME` *inside* the
// declaration, which store_bishopt diverted here instead of applying
// live) names which entry of `sh.themes` the rest of the captured
// opts get registered under; it's removed from that captured map
// first so a theme's own opts never include a "theme" entry pointing
// at itself. If "theme" was never set during the declaration, there's
// no name to register anything under -- the whole batch is just
// discarded, matching "theme behaves unset until explicitly declared
// inside a theme declaration" (declaring opts with no name doesn't
// retroactively give them one). Registering a theme here never
// switches to it -- that still needs its own ordinary `bishopt --set
// theme NAME` afterward, outside any declaration, the same way
// defining a theme and activating one are two separate, deliberate
// steps.
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
