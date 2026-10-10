// The engine behind `compgen`, `complete` and `compopt` and behind Tab
// completion. It doesn't depend on `Shell`, so both the builtins (with a
// live shell) and the editor (with a per-prompt snapshot; see
// `ShellCompletionProvider`) run the same logic on what they have.
//
// The behaviour (which sources the current word filters, the `-F`/`-C`
// calling convention, inert `-o` names, `complete -p`'s print order) was
// worked out by running bash side by side.
//
// `-F` and `-C` run in a subprocess (this binary with `-c` and a preamble
// of the caller's functions and variables; see `functions_preamble`), not
// in-process as in bash, so a completion function's side effects on the
// shell don't persist. The editor's path only has a snapshot, so this
// lets both share one resolver; COMPREPLY is what matters in practice.
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompgenAction {
    Alias,
    ArrayVar,
    Binding,
    Builtin,
    Command,
    Directory,
    Disabled,
    Enabled,
    Export,
    File,
    Function,
    Group,
    HelpTopic,
    Hostname,
    Job,
    Keyword,
    Running,
    Service,
    Setopt,
    Shopt,
    Signal,
    Stopped,
    User,
    Variable,
}

pub fn action_by_name(name: &str) -> Option<CompgenAction> {
    use CompgenAction::*;
    Some(match name {
        "alias" => Alias,
        "arrayvar" => ArrayVar,
        "binding" => Binding,
        "builtin" => Builtin,
        "command" => Command,
        "directory" => Directory,
        "disabled" => Disabled,
        "enabled" => Enabled,
        "export" => Export,
        "file" => File,
        "function" => Function,
        "group" => Group,
        "helptopic" => HelpTopic,
        "hostname" => Hostname,
        "job" => Job,
        "keyword" => Keyword,
        "running" => Running,
        "service" => Service,
        "setopt" => Setopt,
        "shopt" => Shopt,
        "signal" => Signal,
        "stopped" => Stopped,
        "user" => User,
        "variable" => Variable,
        _ => return None,
    })
}

// An action's long name, for `-p` when it has no one-letter flag.
pub fn action_name(action: CompgenAction) -> &'static str {
    use CompgenAction::*;
    match action {
        Alias => "alias",
        ArrayVar => "arrayvar",
        Binding => "binding",
        Builtin => "builtin",
        Command => "command",
        Directory => "directory",
        Disabled => "disabled",
        Enabled => "enabled",
        Export => "export",
        File => "file",
        Function => "function",
        Group => "group",
        HelpTopic => "helptopic",
        Hostname => "hostname",
        Job => "job",
        Keyword => "keyword",
        Running => "running",
        Service => "service",
        Setopt => "setopt",
        Shopt => "shopt",
        Signal => "signal",
        Stopped => "stopped",
        User => "user",
        Variable => "variable",
    }
}

// The one-letter flags in the usage (`[-abcdefgjksuv]`), a subset of the
// actions: arrayvar, binding, disabled, enabled, helptopic, hostname,
// running, setopt, shopt, signal and stopped have none.
pub fn action_from_flag(c: char) -> Option<CompgenAction> {
    use CompgenAction::*;
    Some(match c {
        'a' => Alias,
        'b' => Builtin,
        'c' => Command,
        'd' => Directory,
        'e' => Export,
        'f' => File,
        'g' => Group,
        'j' => Job,
        'k' => Keyword,
        's' => Service,
        'u' => User,
        'v' => Variable,
        _ => return None,
    })
}

pub fn action_flag_char(action: CompgenAction) -> Option<char> {
    use CompgenAction::*;
    Some(match action {
        Alias => 'a',
        Builtin => 'b',
        Command => 'c',
        Directory => 'd',
        Export => 'e',
        File => 'f',
        Group => 'g',
        Job => 'j',
        Keyword => 'k',
        Service => 's',
        User => 'u',
        Variable => 'v',
        _ => return None,
    })
}

// The `-o` names: an unknown one is a usage error, as in bash, but
// otherwise they're inert here, since each only changes readline's
// behaviour, never the generated text (`compgen -o filenames` adds no
// slash).
pub const O_OPTIONS: &[&str] = &["bashdefault", "default", "dirnames", "filenames", "noquote", "nosort", "nospace", "plusdirs"];

