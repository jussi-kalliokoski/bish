mod builtins;
mod exec;
mod lexer;
mod parser;
mod repl;

use std::io::{IsTerminal, Read};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut shell = exec::Shell::new();

    if args.len() >= 3 && args[1] == "-c" {
        std::process::exit(run_source(&mut shell, &args[2]));
    }

    if args.len() >= 2 {
        let path = &args[1];
        match std::fs::read_to_string(path) {
            Ok(src) => std::process::exit(run_source(&mut shell, &src)),
            Err(e) => {
                eprintln!("ash: {}: {}", path, e);
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
                shell.last_status
            }
            Err(e) => {
                eprintln!("ash: syntax error: {}", e);
                2
            }
        },
        Err(e) => {
            eprintln!("ash: syntax error: {}", e);
            2
        }
    }
}
