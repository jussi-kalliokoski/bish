mod arith;
mod builtins;
mod exec;
mod glob;
mod lexer;
mod parser;
mod regex;
mod repl;
mod serialize;

use std::io::{IsTerminal, Read};

fn main() {
    let args: Vec<String> = std::env::args().collect();
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
        repl::run(&mut shell);
    } else {
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_ok() {
            std::process::exit(run_source(&mut shell, &src));
        }
    }
}

fn run_source(shell: &mut exec::Shell, src: &str) -> i32 {
    match lexer::Lexer::new(src).tokenize() {
        Ok(toks) => match parser::Parser::new(toks).parse_program() {
            Ok(prog) => {
                shell.run_program(&prog);
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
