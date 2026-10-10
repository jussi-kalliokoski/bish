mod archive;
mod arith;
#[cfg(test)]
mod bashdiff;
mod bishedit;
mod browser;
mod builtins;
mod commitlist;
mod compgen;
mod coroutine;
mod csscolor;
mod dap;
mod dapclient;
mod debugger;
mod diff;
mod docs;
mod dotenv;
mod editor;
mod editorconfig;
mod encoding;
mod exec;
mod fileeditor;
mod fileops;
mod framing;
mod git;
mod gitignore;
mod gittest;
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
mod lspserver;
mod markdown;
mod mcp;
mod opener;
mod pager;
mod parser;
mod pathspec;
mod platform;
mod poll;
mod prompt;
mod pty;
mod regex;
mod repl;
mod review;
mod roff;
mod scheduler;
mod serialize;
mod session;
mod sort;
mod stackguard;
mod suggest;
mod tempdir;
mod term;
mod theme;
mod time;
mod toml;
mod tool;
mod toolpath;
mod trust;
mod url;
#[cfg(test)]
mod vimdiff;
mod vt100;
mod watch;
mod window;

use std::io::{IsTerminal, Read};

fn main() {
    // As near the bottom of the stack as this program can stand; everything
    // measures its nesting from here (see stackguard).
    stackguard::note_base();
    // First: signals ignored on arrival can't be touched by `trap` for the
    // process's life (see `exec::IGNORED_AT_ENTRY`).
    exec::record_signals_ignored_at_entry();
    let args: Vec<String> = std::env::args().collect();

    // `bish tool <subcommand>`, before the script-path branch, since `tool`
    // isn't a script (see tool.rs).
    if args.get(1).map(String::as_str) == Some("tool") {
        std::process::exit(tool::run(&args[2..]));
    }

    // `bish session <subcommand>`: detachable sessions, before the
    // script-path branch too. `--daemon <name>` is the internal bootstrap
    // `session::run_new` re-execs with.
    if args.get(1).map(String::as_str) == Some("session") {
        if !cfg!(feature = "sessions") {
            eprintln!("bish: detachable sessions are not part of this build");
            std::process::exit(2);
        }
        let code = match args.get(2).map(String::as_str) {
            // `-A`: attach if it exists, create it otherwise.
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

    let mut invocation = match Invocation::parse(&args[1..]) {
        Ok(inv) => inv,
        Err(e) => {
            eprintln!("bish: {e}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    // A re-exec'd child takes its script from the environment and removes
    // it: it holds the parent's whole state, which the script or anything it
    // starts could otherwise read. See `exec::REEXEC_SCRIPT`.
    if invocation.reexec {
        invocation.command = Some(std::env::var(exec::REEXEC_SCRIPT).unwrap_or_default());
        // SAFETY: before any thread starts and before a `Shell` reads the
        // environment.
        unsafe { std::env::remove_var(exec::REEXEC_SCRIPT) };
    }
    // `login(1)` and terminal emulators imitating it start a login shell
    // named `-bish` rather than passing `-l`.
    if args.first().is_some_and(|name| name.starts_with('-')) {
        invocation.login = true;
    }
    if invocation.print_version {
        println!("bish {} (bash {} compatible)", env!("CARGO_PKG_VERSION"), exec::BASH_VERSION);
        std::process::exit(0);
    }
    if invocation.print_help {
        println!("{USAGE}");
        std::process::exit(0);
    }

    let mut shell = exec::Shell::new();
    shell.started_as_login = invocation.login;
    shell.login_shell = invocation.login;
    // `$SHLVL` counts shell nesting: inherited plus one, as bash does, for
    // anything counting depth.
    let depth = shell.lookup_var("SHLVL").trim().parse::<i64>().unwrap_or(0);
    // Set directly rather than by running `export SHLVL=N`, which would
    // lex, parse and execute a statement at every startup.
    shell.export_var("SHLVL", (depth + 1).to_string());
    for (flag, on) in &invocation.set_flags {
        shell.apply_shell_flag(*flag, *on);
    }
    for (name, on) in &invocation.set_options {
        shell.apply_shell_option(name, *on);
    }

    if let Some(command) = &invocation.command {
        // The last letter of `$-`: `c` here, `s` for a script on stdin, `i`
        // when interactive, nothing for a named script.
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
        // A named script has an outermost `main` frame; `-c` text doesn't
        // (see `Shell::running_a_script`).
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
        // `-s` with arguments: they're the positional parameters, and the
        // script comes from stdin.
        shell.set_script_args("bish".to_string(), invocation.operands.clone());
    }

    if invocation.interactive.unwrap_or_else(|| std::io::stdin().is_terminal()) {
        shell.invocation_flag = Some('i');
        // Reachable from its panes (`session::listen_here`). A bish that
        // can't listen is still a shell.
        if let Ok(path) = session::listen_here() {
            shell.export_var("BISH_SOCKET", path.to_string_lossy().into_owned());
        }
        repl::run(shell, invocation.promoted, !invocation.norc);
        session::stop_listening();
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

// What the command line asked for, in bash's spelling; `--promoted` is
// bish's (see `repl::run`).
#[derive(Default)]
struct Invocation {
    command: Option<String>,
    operands: Vec<String>,
    read_stdin: bool,
    // `None`: decide from whether stdin is a terminal, as a shell without
    // `-i`, `-c` or a script does.
    interactive: Option<bool>,
    login: bool,
    norc: bool,
    promoted: bool,
    reexec: bool,
    print_version: bool,
    print_help: bool,
    set_flags: Vec<(char, bool)>,
    set_options: Vec<(String, bool)>,
}

impl Invocation {
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
                // A re-exec'd child: its script is in the environment (see
                // `exec::REEXEC_SCRIPT`).
                "--reexec" => inv.reexec = true,
                _ if arg.starts_with("--") => return Err(format!("{arg}: unrecognized option")),
                // A cluster of single-letter flags, either sense:
                // `-euo pipefail`, `+x`. A lone `-` or `+` is an operand.
                _ if (arg.starts_with('-') || arg.starts_with('+')) && arg.len() > 1 => {
                    let on = arg.starts_with('-');
                    let mut letters = arg[1..].chars();
                    while let Some(c) = letters.next() {
                        match c {
                            // `c` ends the cluster, the command following,
                            // so `-lc 'echo hi'` works.
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
                // The first non-option word is the script; everything after
                // is the script's.
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

    // `bish -lc 'echo hi'` is how a terminal emulator starts a login shell.
    #[test]
    fn a_cluster_ending_in_c_takes_the_next_argument_as_the_command() {
        let inv = parse(&["-lc", "echo hi"]);
        assert_eq!(inv.command.as_deref(), Some("echo hi"));
        assert!(inv.login);
        assert_eq!(inv.interactive, Some(false), "-c is not interactive");
    }

    // `bish -n script.sh` checks syntax and runs nothing, so `n` must reach
    // `apply_shell_flag` (`-s`, `-i`, `-l`, `-c` and `-o` are consumed here).
    #[test]
    fn n_reaches_the_shell_as_an_ordinary_flag() {
        assert_eq!(parse(&["-n", "s.sh"]).set_flags, vec![('n', true)]);
        assert_eq!(parse(&["-n", "s.sh"]).operands, vec!["s.sh".to_string()]);
        // In a cluster, where `c` ends it: `bish -nc 'echo hi'` runs nothing.
        let inv = parse(&["-nc", "echo hi"]);
        assert_eq!(inv.set_flags, vec![('n', true)]);
        assert_eq!(inv.command.as_deref(), Some("echo hi"));
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

    // The first non-option belongs to the script, and what follows:
    // `bish s.sh -x` passes `-x` to the script.
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

    // A lone `-` is an operand, as for every tool.
    #[test]
    fn a_bare_dash_is_not_a_flag() {
        let inv = parse(&["-"]);
        assert!(inv.set_flags.is_empty());
        assert_eq!(inv.operands, vec!["-".to_string()]);
    }
}

/// `bish --promoted -c '<script>'`: runs the script as a split window
/// does, promoted, output to a pane's grid, and prints what the pane
/// shows. For bashdiff's pane corpus: a pane is a different path (a pty
/// per foreground external, `ExecResult::Fg` handed off, a vt100 grid
/// for a pipe), and running the corpus both ways is how it gets checked.
/// The grid is far larger than any case needs, since output scrolled off
/// a pane is lost.
fn run_source_in_a_pane(shell: &mut exec::Shell, src: &str) -> i32 {
    const ROWS: usize = 400;
    const COLS: usize = 400;
    let screen = std::rc::Rc::new(std::cell::RefCell::new(vt100::Screen::new(ROWS, COLS)));
    shell.set_sink_grid(std::rc::Rc::clone(&screen));
    shell.mark_promoted();
    let status = run_source(shell, src);
    // A line's last command hands a pty job off instead of waiting (see
    // `ExecResult::Fg`); with no repl.rs here, it's drained now, before the
    // grid is read.
    shell.settle_pending_fg();
    // A background job writes to its own pty, which repl.rs empties on its
    // idle tick; nothing does here, so it's drained, twice with a pause,
    // since a just-reaped job may not have written yet.
    for _ in 0..2 {
        shell.drain_background_output();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    shell.drain_background_output();
    print!("{}", screen.borrow().text_unwrapped());
    use std::io::Write;
    let _ = std::io::stdout().flush();
    status
}

// `$BASH_ENV` for a non-interactive shell, the login profile for a
// login one, read before a script or `-c` text, as bash does.
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

fn run_source(shell: &mut exec::Shell, src: &str) -> i32 {
    match lexer::Lexer::new(src).tokenize() {
        Ok(toks) => match parser::Parser::new(shell.expand_aliases(toks)).parse_program() {
            Ok(prog) => {
                if let exec::ExecResult::Exit(code) = shell.run_program_of_source(&prog, src) {
                    // The exit trap already ran where this came from (see
                    // `ExecResult::Exit`).
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
