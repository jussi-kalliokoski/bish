use std::io::{self, BufRead, Write};

use crate::exec::Shell;
use crate::lexer::Lexer;
use crate::parser::Parser;

pub fn run(shell: &mut Shell) {
    let stdin = io::stdin();
    // Accumulates lines across a multi-line construct (unclosed if/for/
    // while/case/quote/paren) until the buffered source parses cleanly.
    let mut buffer = String::new();

    loop {
        print!("{}", if buffer.is_empty() { "bish> " } else { "> " });
        let _ = io::stdout().flush();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                println!();
                if !buffer.is_empty() {
                    eprintln!("bish: syntax error: unexpected end of input");
                }
                shell.run_exit_trap();
                break;
            }
            Ok(_) => {
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(line.trim_end_matches('\n'));

                if buffer.trim().is_empty() {
                    buffer.clear();
                    continue;
                }

                match Lexer::new(&buffer).tokenize() {
                    Ok(toks) => match Parser::new(toks).parse_program() {
                        Ok(prog) => {
                            shell.run_program(&prog);
                            buffer.clear();
                        }
                        Err(e) => {
                            if !is_incomplete(&e) {
                                eprintln!("bish: syntax error: {}", e);
                                buffer.clear();
                            }
                        }
                    },
                    Err(e) => {
                        if !is_incomplete(&e) {
                            eprintln!("bish: syntax error: {}", e);
                            buffer.clear();
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("bish: error reading input: {}", e);
                break;
            }
        }
    }
}

// Distinguishes "needs more lines" (unterminated quote/paren, or the parser
// ran out of tokens expecting a closing keyword like `fi`/`done`) from a
// genuine syntax error, by checking for the exact phrasing this crate's own
// lexer/parser error messages use for those cases. Every parser error that
// stems from running out of tokens ends in "None" (`format!("{:?}", other)`
// on an `Option<Tok>` that was `None`), whether it came through the
// `expect()` helper ("expected KwDo, got None") or parse_list_until's own
// message ("...expected one of [...]" -- ends with the debug-printed stop
// list, not "None", hence the separate check).
fn is_incomplete(err: &str) -> bool {
    err.starts_with("unterminated") || err.contains("unexpected end of input") || err.ends_with("None")
}
