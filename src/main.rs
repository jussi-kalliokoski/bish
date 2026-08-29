mod archive;
mod arith;
mod bishedit;
mod browser;
mod builtins;
mod compgen;
mod csscolor;
mod debugger;
mod diff;
mod docs;
mod editor;
mod exec;
mod fileeditor;
mod git;
mod glob;
mod hexedit;
mod history;
mod html;
mod inflate;
mod json;
mod lexer;
mod markdown;
mod pager;
mod parser;
mod poll;
mod prompt;
mod pty;
mod regex;
mod repl;
mod serialize;
mod session;
mod term;
mod tool;
mod vt100;

use std::io::{IsTerminal, Read};

fn main() {
    let mut args: Vec<String> = std::env::args().collect();

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
            Some("new") => match args.get(3) {
                Some(name) => session::run_new(name),
                None => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session new <name>")),
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
            Some("kill") => match args.get(3) {
                Some(name) => session::run_kill(name),
                None => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session kill <name>")),
            },
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "usage: bish session {new|attach|ls|kill} <name>")),
        };
        match code {
            Ok(c) => std::process::exit(c),
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    }

    // `bish --promoted`: starts the interactive REPL already promoted into
    // the windowed compositor (see repl::run's own doc comment on this
    // param) instead of waiting for the first window-family command/
    // Ctrl+Space. Only meaningful for the interactive branch below, so
    // this is checked and stripped before any of `-c`/a script path/piped
    // stdin get their turn at args[1] -- same "leading flag reserved
    // ahead of everything else" treatment as `tool` just above. Harmless
    // (silently ignored, `start_promoted` just never gets used) if
    // combined with one of those non-interactive forms.
    let start_promoted = args.get(1).map(String::as_str) == Some("--promoted");
    if start_promoted {
        args.remove(1);
    }

    let mut shell = exec::Shell::new();

    if args.len() >= 3 && args[1] == "-c" {
        let script_name = args.get(3).cloned().unwrap_or_else(|| "bish".to_string());
        let positional = args.get(4..).map(|s| s.to_vec()).unwrap_or_default();
        shell.set_script_args(script_name, positional);
        std::process::exit(run_source(&mut shell, &args[2]));
    }

    if args.len() >= 2 {
        let path = &args[1];
        shell.set_script_args(path.clone(), args[2..].to_vec());
        match std::fs::read_to_string(path) {
            Ok(src) => std::process::exit(run_source(&mut shell, &src)),
            Err(e) => {
                eprintln!("bish: {}: {}", path, e);
                std::process::exit(1);
            }
        }
    }

    if std::io::stdin().is_terminal() {
        load_config(&mut shell);
        repl::run(shell, start_promoted);
    } else {
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_ok() {
            std::process::exit(run_source(&mut shell, &src));
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
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("bish: {}: {}", path.display(), e),
    }
}

fn run_source(shell: &mut exec::Shell, src: &str) -> i32 {
    match lexer::Lexer::new(src).tokenize() {
        Ok(toks) => match parser::Parser::new(toks).parse_program() {
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
