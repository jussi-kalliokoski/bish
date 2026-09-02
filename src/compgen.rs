// Shared engine behind the `compgen`/`complete`/`compopt` builtins
// (exec.rs) and bish's own interactive Tab completion (bishedit/
// completion.rs) -- same "hand-rolled, standalone, no Shell dependency"
// shape as glob.rs/regex.rs/csscolor.rs, so both the exec layer (which has
// live, mutable Shell state) and the editor layer (which only ever gets an
// owned per-prompt snapshot, see ShellCompletionProvider's own doc comment
// on why) can drive the exact same logic from whatever data they actually
// have on hand.
//
// Every behavioral rule encoded here (which sources get filtered by the
// current word and which don't, the exact -F/-C calling convention, `-o`'s
// names being validated but otherwise inert, complete -p's own print
// order) was reverse-engineered by running real bash side by side, not
// guessed from the man page.
//
// -F/-C run via subprocess (re-invoking this binary with `-c`, given a
// serialized preamble of the caller's current functions/vars -- see
// exec.rs's own functions_preamble), never via an in-process function
// call. That's a deliberate, real (if narrow) divergence from bash's own
// -F semantics for the *standalone* `compgen`/`complete` builtins (real
// bash calls -F in-process, so side effects the function has on shell
// state persist; this doesn't) -- made specifically so the exact same
// resolver also works from bishedit::completion.rs's interactive Tab path,
// which only ever has an owned snapshot (cwd + a serialized preamble), not
// a live, mutably-borrowable Shell, to work with. A hand-written
// completion function mutating its caller's shell state on purpose would
// be very unusual; COMPREPLY is the only thing that matters in practice.
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

// name -> action's own long name, for -p reconstruction when an action has
// no one-letter shorthand.
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

// The single-letter shorthand flags from compgen/complete's own usage
// synopsis (`[-abcdefgjksuv]`) -- a strict subset of the -A action names
// above (no one-letter shorthand exists for arrayvar/binding/disabled/
// enabled/helptopic/hostname/running/setopt/shopt/signal/stopped).
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

// compgen/complete's own `-o` option names -- validated (an unrecognized
// name is a usage error, confirmed against real bash) but otherwise inert
// here, since every one of them only changes interactive readline
// behavior, never the generated candidate text itself (confirmed
// empirically: `compgen -o filenames -W` with a real directory name
// produced no trailing slash, no different from without -o at all).
pub const O_OPTIONS: &[&str] = &["bashdefault", "default", "dirnames", "filenames", "noquote", "nosort", "nospace", "plusdirs"];

// `-A keyword`: every word bish's own lexer turns into a reserved-word
// token, plus the brace/double-bracket pairs that are reserved punctuation
// rather than word-lookup keywords in this grammar. Real bash's own list
// also includes "!" and "time" (pipeline negation/timing) -- bish's
// grammar doesn't reserve either as a keyword, so they're deliberately
// left out here rather than advertised as something typeable that isn't.
pub const KEYWORDS: &[&str] =
    &["if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac", "select", "function", "coproc", "in", "{", "}", "[[", "]]"];

// Every bit of live Shell state a *contextual* action (one that isn't just
// a static table or a plain filesystem read) needs -- built once per use
// from a live Shell (exec.rs's own Shell::action_context) or a per-prompt
// snapshot (repl.rs, for bishedit::completion.rs's interactive path), so
// this module itself never has to reach into exec::Shell's private
// fields. Plain owned data throughout -- cheap enough to snapshot once per
// compgen/complete invocation or once per prompt redraw, not a hot path.
#[derive(Clone, Default)]
pub struct ActionContext {
    pub aliases: Vec<String>,
    pub functions: Vec<String>,
    // Indexed + associative array names combined -- ArrayVar/Variable
    // don't distinguish the two.
    pub arrays: Vec<String>,
    pub exported: Vec<String>,
    // Every name currently visible to a variable lookup: process env
    // (where every non-local bish assignment actually lives) plus any
    // `local`-declared overlay names, NOT including array names (those are
    // folded in separately by whoever builds this, alongside `arrays`).
    pub variables: Vec<String>,
    pub builtins: Vec<String>,
    pub shopt_names: Vec<String>,
    pub set_o_names: Vec<String>,
    // Bare (non-"SIG"-prefixed) signal names, e.g. "HUP" -- resolve_action
    // adds the "SIG" prefix and the EXIT pseudo-signal itself.
    pub signal_names: Vec<String>,
    pub jobs: Vec<String>,
    pub running_jobs: Vec<String>,
    pub stopped_jobs: Vec<String>,
    // Every PATH executable name, unfiltered by any prefix -- the caller
    // (resolve_spec) applies its own word-prefix filter downstream anyway,
    // so prefiltering here would only save (already cheap, once-per-
    // invocation) work, never change the result.
    pub path_commands: Vec<String>,
}