// Every reserved word: the lexer's keyword tokens, the brace and
// double-bracket pairs, and `!` and `time`, which the pipeline rule
// reserves. Used by `compgen -A keyword` and by `type`.
pub const KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac", "select", "function", "coproc", "in", "{", "}", "[[",
    "]]", "!", "time",
];

// The live shell state the contextual actions need, built from a `Shell`
// (`Shell::action_context`) or a per-prompt snapshot (repl.rs), so this
// module never reaches into `Shell`. Owned data, built once per use.
#[derive(Clone, Default)]
pub struct ActionContext {
    pub aliases: Vec<String>,
    pub functions: Vec<String>,
    // Indexed and associative array names together; ArrayVar and Variable
    // don't distinguish them.
    pub arrays: Vec<String>,
    pub exported: Vec<String>,
    // The names a variable lookup can see, without arrays (the builder adds
    // those with `arrays`).
    pub variables: Vec<String>,
    pub builtins: Vec<String>,
    pub shopt_names: Vec<String>,
    pub set_o_names: Vec<String>,
    // Signal names without `SIG` (`HUP`); `resolve_action` adds the prefix
    // and EXIT.
    pub signal_names: Vec<String>,
    pub jobs: Vec<String>,
    pub running_jobs: Vec<String>,
    pub stopped_jobs: Vec<String>,
    // Every PATH executable, unfiltered: `resolve_spec` applies the word
    // filter anyway.
    pub path_commands: Vec<String>,
}

// `-f`/`-d`: only the last path segment of `word` is completed, the
// directory part staying a literal prefix, as in Tab completion. Unlike
// Tab completion, and as bash's `-f`/`-d` do, dotfiles aren't hidden and
// directories get no `/`.
pub fn path_entries(cwd: &Path, word: &str, dirs_only: bool) -> Vec<String> {
    let dir_part = match word.rfind('/') {
        Some(idx) => &word[..idx + 1],
        None => "",
    };
    let dir_path = if let Some(rest) = dir_part.strip_prefix('~') {
        let Ok(home) = std::env::var("HOME") else { return Vec::new() };
        std::path::PathBuf::from(home).join(rest.trim_start_matches('/'))
    } else if dir_part.is_empty() {
        cwd.to_path_buf()
    } else if Path::new(dir_part).is_absolute() {
        std::path::PathBuf::from(dir_part)
    } else {
        cwd.join(dir_part)
    };
    let Ok(entries) = std::fs::read_dir(&dir_path) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if dirs_only && !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                return None;
            }
            Some(format!("{dir_part}{name}"))
        })
        .collect()
}

// `-A group`/`-A user`: the first field of each line of /etc/group or
// /etc/passwd, without NSS (LDAP, sssd). Empty if unreadable.
fn read_colon_first_field(path: &str) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).filter_map(|l| l.split(':').next().map(str::to_string)).collect())
        .unwrap_or_default()
}

// `-A hostname`: the names after the address on each /etc/hosts line.
// bash reads `$HOSTFILE`; this always reads /etc/hosts.
fn read_hosts_hostnames() -> Vec<String> {
    std::fs::read_to_string("/etc/hosts")
        .map(|s| s.lines().map(|l| l.split('#').next().unwrap_or("")).flat_map(|l| l.split_whitespace().skip(1).map(str::to_string)).collect())
        .unwrap_or_default()
}

// `-A service`: /etc/init.d's entries, which is bash's answer too; no
// systemd units.
fn read_dir_names(path: &str) -> Vec<String> {
    std::fs::read_dir(path).map(|entries| entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect()).unwrap_or_default()
}

// Raw candidates for one action, unfiltered by `word` (`resolve_spec`
// filters every source alike). `binding` and `helptopic` are always empty
// rather than unknown.
pub fn resolve_action(action: CompgenAction, ctx: &ActionContext, cwd: &Path, word: &str) -> Vec<String> {
    use CompgenAction::*;
    match action {
        Alias => ctx.aliases.clone(),
        ArrayVar => ctx.arrays.clone(),
        Binding => Vec::new(),
        Builtin => ctx.builtins.clone(),
        // Aliases aren't included, unlike bash; see
        // `ShellCompletionProvider::command_name_candidates`.
        Command => {
            let mut names = ctx.builtins.clone();
            names.extend(ctx.functions.iter().cloned());
            names.extend(ctx.path_commands.iter().cloned());
            names
        }
        Directory => path_entries(cwd, word, true),
        Disabled => Vec::new(),
        Enabled => ctx.builtins.clone(),
        Export => ctx.exported.clone(),
        File => path_entries(cwd, word, false),
        Function => ctx.functions.clone(),
        Group => read_colon_first_field("/etc/group"),
        HelpTopic => Vec::new(),
        Hostname => read_hosts_hostnames(),
        Job => ctx.jobs.clone(),
        Keyword => KEYWORDS.iter().map(|s| s.to_string()).collect(),
        Running => ctx.running_jobs.clone(),
        Service => read_dir_names("/etc/init.d"),
        Setopt => ctx.set_o_names.clone(),
        Shopt => ctx.shopt_names.clone(),
        // `SIG`-prefixed, as bash's `-A signal` prints them, plus EXIT.
        Signal => std::iter::once("EXIT".to_string()).chain(ctx.signal_names.iter().map(|n| format!("SIG{n}"))).collect(),
        Stopped => ctx.stopped_jobs.clone(),
        User => read_colon_first_field("/etc/passwd"),
        Variable => {
            let mut names = ctx.variables.clone();
            names.extend(ctx.arrays.iter().cloned());
            names
        }
    }
}

