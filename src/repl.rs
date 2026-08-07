use std::io::{self, Write};

use crate::editor::{self, ReadOutcome};
use crate::exec::Shell;
use crate::history::History;
use crate::lexer::Lexer;
use crate::parser::{AndOr, Command, Parser, Pipeline, Program};
use crate::prompt;
use crate::term;

pub fn run(shell: &mut Shell) {
    // The shell itself must survive Ctrl-C (bash's own top-level
    // interactive behavior); a foreground child still dies/interrupts
    // normally since exec() resets a *caught* signal like this back to
    // default. See term::ignore_sigint's doc comment.
    term::ignore_sigint();

    let mut history = History::load(".bish_history");
    let mut cmd_history = History::load(".bish_cmd_history");

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
                // Restore the normal screen buffer if promotion switched us
                // to the alternate one -- see run_window/promote_if_needed
                // in exec.rs. Exiting all active sessions is currently the
                // only way out of promoted mode (M1 has only one).
                if shell.is_promoted() {
                    print!("\x1b[?1049l");
                    let _ = io::stdout().flush();
                }
                break;
            }
            Ok(ReadOutcome::Interrupted) => {
                // Ctrl-C abandons whatever multi-line construct was
                // pending, same as bash, and starts fresh at a new prompt.
                buffer.clear();
            }
            Ok(ReadOutcome::CommandMode) => {
                run_command_mode(shell, &mut cmd_history);
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

// Command mode: entered via ':' at an empty insert-mode prompt (see
// editor.rs's ReadOutcome::CommandMode). Has its own history, separate
// from the shell's, and only ever runs builtins directly -- `command NAME`
// is the escape hatch for externals (see restrict_to_builtins in exec.rs).
// An empty line, Ctrl-C, or Ctrl-D returns to insert mode, matching vim's
// Ex command-line ':' + empty Enter/Esc.
fn run_command_mode(shell: &mut Shell, history: &mut History) {
    let mut buffer = String::new();
    loop {
        let prompt_str = if buffer.is_empty() { ": ".to_string() } else { prompt::continuation() };

        match editor::read_line(&prompt_str, history) {
            Ok(ReadOutcome::Eof) | Ok(ReadOutcome::Interrupted) => return,
            // ':' at an empty command-mode prompt too -- nothing to switch
            // to, just stay here.
            Ok(ReadOutcome::CommandMode) => {}
            Ok(ReadOutcome::Line(line)) => {
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(&line);

                if buffer.trim().is_empty() {
                    return;
                }

                match Lexer::new(&buffer).tokenize() {
                    Ok(toks) => match Parser::new(toks).parse_program() {
                        Ok(prog) => {
                            if let Some(msg) = command_mode_violation(&prog) {
                                eprintln!("bish: {}", msg);
                                buffer.clear();
                            } else {
                                history.record(&buffer);
                                shell.restrict_to_builtins = true;
                                shell.run_program(&prog);
                                shell.restrict_to_builtins = false;
                                buffer.clear();
                            }
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
                return;
            }
        }
        let _ = io::stdout().flush();
    }
}

// Command mode allows full control-flow syntax (if/while/for/etc -- every
// leaf command still funnels through the same restrict_to_builtins gate
// regardless of nesting) but not `(...)` subshells, coproc, function
// definitions, or multi-stage `|` pipelines: the first three self-exec (or
// register something persistent) rather than running through that gate,
// and would bypass the restriction entirely; function definitions would
// leak a callable function out into normal shell mode later. Walked
// recursively since these can be nested inside a control-flow body.
fn command_mode_violation(prog: &Program) -> Option<&'static str> {
    prog.iter().find_map(|item| and_or_violation(&item.and_or))
}

fn and_or_violation(ao: &AndOr) -> Option<&'static str> {
    pipeline_violation(&ao.first).or_else(|| ao.rest.iter().find_map(|(_, p)| pipeline_violation(p)))
}

fn pipeline_violation(p: &Pipeline) -> Option<&'static str> {
    if p.commands.len() > 1 {
        return Some("multi-stage pipelines ('|') aren't allowed in command mode");
    }
    p.commands.iter().find_map(command_violation)
}

fn command_violation(c: &Command) -> Option<&'static str> {
    match c {
        Command::Subshell(..) => Some("subshells ('(...)') aren't allowed in command mode"),
        Command::Coproc { .. } => Some("coproc isn't allowed in command mode"),
        Command::FuncDef { .. } => Some("function definitions aren't allowed in command mode"),
        Command::Simple(_) | Command::Arith(..) | Command::Test(..) => None,
        Command::If { branches, else_branch, .. } => branches
            .iter()
            .find_map(|(cond, body)| command_mode_violation(cond).or_else(|| command_mode_violation(body)))
            .or_else(|| else_branch.as_ref().and_then(|b| command_mode_violation(b))),
        Command::While { cond, body, .. } => command_mode_violation(cond).or_else(|| command_mode_violation(body)),
        Command::For { body, .. } | Command::Select { body, .. } | Command::CFor { body, .. } => command_mode_violation(body),
        Command::Case { arms, .. } => arms.iter().find_map(|(_, body, _)| command_mode_violation(body)),
        Command::Group(body, _) => command_mode_violation(body),
    }
}
