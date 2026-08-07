use std::io::{self, Write};

use crate::editor::{self, ReadOutcome};
use crate::exec::Shell;
use crate::history::History;
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::prompt;
use crate::term;

pub fn run(shell: &mut Shell) {
    // The shell itself must survive Ctrl-C (bash's own top-level
    // interactive behavior); a foreground child still dies/interrupts
    // normally since exec() resets a *caught* signal like this back to
    // default. See term::ignore_sigint's doc comment.
    term::ignore_sigint();

    let mut history = History::load();

    // Accumulates lines across a multi-line construct (unclosed if/for/
    // while/case/quote/paren) until the buffered source parses cleanly.
    let mut buffer = String::new();

    loop {
        let prompt_str = if buffer.is_empty() { prompt::render(shell) } else { prompt::continuation() };

        match editor::read_line(&prompt_str, &history) {
            Ok(ReadOutcome::Eof) => {
                if !buffer.is_empty() {
                    eprintln!("bish: syntax error: unexpected end of input");
                }
                shell.run_exit_trap();
                break;
            }
            Ok(ReadOutcome::Interrupted) => {
                // Ctrl-C abandons whatever multi-line construct was
                // pending, same as bash, and starts fresh at a new prompt.
                buffer.clear();
            }
            Ok(ReadOutcome::Line(line)) => {
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(&line);

                if buffer.trim().is_empty() {
                    buffer.clear();
                    continue;
                }

                match Lexer::new(&buffer).tokenize() {
                    Ok(toks) => match Parser::new(toks).parse_program() {
                        Ok(prog) => {
                            // Recorded regardless of the exit status the
                            // command ends up with -- bash and fish both
                            // record what was typed, not what succeeded.
                            history.record(&buffer);
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
        let _ = io::stdout().flush();
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