// A completion source, from `compgen`'s arguments or a `complete NAME`
// registration in `Shell::completions`. Each field is one option; see
// `resolve_spec` for how they combine.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct CompgenSpec {
    pub actions: Vec<CompgenAction>,
    pub globpat: Option<String>,
    pub wordlist: Option<String>,
    pub function: Option<String>,
    pub command: Option<String>,
    pub filterpat: Option<String>,
    pub prefix: String,
    pub suffix: String,
    // `-o` names in the order first given, for `-p` (sorted there, as bash
    // prints them) and `compopt`. `resolve_spec` never reads them; `nosort`
    // is read by the completion UI (see
    // `ShellCompletionProvider::registered_spec_candidates`).
    pub opts: Vec<String>,
}

impl CompgenSpec {
    pub fn has_any_source(&self) -> bool {
        !self.actions.is_empty() || self.globpat.is_some() || self.wordlist.is_some() || self.function.is_some() || self.command.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    UnknownAction(String),
    UnknownOption(char),
    UnknownOptName(String),
    MissingArg(&'static str),
}

// Parses the option grammar `compgen` and `complete` share (not `-V`,
// `-p`, `-r` or `-D`, which their callers handle). Returns the spec and
// every non-option token in order: `compgen` uses the last as its word,
// `complete` all of them as command names.
pub fn parse_spec_args(args: &[String]) -> Result<(CompgenSpec, Vec<String>), ParseError> {
    let mut spec = CompgenSpec::default();
    let mut positionals = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        let a = args[idx].as_str();
        match a {
            "-A" => {
                let name = args.get(idx + 1).ok_or(ParseError::MissingArg("-A"))?;
                spec.actions.push(action_by_name(name).ok_or_else(|| ParseError::UnknownAction(name.clone()))?);
                idx += 2;
            }
            "-G" => {
                spec.globpat = Some(args.get(idx + 1).ok_or(ParseError::MissingArg("-G"))?.clone());
                idx += 2;
            }
            "-W" => {
                spec.wordlist = Some(args.get(idx + 1).ok_or(ParseError::MissingArg("-W"))?.clone());
                idx += 2;
            }
            "-F" => {
                spec.function = Some(args.get(idx + 1).ok_or(ParseError::MissingArg("-F"))?.clone());
                idx += 2;
            }
            "-C" => {
                spec.command = Some(args.get(idx + 1).ok_or(ParseError::MissingArg("-C"))?.clone());
                idx += 2;
            }
            "-X" => {
                spec.filterpat = Some(args.get(idx + 1).ok_or(ParseError::MissingArg("-X"))?.clone());
                idx += 2;
            }
            "-P" => {
                spec.prefix = args.get(idx + 1).ok_or(ParseError::MissingArg("-P"))?.clone();
                idx += 2;
            }
            "-S" => {
                spec.suffix = args.get(idx + 1).ok_or(ParseError::MissingArg("-S"))?.clone();
                idx += 2;
            }
            "-o" => {
                let name = args.get(idx + 1).ok_or(ParseError::MissingArg("-o"))?;
                if !O_OPTIONS.contains(&name.as_str()) {
                    return Err(ParseError::UnknownOptName(name.clone()));
                }
                if !spec.opts.contains(name) {
                    spec.opts.push(name.clone());
                }
                idx += 2;
            }
            "--" => {
                idx += 1;
                positionals.extend(args[idx..].iter().cloned());
                break;
            }
            _ if a.starts_with('-') && a.len() > 1 => {
                for c in a[1..].chars() {
                    spec.actions.push(action_from_flag(c).ok_or(ParseError::UnknownOption(c))?);
                }
                idx += 1;
            }
            _ => {
                positionals.push(a.to_string());
                idx += 1;
            }
        }
    }
    Ok((spec, positionals))
}

enum ExternalKind {
    Function,
    Command,
}

// `-F`/`-C` in a subprocess: this binary with `-c` (as `$(...)` runs),
// after `preamble` (`Shell::functions_preamble`), so the function sees
// the caller's functions and variables. Both get three quoted words
// appended: `compgen`, `word` and "" for $1, $2 and $3, since there's no
// command line to take them from (bash's do the same). For `-F` the text
// calls the function and prints COMPREPLY; for `-C` it's the command
// itself. Output is one candidate per line. Any failure yields nothing,
// since this backs Tab completion too, where an error would be worse.
fn run_external(kind: ExternalKind, text: &str, word: &str, cwd: &Path, preamble: &str) -> Vec<String> {
    let Ok(exe) = std::env::current_exe() else { return Vec::new() };
    let quote = crate::serialize::quote_literal;
    let args = format!("{} {} {}", quote("compgen"), quote(word), quote(""));
    let body = match kind {
        ExternalKind::Command => format!("{text} {args}"),
        ExternalKind::Function => format!("{text} {args}\nprintf '%s\\n' \"${{COMPREPLY[@]}}\"\n"),
    };
    let script = format!("{preamble}{body}");
    match Command::new(exe).arg("-c").arg(script).current_dir(cwd).output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

// Resolves a spec into its candidates: actions, `-G` and `-W` filtered by
// `word`, `-F`/`-C` unfiltered (bash trusts them too), then `-X` over the
// whole pool (`!pattern` keeps matches instead of removing them), then
// `-P`/`-S`. The exit status for an empty result is the caller's call.
pub fn resolve_spec(spec: &CompgenSpec, word: &str, ctx: &ActionContext, cwd: &Path, preamble: &str) -> Vec<String> {
    let mut sourced: Vec<String> = Vec::new();
    for action in &spec.actions {
        sourced.extend(resolve_action(*action, ctx, cwd, word));
    }
    if let Some(pat) = &spec.globpat {
        sourced.extend(crate::glob::expand(pat, crate::glob::Options::default(), cwd).unwrap_or_default());
    }
    if let Some(list) = &spec.wordlist {
        sourced.extend(list.split_whitespace().map(str::to_string));
    }
    let mut candidates: Vec<String> = sourced.into_iter().filter(|c| c.starts_with(word)).collect();

    if let Some(name) = &spec.function {
        candidates.extend(run_external(ExternalKind::Function, name, word, cwd, preamble));
    }
    if let Some(cmd) = &spec.command {
        candidates.extend(run_external(ExternalKind::Command, cmd, word, cwd, preamble));
    }

    if let Some(pat) = &spec.filterpat {
        let (negate, pat) = match pat.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pat.as_str()),
        };
        candidates.retain(|c| crate::glob::matches(pat, c) == negate);
    }
    if !spec.prefix.is_empty() || !spec.suffix.is_empty() {
        candidates = candidates.into_iter().map(|c| format!("{}{c}{}", spec.prefix, spec.suffix)).collect();
    }
    candidates
}