// `-f`/`-d`'s shared directory-splitting logic: only the final path
// segment of `word` is completed, matching the same convention
// ShellCompletionProvider::file_candidates uses for interactive Tab
// completion (a leading directory part stays a literal prefix of every
// returned name). Unlike that interactive path, real bash's own -f/-d
// neither hides dotfiles nor appends a trailing '/' to directories
// (confirmed against real bash) -- so this deliberately doesn't either, a
// real (if narrow) divergence from bish's own Tab-completion convention in
// favor of matching bash's compgen exactly.
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

// `-A group`/`-A user`: first colon-separated field of every non-comment,
// non-blank line. Plain /etc/group and /etc/passwd parsing -- no NSS
// (LDAP/sssd/etc.) consultation the way a real getgrent(3)/getpwent(3)
// call would do, a documented gap matching this codebase's existing
// tolerance for "no crash, just a plausible-not-guaranteed answer" on
// anything system-database-shaped. Empty (not an error) if the file can't
// be read at all.
fn read_colon_first_field(path: &str) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).filter_map(|l| l.split(':').next().map(str::to_string)).collect())
        .unwrap_or_default()
}

// `-A hostname`: every whitespace-separated field after the first (the IP
// address) on each non-comment /etc/hosts line -- real bash reads
// $HOSTFILE (defaulting to /etc/hosts); this always reads /etc/hosts
// itself, a narrower but honest subset.
fn read_hosts_hostnames() -> Vec<String> {
    std::fs::read_to_string("/etc/hosts")
        .map(|s| s.lines().map(|l| l.split('#').next().unwrap_or("")).flat_map(|l| l.split_whitespace().skip(1).map(str::to_string)).collect())
        .unwrap_or_default()
}

// `-A service`: real bash's own answer here is itself just "list
// /etc/init.d" on a traditional SysV-init Linux system, empty everywhere
// else -- same behavior here, no systemd unit-file enumeration attempted.
fn read_dir_names(path: &str) -> Vec<String> {
    std::fs::read_dir(path).map(|entries| entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect()).unwrap_or_default()
}

