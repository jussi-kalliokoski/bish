use std::io::{self, BufRead, Write};

use crate::exec::Shell;
use crate::lexer::Lexer;
use crate::parser::Parser;

pub fn run(shell: &mut Shell) {
    let stdin = io::stdin();
    loop {
        print!("ash> ");
        let _ = io::stdout().flush();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                println!();
                break;
            }
            Ok(_) => {
                let trimmed = line.trim_end_matches('\n');
                if trimmed.trim().is_empty() {
                    continue;
                }
                match Lexer::new(trimmed).tokenize() {
                    Ok(toks) => match Parser::new(toks).parse_program() {
                        Ok(prog) => shell.run_program(&prog),
                        Err(e) => eprintln!("ash: syntax error: {}", e),
                    },
                    Err(e) => eprintln!("ash: syntax error: {}", e),
                }
            }
            Err(e) => {
                eprintln!("ash: error reading input: {}", e);
                break;
            }
        }
    }
}
