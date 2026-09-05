mod archive;
mod arith;
#[cfg(test)]
mod bashdiff;
mod bishedit;
mod browser;
mod builtins;
mod compgen;
mod coroutine;
mod csscolor;
mod debugger;
mod diff;
mod docs;
mod dotenv;
mod editor;
mod editorconfig;
mod exec;
mod fileeditor;
mod git;
mod gitignore;
mod glob;
mod hexedit;
mod history;
mod hosts;
mod html;
mod inflate;
mod ini;
mod json;
mod keymap;
mod lexer;
mod lsp;
mod lspclient;
mod markdown;
mod pager;
mod parser;
mod pathspec;
mod poll;
mod prompt;
mod pty;
mod regex;
mod repl;
mod roff;
mod scheduler;
mod serialize;
mod session;
mod stackguard;
mod suggest;
mod tempdir;
mod term;
mod theme;
mod time;
mod toml;
mod tool;
mod url;
#[cfg(test)]
mod vimdiff;
mod vt100;
mod window;

use std::io::{IsTerminal, Read};

fn main() {
    // As close to the bottom of the stack as this program has a place
    // to stand -- everything below measures its own nesting against
    // here. See stackguard's own doc comment.
    stackguard::note_base();
    let args: Vec<String> = std::env::args().collect();

    // `bish tool <subcommand>` -- checked first, ahead of every other
    // argv-based branch below (in particular the generic `args.len() >=
    // 2` script-path one just after), since `tool` isn't a script name
    // to try to open. See tool.rs's own doc comment for what lives here
    // today and what's planned alongside it.
    if args.get(1).map(String::as_str) == Some("tool") {
        std::process::exit(tool::run(&args[2..]));
    }

    // `bish session <subcommand>` -- detachable sessions (see
    // ../bish-detachable-sessions.md and
    // ~/.claude/plans/melodic-sauteeing-boot.md). Same "checked ahead of
    // the generic script-path branch" treatment as `tool` above, since
    // `session` isn't a script name either. `--daemon <name>` is an
    // internal bootstrap mode (`session::run_new` re-execs this same
    // binary with it) -- not documented as user-facing here for that
    // reason, though nothing stops a user from invoking it directly.
    if args.get(1).map(String::as_str) == Some("session") {
        let code = match args.get(2).map(String::as_str) {
            // `-A` is tmux's own spelling for "attach to it if it is
            // already there, create it if it is not".
            Some("new") => match (args.get(3).map(String::as_str), args.get(4)) {
                (Some("-A" | "--attach"), Some(name)) => session::run_new(name, true),
                (Some("-A" | "--attach"), None) => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session new [-A] <name>")),
                (Some(name), _) => session::run_new(name, false),
                (None, _) => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session new [-A] <name>")),
            },
            Some("attach") => match args.get(3) {
                Some(name) => session::run_client(name),
                None => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session attach <name>")),
            },
            Some("--daemon") => match args.get(3) {
                Some(name) => session::run_daemon(name),
                None => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session --daemon <name>")),
            },
            Some("ls") => session::run_ls(),
            Some("send") => match args.get(3) {
                Some(name) => session::run_send(name, &args[4..]),
                None => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session send <name> <keys>...")),
            },
            Some("capture") => match args.get(3) {
                Some(name) => session::run_capture(name),
                None => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session capture <name>")),
            },
            Some("rename") => match (args.get(3), args.get(4)) {
                (Some(from), Some(to)) => session::run_rename(from, to),
                _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session rename <old> <new>")),
            },
            Some("kill") => match args.get(3) {
                Some(name) => session::run_kill(name),
                None => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session kill <name>")),
            },
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session {new|attach|ls|rename|send|capture|kill} <name>")),
        };
        match code {
            Ok(c) => std::process::exit(c),
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    }

    let invocation = match Invocation::parse(&args[1..]) {
        Ok(inv) => inv,
        Err(e) => {
            eprintln!("bish: {e}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if invocation.print_version {
        println!("bish {} (bash {} compatible)", env!("CARGO_PKG_VERSION"), exec::BASH_VERSION);
        std::process::exit(0);
    }
    if invocation.print_help {
        println!("{USAGE}");
        std::process::exit(0);
    }

    let mut shell = exec::Shell::new();
    // $SHLVL counts how deep this shell is inside other shells. bash
    // reads whatever it inherited and adds one; a shell that does not
    // is invisible to anything counting nesting, `exit`-on-last-level
    // prompts included.
    let depth = shell.lookup_var("SHLVL").trim().parse::<i64>().unwrap_or(0);
    // Set directly rather than by running `export SHLVL=N` as a script.
    // Lexing, parsing and executing a statement to increment a counter
    // cost 68us of a 355us startup, and every re-exec'd construct paid
    // it before running a single character of what it was actually
    // asked to do.
    shell.export_var("SHLVL", (depth + 1).to_string());
    for (flag, on) in &invocation.set_flags {
        shell.apply_shell_flag(*flag, *on);
    }
    for (name, on) in &invocation.set_options {
        shell.apply_shell_option(name, *on);
    }

    if let Some(command) = &invocation.command {
        // The last letter of `$-`: `c` here, `s` for a script read from
        // stdin, `i` when interactive, and nothing for a named script.
        shell.invocation_flag = Some('c');
        let script_name = invocation.operands.first().cloned().unwrap_or_else(|| "bish".to_string());
        let positional = invocation.operands.get(1..).map(<[String]>::to_vec).unwrap_or_default();
        shell.set_script_args(script_name, positional);
        source_bash_env(&mut shell, &invocation);
        if invocation.promoted {
            std::process::exit(run_source_in_a_pane(&mut shell, command));
        }
        std::process::exit(run_source(&mut shell, command));
    }

    // A named script, unless `-s` said to read stdin regardless.
    if !invocation.read_stdin
        && let Some(path) = invocation.operands.first()
    {
        shell.set_script_args(path.clone(), invocation.operands[1..].to_vec());
        // A named script has an outermost `main` call frame; `-c` text
        // does not. See Shell::running_a_script.
        shell.running_a_script = true;
        source_bash_env(&mut shell, &invocation);
        match std::fs::read_to_string(path) {
            Ok(src) => std::process::exit(run_source(&mut shell, &src)),
            Err(e) => {
                eprintln!("bish: {}: {}", path, exec::os_message(&e));
                std::process::exit(127);
            }
        }
    }
    if !invocation.operands.is_empty() {
        // `-s` with arguments: they are the positional parameters, and
        // the script itself comes from stdin.
        shell.set_script_args("bish".to_string(), invocation.operands.clone());
    }

    if invocation.interactive.unwrap_or_else(|| std::io::stdin().is_terminal()) {
        shell.invocation_flag = Some('i');
        if !invocation.norc {
            load_config(&mut shell);
        }
        repl::run(shell, invocation.promoted);
    } else {
        shell.invocation_flag = Some('s');
        source_bash_env(&mut shell, &invocation);
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_ok() {
            std::process::exit(run_source(&mut shell, &src));
        }
    }
}

const USAGE: &str = "\
usage: bish [options] [script [args...]]
       bish [options] -c command [name [args...]]
       bish tool|session <subcommand>

  -c COMMAND     run COMMAND
  -s             read commands from stdin, remaining arguments are $1..
  -i             force interactive, even without a terminal
  -l, --login    a login shell: read $BASH_ENV/profile before anything else
  --norc         do not read ~/.config/bish/config.bash
  -o NAME        set the `set -o` option NAME (`+o NAME` unsets it)
  -e -u -x -f    the `set` flags, and any other single-letter one
  --version      print the version and exit
  --help         print this and exit";

// What the command line asked for. Everything here is bash's own
// spelling; `--promoted` is bish's own (see repl::run).
#[derive(Default)]
struct Invocation {
    command: Option<String>,
    operands: Vec<String>,
    read_stdin: bool,
    // `None` means "decide from whether stdin is a terminal", which is
    // what a shell with no `-i`/`-c`/script does.
    interactive: Option<bool>,
    login: bool,
    norc: bool,
    promoted: bool,
    print_version: bool,
    print_help: bool,
    set_flags: Vec<(char, bool)>,
    set_options: Vec<(String, bool)>,
}

impl Invocation {
    // Everything that is not `-c`, a script path, `tool` or
    // `--promoted` used to be read as a *filename*, so `bish -lc 'echo'`
    // -- how a terminal emulator starts a login shell -- failed with
    // "-lc: No such file or directory", and `bish --version` did too.
    fn parse(args: &[String]) -> Result<Invocation, String> {
        let mut inv = Invocation::default();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            match arg.as_str() {
                "--" => {
                    i += 1;
                    break;
                }
                "--version" => inv.print_version = true,
                "--help" => inv.print_help = true,
                "--login" => inv.login = true,
                "--norc" | "--noprofile" => inv.norc = true,
                "--promoted" => inv.promoted = true,
                _ if arg.starts_with("--") => return Err(format!("{arg}: unrecognized option")),
                // A cluster of single-letter flags, either sense:
                // `-euo pipefail`, `+x`. A bare `-` or `+` is an
                // operand, not a flag.
                _ if (arg.starts_with('-') || arg.starts_with('+')) && arg.len() > 1 => {
                    let on = arg.starts_with('-');
                    let mut letters = arg[1..].chars();
                    while let Some(c) = letters.next() {
                        match c {
                            // `c` ends the cluster: what follows is the
                            // command, which is why `-lc 'echo hi'`
                            // works.
                            'c' => {
                                i += 1;
                                inv.command = Some(args.get(i).cloned().ok_or("-c: option requires an argument")?);
                                inv.interactive.get_or_insert(false);
                            }
                            'o' => {
                                let rest: String = letters.by_ref().collect();
                                let name = match rest.is_empty() {
                                    false => rest,
                                    true => {
                                        i += 1;
                                        args.get(i).cloned().ok_or("-o: option requires an argument")?
                                    }
                                };
                                inv.set_options.push((name, on));
                            }
                            's' => inv.read_stdin = true,
                            'i' => inv.interactive = Some(on),
                            'l' => inv.login = true,
                            other => inv.set_flags.push((other, on)),
                        }
                    }
                }
                // The first non-option word is the script, and
                // everything after it belongs to the script rather than
                // to this shell.
                _ => break,
            }
            i += 1;
        }
        inv.operands.extend_from_slice(&args[i.min(args.len())..]);
        Ok(inv)
    }
}

#[cfg(test)]
mod tests {
    use super::Invocation;

    fn parse(args: &[&str]) -> Invocation {
        Invocation::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).expect("parses")
    }

    // Everything except `-c`, a script path and two bish-only words
    // used to be read as a *filename*, so `bish -lc 'echo hi'` -- how a
    // terminal emulator starts a login shell -- failed with "-lc: No
    // such file or directory".
    #[test]
    fn a_cluster_ending_in_c_takes_the_next_argument_as_the_command() {
        let inv = parse(&["-lc", "echo hi"]);
        assert_eq!(inv.command.as_deref(), Some("echo hi"));
        assert!(inv.login);
        assert_eq!(inv.interactive, Some(false), "-c is not interactive");
    }

    #[test]
    fn set_flags_and_options_come_through() {
        let inv = parse(&["-euo", "pipefail", "-c", "true"]);
        assert_eq!(inv.set_flags, vec![('e', true), ('u', true)]);
        assert_eq!(inv.set_options, vec![("pipefail".to_string(), true)]);
        assert_eq!(inv.command.as_deref(), Some("true"));
    }

    #[test]
    fn plus_unsets_what_minus_sets() {
        let inv = parse(&["+x", "script.sh"]);
        assert_eq!(inv.set_flags, vec![('x', false)]);
        assert_eq!(inv.operands, vec!["script.sh".to_string()]);
    }

    // The first word that is not an option belongs to the script, and
    // so does everything after it -- `bish s.sh -x` passes `-x` to the
    // script, it does not turn on xtrace.
    #[test]
    fn options_stop_at_the_first_operand() {
        let inv = parse(&["-e", "s.sh", "-x", "arg"]);
        assert_eq!(inv.set_flags, vec![('e', true)]);
        assert_eq!(inv.operands, vec!["s.sh".to_string(), "-x".to_string(), "arg".to_string()]);
    }

    #[test]
    fn dash_dash_ends_the_options() {
        let inv = parse(&["--", "-notaflag"]);
        assert!(inv.set_flags.is_empty());
        assert_eq!(inv.operands, vec!["-notaflag".to_string()]);
    }

    #[test]
    fn a_missing_option_argument_is_an_error() {
        let args = ["-c".to_string()];
        assert!(Invocation::parse(&args).is_err());
        let args = ["-o".to_string()];
        assert!(Invocation::parse(&args).is_err());
    }

    #[test]
    fn an_unknown_long_option_is_an_error() {
        let args = ["--nosuch".to_string()];
        assert!(Invocation::parse(&args).is_err());
    }

    #[test]
    fn version_and_help_are_recognized() {
        assert!(parse(&["--version"]).print_version);
        assert!(parse(&["--help"]).print_help);
    }

    // A lone `-` is an operand, the way it is to every other tool.
    #[test]
    fn a_bare_dash_is_not_a_flag() {
        let inv = parse(&["-"]);
        assert!(inv.set_flags.is_empty());
        assert_eq!(inv.operands, vec!["-".to_string()]);
    }
}

// `$BASH_ENV` for a non-interactive shell, and the login profile for a
// login one -- bash reads the first before running a script or `-c`
// text, which is how a system-wide setup file reaches a script at all.
/// `bish --promoted -c '<script>'`: runs the script the way a split or
/// tabbed window runs one -- promoted, with output going to a pane's
/// grid instead of fd 1 -- and prints what the pane ends up showing.
///
/// This exists for bashdiff's pane corpus. A pane is a genuinely
/// different execution path: a pty per foreground external command,
/// `ExecResult::Fg` handed back for something else to drive, and a
/// vt100 grid where a pipe would otherwise be. Three separate bugs
/// lived in it unnoticed -- a builtin pipeline stage writing past its
/// pipe, `$(external)` coming back empty, and every command after an
/// external one being dropped -- precisely because every corpus case
/// ran the other way. Running the same script both ways, from outside,
/// is what lets the corpus have an opinion about it.
///
/// The grid is deliberately far larger than any case needs: what
/// scrolls off the top of a pane is gone, and a corpus case whose
/// output was silently truncated would compare as a difference that
/// has nothing to do with the shell.
fn run_source_in_a_pane(shell: &mut exec::Shell, src: &str) -> i32 {
    const ROWS: usize = 400;
    const COLS: usize = 400;
    let screen = std::rc::Rc::new(std::cell::RefCell::new(vt100::Screen::new(ROWS, COLS)));
    shell.set_sink_grid(std::rc::Rc::clone(&screen));
    shell.mark_promoted();
    let status = run_source(shell, src);
    // The last command of a line hands a pty-backed job off rather than
    // waiting for it (see ExecResult::Fg). In a pane repl.rs drives it;
    // here nothing else will, so it is drained now -- otherwise the
    // grid is read before the command that filled it has finished.
    shell.settle_pending_fg();
    print!("{}", screen.borrow().text_unwrapped());
    use std::io::Write;
    let _ = std::io::stdout().flush();
    status
}

fn source_bash_env(shell: &mut exec::Shell, invocation: &Invocation) {
    if invocation.norc {
        return;
    }
    let mut candidates: Vec<String> = Vec::new();
    if invocation.login
        && let Some(home) = std::env::var_os("HOME")
    {
        candidates.push(std::path::PathBuf::from(home).join(".profile").display().to_string());
    }
    let env_var = shell.lookup_var("BASH_ENV");
    if !env_var.trim().is_empty() {
        candidates.push(env_var);
    }
    for path in candidates {
        if let Ok(src) = std::fs::read_to_string(&path) {
            shell.run_source_here(&src, &path);
        }
    }
}

// Runs $HOME/.config/bish/config.bash, if present, in the shell's own
// top-level scope before the interactive prompt starts -- matching
// bash's own ~/.bashrc: vars/functions/aliases it sets persist into the
// session that follows (Shell::run_source_here, shared with `source`/`.`
// -- see its own doc comment for why this needs that exact "run in
// place" semantics rather than a subprocess). Only reached from the
// interactive branch below, not `-c`/a script path/piped stdin -- same
// as bash not sourcing ~/.bashrc for a non-interactive run. A missing
// file is the common case, not an error, so it's silently skipped; a
// real read failure or a syntax error inside it is reported (through
// the shell's own stderr sink, same as any other script error) but
// doesn't stop the shell from reaching its prompt -- config.bash is
// just this entry point; anything else the user wants loaded, they
// `source` themselves from inside it.
fn load_config(shell: &mut exec::Shell) {
    let Some(home) = std::env::var_os("HOME") else { return };
    let path = std::path::PathBuf::from(home).join(".config/bish/config.bash");
    match std::fs::read_to_string(&path) {
        Ok(src) => {
            shell.run_source_here(&src, &path.display().to_string());
            // Everything config.bash just set has to be captured into
            // this shell's own remembered environment, or the very first
            // `sync_real_state_in` wipes it.
            //
            // That snapshot is taken in `Shell::new`, which runs *before*
            // this -- and `sync_real_state_in` (which every command goes
            // through, so sibling windows cannot clobber each other's
            // variables) removes every real env var the snapshot does not
            // have. So a plain `MYVAR=x` in config.bash survived exactly
            // until the first command ran. Aliases and functions live on
            // the `Shell` and were never affected, which is what made
            // this look like config.bash working.
            shell.sync_real_state_out();
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("bish: {}: {}", path.display(), e),
    }
}

fn run_source(shell: &mut exec::Shell, src: &str) -> i32 {
    match lexer::Lexer::new(src).tokenize() {
        Ok(toks) => match parser::Parser::new(shell.expand_aliases(toks)).parse_program() {
            Ok(prog) => {
                if let exec::ExecResult::Exit(code) = shell.run_program(&prog) {
                    // The exit trap already ran at whichever site produced
                    // this (see ExecResult::Exit's own doc comment).
                    return code;
                }
                shell.run_exit_trap();
                shell.last_status
            }
            Err(e) => {
                eprintln!("bish: syntax error: {}", e);
                2
            }
        },
        Err(e) => {
            eprintln!("bish: syntax error: {}", e);
            2
        }
    }
}