// Raw candidates for one -A action/shorthand flag, unfiltered by `word`
// (the caller, resolve_spec, applies the prefix filter uniformly across
// every source). A few real bash actions have nothing backing them in
// bish at all -- `binding` (no named-keybinding registry; bish's editor
// never exposed one) and `helptopic` (no `help` builtin) always yield
// empty rather than being rejected as unknown action names, the same
// "recognized, honestly empty" tolerance as an unready man page elsewhere
// in this codebase.
pub fn resolve_action(action: CompgenAction, ctx: &ActionContext, cwd: &Path, word: &str) -> Vec<String> {
    use CompgenAction::*;
    match action {
        Alias => ctx.aliases.clone(),
        ArrayVar => ctx.arrays.clone(),
        Binding => Vec::new(),
        Builtin => ctx.builtins.clone(),
        // Deliberately excludes aliases, unlike real bash -- same "this
        // won't actually run" reasoning as ShellCompletionProvider's own
        // command_name_candidates (bish's `alias` is never expanded at
        // command-run time).
        Command => {
            let mut names = ctx.builtins.clone();
            names.extend(ctx.functions.iter().cloned());
            names.extend(ctx.path_commands.iter().cloned());
            names
        }
        Directory => path_entries(cwd, word, true),
        // bish has no `enable`/`disable` builtin -- nothing is ever
        // disabled, so this pair's answer is always "all of them" / "none
        // of them", same as a real bash session that has never called
        // `enable -n` on anything.
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
        // "SIG"-prefixed, matching real bash's own -A signal output
        // (confirmed: "SIGHUP" "SIGINT" ...) plus the EXIT pseudo-signal --
        // the only pseudo-signal bish's own `trap` actually recognizes
        // (unlike real bash, no DEBUG/ERR/RETURN support to advertise
        // here).
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

// A registered/parsed completion source -- the same shape whether it came
// from `compgen`'s own one-shot arguments or a `complete NAME`
// registration stored in Shell::completions. Every field mirrors one of
// compgen/complete's own options 1:1; see resolve_spec for how they
// combine.
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
    // -o names, in the order first given -- kept around purely for -p
    // reconstruction (sorted there, matching real bash's own -p output
    // order) and compopt's -o/+o toggling; never consulted by
    // resolve_spec itself (see O_OPTIONS' own doc comment on why they're
    // inert for generated text).
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

// Parses the option grammar `compgen`/`complete` share (everything except
// compgen's own `-V varname` and complete's own `-p`/`-r`/`-D`, which their
// respective callers handle before/around this). Returns the built spec
// plus every non-option token encountered (both bare words and anything
// after a `--`) in order -- compgen only ever wants the *last* one (its
// own `[word]` is singular; a caller passing more than one just has the
// last one win, matching this module's own precedent elsewhere), complete
// wants *all* of them (one registration can name several commands at
// once).
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

// `-F`/`-C`: runs in a subprocess -- the same re-invoke-this-binary-with-
// `-c` pattern exec.rs's own run_command_substitution uses for `$(...)`,
// given `preamble` (exec.rs's Shell::functions_preamble output: every
// currently-visible var/array/function serialized as real bish source
// text) so the invoked function/command sees the same functions and
// variables the caller does. Both kinds get the same three positional
// words appended, shell-quoted, onto the invoked text -- $1/$2/$3 =
// "compgen"/current word/previous word, always "compgen"/`word`/""
// since there's no real command-line context to draw $1 or $3 from
// (confirmed against real bash: a function that echoes "$1 $2 $3" prints
// "compgen <word> " every time; `-C "echo hi"` runs as if you'd typed
// `echo hi 'compgen' '<word>' ''`). For -F, the invoked text is a call to
// the named function followed by dumping $COMPREPLY -- for -C, `text`
// itself is the whole command line. Splits captured stdout into lines,
// one candidate per line; any subprocess-spawn failure (missing exe,
// nonexistent function -- caught here as simply "no COMPREPLY output")
// just yields no candidates rather than erroring, since this same
// function backs the interactive Tab-completion path, where a hard error
// would be far more disruptive than silently offering nothing.
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

// Resolves a whole spec into its final candidate list, in front of the
// `word` prefix filter (-A actions/-G/-W) or deliberately behind it (-F/
// -C, trusted to have already applied their own prefix logic -- confirmed
// against real bash: a COMPREPLY entry not matching `word` at all still
// comes through unfiltered), then -X (whole-pool filter, `!pattern` keeps
// only matches instead of excluding them) and -P/-S (whole-pool wrap).
// Does not decide exit-status/empty-vs-had-a-source semantics -- that's
// each caller's own business (compgen: exit 1 iff a source was given and
// this came back empty; the interactive path: just show whatever comes
// back, empty or not).
pub fn resolve_spec(spec: &CompgenSpec, word: &str, ctx: &ActionContext, cwd: &Path, preamble: &str) -> Vec<String> {
    let mut sourced: Vec<String> = Vec::new();
    for action in &spec.actions {
        sourced.extend(resolve_action(*action, ctx, cwd, word));
    }
    if let Some(pat) = &spec.globpat {
        sourced.extend(crate::glob::expand(pat, crate::glob::Options::default()).unwrap_or_default());
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

// `complete -p`: reconstructs a spec's own `complete [options] NAME` text
// -- print order (-o sorted, then each action as its shorthand flag if one
// exists else `-A name`, then -G/-W/-F/-C, then -P/-S, then -X, then the
// trailing name) matches real bash's own observed -p output exactly.
pub fn format_spec(spec: &CompgenSpec, trailing: &str) -> String {
    let mut parts = vec!["complete".to_string()];
    let mut sorted_opts = spec.opts.clone();
    sorted_opts.sort();
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
