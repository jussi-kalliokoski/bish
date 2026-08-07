use std::collections::HashMap;
use std::io::{self, Write};

use crate::editor::{self, ReadOutcome};
use crate::exec::{ExecResult, Shell, WindowAction};
use crate::history::History;
use crate::lexer::Lexer;
use crate::parser::{AndOr, Command, Parser, Pipeline, Program};
use crate::prompt;
use crate::term;

type SessionId = u32;

// One virtual shell session -- either the original process's own session
// (the root, id 0) or one created by `window new` (via Shell::
// new_virtual_child). Each has its own insert-mode multi-line
// continuation buffer and its own history_boundary (see History's doc
// comment): a session only ever browses commands recorded from its own
// creation point forward, never anything from before it existed.
struct SessionState {
    shell: Shell,
    buffer: String,
    history_boundary: usize,
}

// A window's one currently-active session. Deliberately not yet a stack
// of sessions/jobs (the plan's eventual Frame model) -- none of the
// `window` subcommands available so far (next/previous/new/close) ever
// attach an *existing* session to a second window or push an external job
// onto one, so every window has exactly one session for now. The stack
// becomes real once fg-into-a-window's-session and fg-into-an-external-
// job land, alongside the real compositor that can actually make
// switching between them visible.
struct WindowEntry {
    id: u32,
    session: SessionId,
}