// `complete -p`: a spec back as `complete [options] NAME`, in bash's
// order: `-o` sorted, actions as flags or `-A name`, `-G`/`-W`/`-F`/`-C`,
// `-P`/`-S`, `-X`, then the name.
pub fn format_spec(spec: &CompgenSpec, trailing: &str) -> String {
    let mut parts = vec!["complete".to_string()];
    let mut sorted_opts = spec.opts.clone();
    crate::sort::sort(&mut sorted_opts);
    for o in &sorted_opts {
        parts.push("-o".to_string());
        parts.push(o.clone());
    }
    for action in &spec.actions {
        match action_flag_char(*action) {
            Some(c) => parts.push(format!("-{c}")),
            None => {
                parts.push("-A".to_string());
                parts.push(action_name(*action).to_string());
            }
        }
    }
    if let Some(g) = &spec.globpat {
        parts.push("-G".to_string());
        parts.push(crate::serialize::quote_literal(g));
    }
    if let Some(w) = &spec.wordlist {
        parts.push("-W".to_string());
        parts.push(crate::serialize::quote_literal(w));
    }
    if let Some(f) = &spec.function {
        parts.push("-F".to_string());
        parts.push(f.clone());
    }
    if let Some(c) = &spec.command {
        parts.push("-C".to_string());
        parts.push(crate::serialize::quote_literal(c));
    }
    if !spec.prefix.is_empty() {
        parts.push("-P".to_string());
        parts.push(crate::serialize::quote_literal(&spec.prefix));
    }
    if !spec.suffix.is_empty() {
        parts.push("-S".to_string());
        parts.push(crate::serialize::quote_literal(&spec.suffix));
    }
    if let Some(x) = &spec.filterpat {
        parts.push("-X".to_string());
        parts.push(crate::serialize::quote_literal(x));
    }
    parts.push(trailing.to_string());
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ActionContext {
        ActionContext::default()
    }

    #[test]
    fn parse_spec_args_builds_actions_from_long_and_short_forms() {
        let (spec, positionals) = parse_spec_args(&["-A".to_string(), "alias".to_string(), "-b".to_string()]).unwrap();
        assert_eq!(spec.actions, vec![CompgenAction::Alias, CompgenAction::Builtin]);
        assert!(positionals.is_empty());
    }

    #[test]
    fn parse_spec_args_combines_shorthand_flags_in_one_token() {
        let (spec, _) = parse_spec_args(&["-ab".to_string()]).unwrap();
        assert_eq!(spec.actions, vec![CompgenAction::Alias, CompgenAction::Builtin]);
    }

    #[test]
    fn parse_spec_args_collects_every_positional_both_before_and_after_dashdash() {
        let (_, positionals) = parse_spec_args(&["foo".to_string(), "--".to_string(), "bar".to_string(), "baz".to_string()]).unwrap();
        assert_eq!(positionals, vec!["foo".to_string(), "bar".to_string(), "baz".to_string()]);
    }

    #[test]
    fn parse_spec_args_rejects_unknown_action_and_option() {
        assert_eq!(parse_spec_args(&["-A".to_string(), "bogus".to_string()]).unwrap_err(), ParseError::UnknownAction("bogus".to_string()));
        assert_eq!(parse_spec_args(&["-Z".to_string()]).unwrap_err(), ParseError::UnknownOption('Z'));
        assert_eq!(parse_spec_args(&["-o".to_string(), "bogus".to_string()]).unwrap_err(), ParseError::UnknownOptName("bogus".to_string()));
    }

    #[test]
    fn resolve_spec_wordlist_preserves_order_and_filters_by_prefix() {
        let spec = CompgenSpec { wordlist: Some("banana apple cherry".to_string()), ..Default::default() };
        let out = resolve_spec(&spec, "", &ctx(), Path::new("/"), "");
        assert_eq!(out, vec!["banana".to_string(), "apple".to_string(), "cherry".to_string()]);
        let out = resolve_spec(&spec, "a", &ctx(), Path::new("/"), "");
        assert_eq!(out, vec!["apple".to_string()]);
    }

    #[test]
    fn resolve_spec_x_filter_excludes_by_default_and_keeps_only_matches_when_negated() {
        let spec = CompgenSpec { wordlist: Some("apple banana avocado".to_string()), filterpat: Some("a*".to_string()), ..Default::default() };
        assert_eq!(resolve_spec(&spec, "", &ctx(), Path::new("/"), ""), vec!["banana".to_string()]);

        let spec = CompgenSpec { filterpat: Some("!a*".to_string()), ..spec };
        assert_eq!(resolve_spec(&spec, "", &ctx(), Path::new("/"), ""), vec!["apple".to_string(), "avocado".to_string()]);
    }

    #[test]
    fn resolve_spec_prefix_and_suffix_wrap_every_candidate() {
        let spec = CompgenSpec { wordlist: Some("a b".to_string()), prefix: "<".to_string(), suffix: ">".to_string(), ..Default::default() };
        assert_eq!(resolve_spec(&spec, "", &ctx(), Path::new("/"), ""), vec!["<a>".to_string(), "<b>".to_string()]);
    }

    #[test]
    fn resolve_action_keyword_and_signal_are_static() {
        let out = resolve_action(CompgenAction::Keyword, &ctx(), Path::new("/"), "");
        assert!(out.iter().any(|n| n == "if"));
        let mut c = ctx();
        c.signal_names = vec!["TERM".to_string()];
        let out = resolve_action(CompgenAction::Signal, &c, Path::new("/"), "");
        assert_eq!(out, vec!["EXIT".to_string(), "SIGTERM".to_string()]);
    }

    #[test]
    fn format_spec_matches_real_bashs_own_print_order() {
        let spec = CompgenSpec {
            wordlist: Some("a b".to_string()),
            filterpat: Some("!a*".to_string()),
            prefix: "<".to_string(),
            suffix: ">".to_string(),
            opts: vec!["nospace".to_string(), "filenames".to_string()],
            ..Default::default()
        };
        assert_eq!(format_spec(&spec, "cmd1"), "complete -o filenames -o nospace -W 'a b' -P '<' -S '>' -X '!a*' cmd1");
    }
}