pub fn run(shell: Shell) {
    // The shell itself must survive Ctrl-C (bash's own top-level
    // interactive behavior); a foreground child still dies/interrupts
    // normally since exec() resets a *caught* signal like this back to
    // default. See term::ignore_sigint's doc comment.
    term::ignore_sigint();

    let mut history = History::load(".bish_history");
    let mut cmd_history = History::load(".bish_cmd_history");

    let mut sessions: HashMap<SessionId, SessionState> = HashMap::new();
    sessions.insert(0, SessionState { shell, buffer: String::new(), history_boundary: 0 });
    let mut windows: Vec<WindowEntry> = vec![WindowEntry { id: 0, session: 0 }];
    let mut current_window: usize = 0;
    let mut next_session_id: SessionId = 1;
    let mut next_window_id: u32 = 1;

    loop {
        let session_id = windows[current_window].session;
        let boundary = sessions[&session_id].history_boundary;
        let prompt_str = {
            let session = &sessions[&session_id];
            if session.buffer.is_empty() { prompt::render(&session.shell) } else { prompt::continuation() }
        };

        match editor::read_line(&prompt_str, &history, boundary) {
            Ok(ReadOutcome::Eof) => {
                let session = sessions.get_mut(&session_id).unwrap();
                if !session.buffer.is_empty() {
                    eprintln!("bish: syntax error: unexpected end of input");
                }
                session.shell.run_exit_trap();
                if windows.len() == 1 {
                    // Last window, last session: really exit. Restore the
                    // normal screen buffer if promotion ever switched us
                    // to the alternate one -- exiting every active
                    // session is currently the only way out of promoted
                    // mode.
                    if session.shell.is_promoted() {
                        print!("\x1b[?1049l");
                        let _ = io::stdout().flush();
                    }
                    break;
                }
                // Otherwise: EOF on a window's session closes that
                // window, same as `window close` would.
                apply_window_action(
                    WindowAction::Close,
                    &mut sessions,
                    &mut windows,
                    &mut current_window,
                    &mut next_session_id,
                    &mut next_window_id,
                    &history,
                );
            }
            Ok(ReadOutcome::Interrupted) => {
                // Ctrl-C abandons whatever multi-line construct was
                // pending, same as bash, and starts fresh at a new prompt.
                sessions.get_mut(&session_id).unwrap().buffer.clear();
            }
            Ok(ReadOutcome::CommandMode) => {
                let action = {
                    let session = sessions.get_mut(&session_id).unwrap();
                    run_command_mode(&mut session.shell, &mut cmd_history)
                };
                if let Some(action) = action {
                    apply_window_action(
                        action,
                        &mut sessions,
                        &mut windows,
                        &mut current_window,
                        &mut next_session_id,
                        &mut next_window_id,
                        &history,
                    );
                }
            }
            Ok(ReadOutcome::Line(line)) => {
                let mut window_action = None;
                {
                    let session = sessions.get_mut(&session_id).unwrap();
                    if !session.buffer.is_empty() {
                        session.buffer.push('\n');
                    }
                    session.buffer.push_str(&line);

                    if session.buffer.trim().is_empty() {
                        session.buffer.clear();
                        continue;
                    }

                    match Lexer::new(&session.buffer).tokenize() {
                        Ok(toks) => match Parser::new(toks).parse_program() {
                            Ok(prog) => {
                                // Recorded regardless of the exit status
                                // the command ends up with -- bash and
                                // fish both record what was typed, not
                                // what succeeded.
                                history.record(&session.buffer);
                                let result = session.shell.run_program(&prog);
                                session.buffer.clear();
                                if let ExecResult::Window(action) = result {
                                    window_action = Some(action);
                                }
                            }
                            Err(e) => {
                                if !is_incomplete(&e) {
                                    eprintln!("bish: syntax error: {}", e);
                                    session.buffer.clear();
                                }
                            }
                        },
                        Err(e) => {
                            if !is_incomplete(&e) {
                                eprintln!("bish: syntax error: {}", e);
                                session.buffer.clear();
                            }
                        }
                    }
                }
                if let Some(action) = window_action {
                    apply_window_action(
                        action,
                        &mut sessions,
                        &mut windows,
                        &mut current_window,
                        &mut next_session_id,
                        &mut next_window_id,
                        &history,
                    );
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

// Performs a `window`-family action against the real session/window
// state repl.rs owns directly (see ExecResult::Window's doc comment in
// exec.rs for why this can't live inside Shell itself). Redraws the tab
// bar afterward for every action that actually changed something.
#[allow(clippy::too_many_arguments)]
fn apply_window_action(
    action: WindowAction,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: &mut usize,
    next_session_id: &mut SessionId,
    next_window_id: &mut u32,
    history: &History,
) {
    match action {
        WindowAction::Next => {
            *current_window = (*current_window + 1) % windows.len();
        }
        WindowAction::Previous => {
            *current_window = (*current_window + windows.len() - 1) % windows.len();
        }
        WindowAction::New => {
            let parent_id = windows[*current_window].session;
            let child_shell = sessions[&parent_id].shell.new_virtual_child();
            let sid = *next_session_id;
            *next_session_id += 1;
            sessions.insert(sid, SessionState { shell: child_shell, buffer: String::new(), history_boundary: history.boundary() });
            let wid = *next_window_id;
            *next_window_id += 1;
            windows.push(WindowEntry { id: wid, session: sid });
            *current_window = windows.len() - 1;
        }
        WindowAction::Close => {
            if windows.len() == 1 {
                eprintln!("bish: window close: cannot close the last window -- exit the shell instead");
                return;
            }
            let closed_session = windows.remove(*current_window).session;
            sessions.remove(&closed_session);
            if *current_window >= windows.len() {
                *current_window = windows.len() - 1;
            }
        }
    }
    redraw_tab_bar(sessions, windows, *current_window);
}

// Clears the screen and draws a plain tab bar, one entry per window
// (index + that window's session's cwd), the current one highlighted.
// No terminal-size query yet (needs a TIOCGWINSZ ioctl, planned for the
// real compositor), so this can't pin itself to the actual bottom row --
// an acknowledged, temporary limitation, not the final design.
fn redraw_tab_bar(sessions: &HashMap<SessionId, SessionState>, windows: &[WindowEntry], current_window: usize) {
    let mut line = String::new();
    for (i, w) in windows.iter().enumerate() {
        let cwd = sessions[&w.session].shell.cwd.display();
        if i == current_window {
            line.push_str(&format!("\x1b[7m [{}] {} \x1b[0m ", w.id, cwd));
        } else {
            line.push_str(&format!(" [{}] {} ", w.id, cwd));
        }
    }
    print!("\x1b[2J\x1b[H");
    println!("bish window manager\r");
    println!("{}\r", line);
    println!("\r");
    let _ = io::stdout().flush();
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
// Ex command-line ':' + empty Enter/Esc. Returns a WindowAction if the
// command that ran was a `window`-family one, for the caller to apply
// against the real session/window state (which run_command_mode itself
// has no access to).
fn run_command_mode(shell: &mut Shell, history: &mut History) -> Option<WindowAction> {
    let mut buffer = String::new();
    loop {
        let prompt_str = if buffer.is_empty() { ": ".to_string() } else { prompt::continuation() };

        match editor::read_line(&prompt_str, history, 0) {
            Ok(ReadOutcome::Eof) | Ok(ReadOutcome::Interrupted) => return None,
            // ':' at an empty command-mode prompt too -- nothing to switch
            // to, just stay here.
            Ok(ReadOutcome::CommandMode) => {}
            Ok(ReadOutcome::Line(line)) => {
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(&line);

                if buffer.trim().is_empty() {
                    return None;
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
                                let result = shell.run_program(&prog);
                                shell.restrict_to_builtins = false;
                                buffer.clear();
                                if let ExecResult::Window(action) = result {
                                    return Some(action);
                                }
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
                return None;
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
