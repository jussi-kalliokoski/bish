use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::rc::Rc;

use crate::bishedit::completion;
use crate::bishedit::highlight::HighlightContext;
use crate::bishedit::motion;
use crate::bishedit::vimkeys::{KeyOutcome, VimKeys, WindowCmd};
use crate::bishedit::Buffer as BisheditBuffer;
use crate::editor::{self, Key, ReadOutcome};
use crate::exec::{self, ExecResult, PaneDirection, Shell, WindowAction};
use crate::history::{self, History};
use crate::lexer::Lexer;
use crate::parser::{AndOr, Command, Parser, Pipeline, Program};
use crate::prompt;
use crate::pty;
use crate::term;
use crate::vt100;

type SessionId = u32;

// One virtual shell session -- either the original process's own session
// (the root, id 0) or one created by `window new` (via Shell::
// new_virtual_child). Each has its own insert-mode multi-line
// continuation buffer, its own History (a persistent, independently-
// diverging chain -- see History's own doc comment for why forking one
// per session, rather than sharing one with a per-session cutoff index,
// is what actually keeps sibling panes' commands from leaking into each
// other's Up/Down browsing), and its own VT100 grid: before promotion
// the grid sits empty and unused (the session's Shell writes straight to
// the real terminal); after promotion every session's output is
// captured into its own grid (see apply_window_action), so switching
// `window`s can redraw whatever that window last drew instead of
// showing stale/real-terminal content.
struct SessionState {
    shell: Shell,
    buffer: String,
    history: History,
    screen: Rc<RefCell<vt100::Screen>>,
    // Set once EOF has already warned this session about stopped jobs
    // (see the ReadOutcome::Eof handler) without actually exiting --
    // cleared the moment a real command runs, so a *second*, immediate
    // Ctrl-D confirms the exit the same way real bash's "There are
    // stopped jobs." + a second EOF does, while typing anything else
    // first re-arms the warning.
    warned_stopped_jobs: bool,
    // Browser-style directory history for Alt+Left/Right (see
    // push_dir_history and the ReadOutcome::DirNav handler): every
    // directory this session's cwd has actually changed to (detected by
    // diffing Shell::cwd before/after a command runs -- catches `cd`
    // however it was invoked, not just a literal `cd` command), with
    // dir_history_index marking "where we are" in that list. Seeded with
    // the session's starting directory so Alt+Left works immediately,
    // before any `cd` at all.
    dir_history: Vec<std::path::PathBuf>,
    dir_history_index: usize,
    // Every command mode has actually run in this session (successfully
    // parsed and executed, regardless of its own exit status -- a
    // command_mode_violation/syntax error never reaches here, matching
    // how `history` itself only ever records something that parsed),
    // across every separate ':' invocation -- see run_command_mode's own
    // doc comment for why this outlives any one invocation. Viewed via
    // Ctrl-L (render_command_transcript), both while composing a command
    // mode line and, once one's just run, from normal mode's own
    // pending-overlay state (run_normal_mode_navigation).
    command_transcript: Vec<TranscriptEntry>,
}

type JobFrameId = u32;

// One layer of a pane's view stack. Session is the vim-like "same
// session shown in multiple windows" case (see WindowEntry's doc
// comment); Job is a fg'd background job, poll-driven against its own
// pty (see exec::FgJob and drive_fg_job below) instead of blocking the
// whole process the way exec.rs's old Shell-local drive_pending_fg did.
// A Job frame holds an id into `job_frames` (owned by repl::run, see
// below) rather than the FgJob itself, since -- unlike a session, which
// can legitimately be the SAME live thing shown in two places at once --
// a running job is inherently a one-place-at-a-time resource, and
// keeping Frame Copy (an FgJob owns a real OS process + pty, definitely
// not Copy) matters for how cheaply `window fg` can duplicate a Session
// frame onto another window's stack.
#[derive(Clone, Copy, PartialEq)]
enum Frame {
    Session(SessionId),
    Job(JobFrameId),
}

type PaneId = u32;

// One pane of a (possibly split) window: exactly what a window's view
// stack used to be before panes existed (see Frame's doc comment) --
// the last (top) entry is what's currently rendered/driven in this
// pane. `window close`/EOF pops the top frame; once a pane's stack
// empties the pane itself closes (see apply_window_action's Close arm),
// collapsing the split, or -- if it was the window's only pane -- falls
// through to the existing "close the whole window" logic unchanged.
struct Pane {
    id: PaneId,
    stack: Vec<Frame>,
}

impl Pane {
    fn owning_session(&self) -> SessionId {
        for frame in self.stack.iter().rev() {
            if let Frame::Session(id) = frame {
                return *id;
            }
        }
        panic!("a pane's stack always has an underlying session frame")
    }
}

// One child of a Split: its own subtree, plus how large a share of the
// split's space it gets relative to its siblings there. Weights are
// relative, not normalized to any fixed total -- compute_regions always
// divides a Split's available space in proportion to weight/(sum of all
// siblings' weights), so a fresh 2-way split with both sides at the
// default 1.0 divides evenly (1/2 each), and `window +`/`-`/`size` (see
// repl.rs's resize_focused_pane/set_focused_pane_size) only ever need
// to change *this* pane's own weight, never touch its siblings' -- the
// division naturally renormalizes around whatever's there.
struct SplitChild {
    layout: PaneLayout,
    weight: f64,
}

// How a window's screen area is currently divided among its panes.
// Leaf is the common case (an unsplit window, or one of a split's
// occupied regions); Split recurses, so nested splits (a horizontal
// divide with one side further vsplit) are represented naturally.
// `horizontal` names the divider's own orientation, matching vim's
// :split (horizontal divider, panes stacked top/bottom) vs :vsplit
// (vertical divider, panes side by side) -- see compute_regions for how
// this becomes actual screen rectangles.
enum PaneLayout {
    Leaf(PaneId),
    Split { horizontal: bool, children: Vec<SplitChild> },
}

// A window: a set of panes (only ever more than one after `window
// split`/`vsplit`) arranged by `layout`, with `focused_pane` marking
// which one currently receives input and drives the prompt/job-loop --
// everything that used to read/write a window's single `stack` now goes
// through `stack()`/`stack_mut()`, which always resolve to the focused
// pane's stack, so the bulk of the surrounding machinery (job driving,
// EOF handling, `window fg`, close) needed no conceptual changes, only
// this one indirection. Since the same SessionId can legally be the top
// of more than one pane's stack at once (across windows via `window
// fg`, or now within one window too), a session is only ever dropped
// from `sessions` once no pane anywhere references it, at any depth --
// see close_orphaned_sessions.
struct WindowEntry {
    id: u32,
    layout: PaneLayout,
    panes: Vec<Pane>,
    focused_pane: PaneId,
    next_pane_id: PaneId,
}

impl WindowEntry {
    fn single(id: u32, initial_frame: Frame) -> WindowEntry {
        WindowEntry {
            id,
            layout: PaneLayout::Leaf(0),
            panes: vec![Pane { id: 0, stack: vec![initial_frame] }],
            focused_pane: 0,
            next_pane_id: 1,
        }
    }

    fn pane(&self, id: PaneId) -> &Pane {
        self.panes.iter().find(|p| p.id == id).expect("pane id always refers to a live pane in this window")
    }

    fn pane_mut(&mut self, id: PaneId) -> &mut Pane {
        self.panes.iter_mut().find(|p| p.id == id).expect("pane id always refers to a live pane in this window")
    }

    fn focused(&self) -> &Pane {
        self.pane(self.focused_pane)
    }

    // The focused pane's own frame stack -- everywhere this used to be
    // a plain field access (`window.stack`) before panes existed.
    fn stack(&self) -> &Vec<Frame> {
        &self.focused().stack
    }

    fn stack_mut(&mut self) -> &mut Vec<Frame> {
        let id = self.focused_pane;
        &mut self.pane_mut(id).stack
    }

    // The nearest Session frame at or below the top of the *focused*
    // pane's stack. In practice this only ever needs to look one level
    // down (a Job frame is always pushed onto a stack that already has
    // a Session beneath it, and nothing currently pushes a *second* Job
    // frame on top of a first), but walks generally rather than
    // assuming that.
    fn owning_session(&self) -> SessionId {
        self.focused().owning_session()
    }
}

// Queries the real terminal's size via the same TIOCGWINSZ ioctl pty.rs
// uses for pty slaves -- it works identically on any tty fd, including
// our own controlling terminal (fd 0). Falls back to a conservative
// default if the query fails (e.g. stdin somehow isn't a tty by the time
// this runs, which main.rs already guards against before calling here).
fn query_term_size() -> (usize, usize) {
    match pty::get_size(0) {
        Ok(ws) if ws.rows > 0 && ws.cols > 0 => (ws.rows as usize, ws.cols as usize),
        _ => (24, 80),
    }
}

fn content_rows(term_rows: usize) -> usize {
    term_rows.saturating_sub(1).max(1)
}

// True if `sid` is reachable from some (window, stack-depth) pair other
// than the exact top frame of `windows[current_window]` -- i.e. whether
// that one reference is the *only* thing keeping the session alive. Used
// by the EOF handler to decide whether closing this one reference would
// actually orphan the session (and so should run its exit trap) or just
// drop one of several still-live references to it (see WindowEntry's
// doc comment on why the same session can appear more than once).
fn session_referenced_elsewhere(windows: &[WindowEntry], current_window: usize, sid: SessionId) -> bool {
    for (i, w) in windows.iter().enumerate() {
        for pane in &w.panes {
            for (depth, frame) in pane.stack.iter().enumerate() {
                let matches = match frame {
                    Frame::Session(s) => *s == sid,
                    Frame::Job(_) => false,
                };
                if !matches {
                    continue;
                }
                let is_the_one_reference_in_question = i == current_window && pane.id == w.focused_pane && depth == pane.stack.len() - 1;
                if !is_the_one_reference_in_question {
                    return true;
                }
            }
        }
    }
    false
}

pub fn run(mut shell: Shell) {
    // The shell itself must survive Ctrl-C (bash's own top-level
    // interactive behavior); a foreground child still dies/interrupts
    // normally since exec() resets a *caught* signal like this back to
    // default. See term::ignore_sigint's doc comment.
    term::ignore_sigint();
    exec::install_winch_handler();
    // Real bash enables job control automatically for an interactive
    // shell -- see Shell::enable_monitor_mode's own doc comment.
    shell.enable_monitor_mode();

    let mut cmd_history = History::load(".bish_cmd_history");

    let (mut term_rows, mut term_cols) = query_term_size();

    let mut sessions: HashMap<SessionId, SessionState> = HashMap::new();
    let root_screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(term_rows), term_cols)));
    let root_cwd = shell.cwd.clone();
    sessions.insert(
        0,
        SessionState {
            shell,
            buffer: String::new(),
            history: History::load(".bish_history"),
            screen: root_screen,
            warned_stopped_jobs: false,
            dir_history: vec![root_cwd],
            dir_history_index: 0,
            command_transcript: Vec::new(),
        },
    );
    let mut windows: Vec<WindowEntry> = vec![WindowEntry::single(0, Frame::Session(0))];
    let mut current_window: usize = 0;
    let mut next_session_id: SessionId = 1;
    let mut next_window_id: u32 = 1;
    // Owns every currently-fg'd job, keyed by the id a Frame::Job on some
    // window's stack points to -- see Frame's doc comment for why a job
    // isn't stored directly in the stack itself.
    let mut job_frames: HashMap<JobFrameId, exec::FgJob> = HashMap::new();
    let mut next_job_frame_id: JobFrameId = 1;
    // Flips true (and stays true) the first time any window-family
    // command promotes the terminal -- see apply_window_action. Every
    // session's sink is Real until then, matching today's plain behavior
    // exactly when `:`/`window` are never invoked.
    let mut sinks_are_grid = false;
    // Set only when run_normal_mode_navigation returns text/cursor to
    // resume editing with (see its own doc comment) -- consumed by the
    // very next editor::read_line call below, then left None again for
    // every ordinary iteration.
    let mut pending_initial: Option<(String, usize)> = None;

    loop {
        // Polled once per loop iteration rather than truly asynchronously:
        // editor::read_line below blocks on the next keypress with no
        // signal-aware select/poll of its own, so a resize that arrives
        // mid-block is only noticed once that read returns (the user's
        // next keystroke or Enter). A real event loop that can react to
        // WINCH *while* blocked on input is M9b's job (the same compositor
        // work that makes poll-driven `fg` possible) -- acknowledged,
        // temporary, same spirit as this codebase's other documented
        // scope boundaries.
        if exec::take_winch() {
            let (new_rows, new_cols) = query_term_size();
            if (new_rows, new_cols) != (term_rows, term_cols) {
                term_rows = new_rows;
                term_cols = new_cols;
                for s in sessions.values() {
                    s.screen.borrow_mut().resize(content_rows(term_rows), term_cols);
                }
                if sinks_are_grid {
                    compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                }
            }
        }

        let session_id = windows[current_window].owning_session();

        // The focused window's top frame is a running job (M10c): it was
        // either just pushed by the fg_pending branch below, or -- the
        // new case this milestone adds -- the user switched back to a
        // window they'd earlier detached from (drive_fg_job's
        // FgOutcome::Detached) while it kept running in the background.
        // Either way, drive it the same way.
        if let Frame::Job(job_frame_id) = *windows[current_window].stack().last().unwrap() {
            run_fg_job_frame(
                job_frame_id,
                session_id,
                &mut sessions,
                &mut windows,
                &mut current_window,
                &mut next_session_id,
                &mut next_window_id,
                &mut job_frames,
                &mut cmd_history,
                &mut sinks_are_grid,
                term_rows,
                term_cols,
            );
            let _ = io::stdout().flush();
            continue;
        }

        // Real (un-promoted) case only: OutputSink::Grid's own vt100
        // emulator already tracks its cursor precisely, and
        // compositor_redraw always repaints from that real state, so
        // this can't come up there -- see Shell::take_needs_newline's
        // own doc comment for what this is fixing (a builtin's output
        // that doesn't end in "\n" -- `printf foo`, `echo -n foo` --
        // otherwise gets silently erased by this same row's own
        // next-prompt redraw, which assumes it already owns whatever's
        // on this row). If an *external* command ran instead (or as part
        // of the same line -- `printf foo; ls`), take_needs_newline
        // can't know whether its own, unobserved output left the cursor
        // mid-row too, so fall back to actually asking the terminal (a
        // real DSR round-trip, more expensive, which is why this isn't
        // done unconditionally on every prompt -- see
        // Shell::take_ran_external's own doc comment). A query that
        // fails/times out (`None`) is treated as "assume no newline
        // needed," the same as before this existed, rather than risking
        // a stall.
        if !sinks_are_grid {
            let shell = &sessions[&session_id].shell;
            let ran_external = shell.take_ran_external();
            // Always drained, even when about to be superseded by the
            // DSR query below -- otherwise a stale `true` left over from
            // a builtin earlier in the same line (`printf foo; ls`) would
            // leak into some *later*, unrelated prompt draw once
            // ran_external stops being true, wrongly inserting a
            // newline that real terminal state no longer calls for.
            let builtin_needs_newline = shell.take_needs_newline();
            let needs_newline =
                if ran_external { term::query_cursor_column().is_some_and(|col| col != 1) } else { builtin_needs_newline };
            if needs_newline {
                print!("\r\n");
                let _ = io::stdout().flush();
            }
        }
        let prompt_str = {
            let session = &sessions[&session_id];
            if session.buffer.is_empty() { prompt::render(&session.shell) } else { prompt::continuation() }
        };
        let (col_origin, width) = focused_col_origin(&windows[current_window], sinks_are_grid, term_rows, term_cols);
        // A standalone snapshot, not a live borrow: on_idle below needs
        // its own mutable borrow of `sessions` (to service other
        // windows' jobs), which would conflict with holding this
        // session's own History borrowed for the whole call otherwise.
        // Nothing recorded elsewhere during this one read_line call
        // needs to be visible mid-browse anyway (see History's own doc
        // comment on what a clone/fork actually shares).
        let session_history = sessions[&session_id].history.clone();
        // Same "standalone snapshot, not a live borrow" reasoning as
        // session_history just above -- on_idle's own &mut sessions borrow
        // below would conflict with borrowing sessions[&session_id].shell's
        // own cwd/functions directly as same-call arguments. Function
        // *names* only, not bodies -- cheap to clone, and all the
        // command-validity check needs (see HighlightContext's own doc
        // comment on why aliases are deliberately not included here too).
        let cwd_snapshot = sessions[&session_id].shell.cwd.clone();
        let known_functions: HashSet<String> = sessions[&session_id].shell.function_names().map(String::from).collect();
        let highlight_ctx = HighlightContext { cwd: Some(cwd_snapshot.as_path()), known_functions: Some(&known_functions) };
        // Same owned-snapshot pattern as highlight_ctx just above -- built
        // from the exact same locals, not re-snapshotted.
        let shell_completion = completion::ShellCompletionProvider { cwd: Some(cwd_snapshot.as_path()), known_functions: Some(&known_functions) };
        // Relative-cursor-row menu tricks (a later stage) are only safe on
        // a single real terminal -- a promoted/split-pane session risks
        // spilling the extra row into a neighboring pane or the tab bar.
        let menu_capable = !sinks_are_grid;

        match editor::read_line(
            &prompt_str,
            &session_history,
            false,
            false,
            pending_initial.take(),
            col_origin,
            width,
            highlight_ctx,
            Some(&shell_completion),
            menu_capable,
            || {
                service_background_jobs(&mut sessions, &mut windows, &mut job_frames, current_window);
            },
        ) {
            Ok(ReadOutcome::Eof) => {
                // Whether closing *this* (window, top-frame) reference
                // would leave the session with no reference anywhere
                // (any window's stack, any depth) -- since `window fg`
                // lets the same session be the top of more than one
                // window's stack, or sit buried under another frame,
                // EOF closing just one of those references shouldn't
                // fire the exit trap for a session that's still alive
                // and reachable elsewhere.
                let will_orphan = !session_referenced_elsewhere(&windows, current_window, session_id);
                let is_final_exit = will_orphan && windows.len() == 1 && windows[current_window].panes.len() == 1 && windows[current_window].stack().len() == 1;
                // Real bash refuses a plain Ctrl-D exit the first time
                // there's a stopped job ("There are stopped jobs."),
                // requiring a second immediate EOF to actually confirm
                // -- see Shell::has_stopped_jobs and
                // SessionState::warned_stopped_jobs' own doc comments.
                // Scoped to the genuine "about to exit the whole
                // process" case: closing one window/frame among several
                // doesn't lose track of any job (the table's shared
                // across every session -- see the `jobs` field comment
                // in exec.rs), so there's nothing to warn about there.
                if is_final_exit {
                    let session = sessions.get_mut(&session_id).unwrap();
                    if session.buffer.is_empty() && session.shell.has_stopped_jobs() && !session.warned_stopped_jobs {
                        session.shell.sink_err("bish: There are stopped jobs.\n");
                        session.warned_stopped_jobs = true;
                        if sinks_are_grid {
                            compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                        }
                        let _ = io::stdout().flush();
                        continue;
                    }
                }
                let session = sessions.get_mut(&session_id).unwrap();
                if !session.buffer.is_empty() {
                    session.shell.sink_err("bish: syntax error: unexpected end of input\n");
                }
                if will_orphan {
                    session.shell.run_exit_trap();
                }
                if is_final_exit {
                    // Last window, last frame anywhere: really exit.
                    // Restore the normal screen buffer if promotion ever
                    // switched us to the alternate one -- exiting every
                    // active session is currently the only way out of
                    // promoted mode.
                    if session.shell.is_promoted() {
                        // Direct, not session.shell.sink_out: this is a
                        // whole-terminal action (leaving the alternate
                        // screen buffer), the same category as
                        // compositor_redraw/apply_window_action's messages
                        // below -- always meant for the real screen, never
                        // a session's own captured output.
                        print!("\x1b[?1049l");
                        let _ = io::stdout().flush();
                    }
                    break;
                }
                // Otherwise: EOF on a window's top frame pops/closes it,
                // same as `window close` would.
                apply_window_action(
                    WindowAction::Close,
                    &mut sessions,
                    &mut windows,
                    &mut current_window,
                    &mut next_session_id,
                    &mut next_window_id,
                    &mut sinks_are_grid,
                    term_rows,
                    term_cols,
                );
            }
            // ctrl_l_reports is false for this call (Ctrl-L at the
            // ordinary shell prompt keeps meaning "clear the real
            // screen," handled inside read_line itself) -- this variant
            // is only ever produced when that flag is true (see its own
            // doc comment), i.e. only for command mode's own nested
            // read_line call in run_command_mode.
            Ok(ReadOutcome::CtrlL) => unreachable!("ctrl_l_reports is false for this read_line call"),
            Ok(ReadOutcome::Interrupted) => {
                // Ctrl-C abandons whatever multi-line construct was
                // pending, same as bash, and starts fresh at a new prompt.
                let session = sessions.get_mut(&session_id).unwrap();
                session.buffer.clear();
                // Same re-arming as a real Line -- Ctrl-C isn't an
                // immediate repeated Ctrl-D either.
                session.warned_stopped_jobs = false;
            }
            Ok(ReadOutcome::DirNav(kind)) => {
                let session = sessions.get_mut(&session_id).unwrap();
                session.warned_stopped_jobs = false;
                navigate_dir(session, kind);
                if sinks_are_grid {
                    compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                }
            }
            Ok(ReadOutcome::NormalMode { text, cursor }) => {
                ensure_promoted(&mut sessions, &mut sinks_are_grid);
                match run_normal_mode_navigation(
                    session_id,
                    &mut sessions,
                    &mut windows,
                    &mut current_window,
                    &mut next_session_id,
                    &mut next_window_id,
                    &mut sinks_are_grid,
                    &mut job_frames,
                    &mut cmd_history,
                    text,
                    cursor,
                    term_rows,
                    term_cols,
                ) {
                    Ok(resume) => pending_initial = resume,
                    Err(e) => {
                        sessions.get(&session_id).unwrap().shell.sink_err(&format!("bish: error reading input: {}\n", e));
                        break;
                    }
                }
            }
            Ok(ReadOutcome::Line(line)) => {
                let mut window_action = None;
                let mut fg_pending = false;
                {
                    let session = sessions.get_mut(&session_id).unwrap();
                    // Any real input -- even a blank Enter -- re-arms the
                    // stopped-jobs exit warning: only an *immediate*
                    // second Ctrl-D, with nothing typed in between,
                    // should confirm the exit (see the Eof handler).
                    session.warned_stopped_jobs = false;

                    // History expansion (!!, !-n, !n, !$, !/prefix,
                    // !?text -- see history::expand's own doc comment)
                    // only applies to the first line of a fresh command,
                    // never a continuation line -- a heredoc body or a
                    // backslash-continued line could legitimately
                    // contain a literal `!`, and there's no "start of
                    // command" to anchor the leading-bang/child-shell
                    // case to partway through one anyway.
                    let line = if session.buffer.is_empty() {
                        match history::expand(&line, &session.history) {
                            Ok(history::Expansion::Substituted(s)) => s,
                            Ok(history::Expansion::UnrecognizedBang(rest)) => format!("({})", rest),
                            Err(msg) => {
                                session.shell.sink_err(&format!("{}\n", msg));
                                if sinks_are_grid {
                                    compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                                }
                                continue;
                            }
                        }
                    } else {
                        line
                    };
                    let session = sessions.get_mut(&session_id).unwrap();

                    // Feed the prompt-and-what-was-typed into the grid
                    // itself, not just the real terminal -- editor::
                    // read_line only ever draws it directly to the real
                    // screen, never through the sink, so without this a
                    // promoted session's next compositor_redraw (a full
                    // clear-and-redraw-from-the-grid -- see that
                    // function's own doc comment) wipes it the instant
                    // any command finishes, even though the command's
                    // own output now renders fine (M11b). This is what
                    // actually makes a promoted window read as a normal
                    // scrolling terminal -- prompt, command, output,
                    // next prompt -- instead of only ever showing the
                    // most recent command's output. Echoes the *expanded*
                    // line (post history::expand), matching bash's own
                    // behavior of showing what a `!!`/`!n`/etc actually
                    // resolved to, not the literal designator typed.
                    // Leading "\r\x1b[K": this pane's grid may already
                    // hold an *idle* prompt on this exact row -- fed in
                    // by freeze_idle_prompt the last time this pane lost
                    // focus (see its own doc comment), with the grid's
                    // cursor left sitting right after it. Without
                    // returning to this row's start and clearing first,
                    // this feed would land right after that stale
                    // prompt instead of overwriting it, showing two
                    // prompts back to back the first time a command runs
                    // in a pane that's regained focus. Safe here in a
                    // way it wouldn't be for the real terminal (see
                    // editor::read_line's col_origin/width): this grid
                    // is one pane's own private, independently-sized
                    // Screen, never shared with another pane's content
                    // the way one real terminal row can be.
                    if sinks_are_grid {
                        let echoed = format!("\r\x1b[K{}{}\r\n", prompt_str, line);
                        session.screen.borrow_mut().feed(echoed.as_bytes());
                    }

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
                                // Snapshotted before recording (not just
                                // before running) so record() can tag this
                                // entry with the directory it's about to
                                // run in, for the suggestions engine's
                                // directory/sequence heuristic -- also
                                // still used below exactly as before, to
                                // detect a directory change regardless of
                                // how it happened (a literal `cd`, a
                                // function that cd's, whatever) rather
                                // than only hooking the `cd` builtin
                                // itself.
                                let cwd_before = session.shell.cwd.clone();
                                // Recorded regardless of the exit status
                                // the command ends up with -- bash and
                                // fish both record what was typed, not
                                // what succeeded.
                                session.history.record(&session.buffer, Some(&cwd_before));
                                let result = session.shell.run_program(&prog);
                                if session.shell.cwd != cwd_before {
                                    push_dir_history(session, session.shell.cwd.clone());
                                }
                                session.buffer.clear();
                                match result {
                                    ExecResult::Window(action) => window_action = Some(action),
                                    ExecResult::Fg => fg_pending = true,
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                if !is_incomplete(&e) {
                                    session.shell.sink_err(&format!("bish: syntax error: {}\n", e));
                                    session.buffer.clear();
                                }
                            }
                        },
                        Err(e) => {
                            if !is_incomplete(&e) {
                                session.shell.sink_err(&format!("bish: syntax error: {}\n", e));
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
                        &mut sinks_are_grid,
                        term_rows,
                        term_cols,
                    );
                } else if fg_pending {
                    // A pty-attached background job was fg'd (see
                    // ExecResult::Fg's doc comment in exec.rs). Push it
                    // as a real Frame::Job on the focused window's own
                    // stack -- see Frame's doc comment for why a Job
                    // frame holds an id into `job_frames` rather than
                    // the FgJob itself -- then drive it via the same
                    // run_fg_job_frame the main loop's own top uses to
                    // re-enter a job left running in a window the user
                    // switched back to (M10c).
                    let fg_job = sessions.get_mut(&session_id).unwrap().shell.take_pending_fg().expect("ExecResult::Fg implies a job was stashed");
                    let job_frame_id = next_job_frame_id;
                    next_job_frame_id += 1;
                    job_frames.insert(job_frame_id, fg_job);
                    windows[current_window].stack_mut().push(Frame::Job(job_frame_id));

                    run_fg_job_frame(
                        job_frame_id,
                        session_id,
                        &mut sessions,
                        &mut windows,
                        &mut current_window,
                        &mut next_session_id,
                        &mut next_window_id,
                        &mut job_frames,
                        &mut cmd_history,
                        &mut sinks_are_grid,
                        term_rows,
                        term_cols,
                    );
                } else if sinks_are_grid {
                    // The common case once promoted: a normal command ran
                    // in the focused session and its output (if any)
                    // landed in that session's grid via its sink -- make
                    // it visible. Nothing streams live mid-command outside
                    // of a poll-driven `fg` (the branch above -- this is
                    // every *other* command); the redraw happens once the
                    // command has fully finished, same as every other
                    // redraw trigger here.
                    compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                }
            }
            Err(e) => {
                sessions.get(&session_id).unwrap().shell.sink_err(&format!("bish: error reading input: {}\n", e));
                break;
            }
        }
        let _ = io::stdout().flush();
    }
}

// Shared by both places command mode can be entered: normal mode's own
// ':' (run_normal_mode_navigation -- the *only* typed-text path now, see
// editor::ReadOutcome::NormalMode's doc comment), and (M10c) the detach
// key firing while a Job frame owns the window instead. Returns the
// underlying CommandModeOutcome so a caller that cares (only normal
// mode's ':' handler does) can tell Quit apart from a command that just
// ran normally -- the job-detach path ignores it, same as it always
// ignored what command mode did before this returned anything.
#[allow(clippy::too_many_arguments)]
fn handle_command_mode(
    session_id: SessionId,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: &mut usize,
    next_session_id: &mut SessionId,
    next_window_id: &mut u32,
    cmd_history: &mut History,
    sinks_are_grid: &mut bool,
    job_frames: &mut HashMap<JobFrameId, exec::FgJob>,
    term_rows: usize,
    term_cols: usize,
) -> CommandModeOutcome {
    let outcome = run_command_mode(session_id, sessions, windows, *current_window, cmd_history, job_frames, term_rows, term_cols);
    match outcome {
        CommandModeOutcome::Action(action) => {
            apply_window_action(action, sessions, windows, current_window, next_session_id, next_window_id, sinks_are_grid, term_rows, term_cols);
        }
        CommandModeOutcome::Quit | CommandModeOutcome::Cancelled | CommandModeOutcome::Ran { .. } => {
            if *sinks_are_grid {
                // No window action, but command mode may still have
                // written a rejected-attempt message straight to this
                // session's grid (a command_mode_violation/syntax error
                // -- see run_command_mode's own doc comment; an ordinary
                // Ran result's own output never touches the grid, it's
                // captured -- see OutputSink::Capture) -- without this,
                // that message would sit written but never actually
                // drawn. Also restores the plain compositor frame under
                // a Ran result before the caller (only normal mode's ':'
                // handler cares) paints its own overlay on top of it --
                // see PendingView's own doc comment.
                compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
            }
        }
    }
    outcome
}

// Drives whatever job is behind `job_frame_id` -- freshly pushed by the
// main loop's fg_pending handling, or already sitting there from an
// earlier detach (M10c) -- via drive_fg_job, and handles however that
// ends: the job exiting pops the frame and records its status, same as
// M10b; the user detaching again just re-inserts the (untouched, still
// running) job back into job_frames and drops straight into command mode
// for this same window, so `window next`/`previous` can move away from
// it without ever having to touch the job itself.
#[allow(clippy::too_many_arguments)]
fn run_fg_job_frame(
    job_frame_id: JobFrameId,
    session_id: SessionId,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: &mut usize,
    next_session_id: &mut SessionId,
    next_window_id: &mut u32,
    job_frames: &mut HashMap<JobFrameId, exec::FgJob>,
    cmd_history: &mut History,
    sinks_are_grid: &mut bool,
    term_rows: usize,
    term_cols: usize,
) {
    // Taken out of job_frames (rather than borrowed via get_mut) so the
    // on_idle closure below can freely borrow job_frames itself to
    // service every *other* window's job -- see service_background_jobs.
    let mut job = job_frames.remove(&job_frame_id).expect("Frame::Job always has a live job_frames entry");
    let focused_screen = sessions[&session_id].screen.clone();
    let tab_bar = tab_bar_line(sessions, windows, *current_window);
    let layout = snapshot_window(&windows[*current_window], sessions, term_rows, term_cols);
    let cw = *current_window;
    let outcome = drive_fg_job(
        &mut job,
        &focused_screen,
        || render_compositor_frame(&layout, &tab_bar, term_rows),
        || service_background_jobs(sessions, windows, job_frames, cw),
    );
    match outcome {
        FgOutcome::Exited(status) => {
            windows[*current_window].stack_mut().pop();
            sessions.get_mut(&session_id).unwrap().shell.last_status = status;
            if *sinks_are_grid {
                compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
            }
        }
        FgOutcome::Detached => {
            job_frames.insert(job_frame_id, job);
            let _ = handle_command_mode(
                session_id,
                sessions,
                windows,
                current_window,
                next_session_id,
                next_window_id,
                cmd_history,
                sinks_are_grid,
                job_frames,
                term_rows,
                term_cols,
            );
        }
        FgOutcome::Stopped => {
            // Unlike Detached, this job doesn't go back into job_frames
            // -- it's no longer "a window's live top frame," it's a
            // genuine Stopped job now, addressable via jobs/fg/bg like
            // any other (see Shell::park_stopped_fg_job).
            windows[*current_window].stack_mut().pop();
            let session = sessions.get_mut(&session_id).unwrap();
            let (id, cmd_text) = session.shell.park_stopped_fg_job(job);
            session.shell.sink_err(&format!("\n[{}]+  Stopped                 {}\n", id, cmd_text));
            session.shell.last_status = 148;
            if *sinks_are_grid {
                compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
            }
        }
    }
}

// Performs a `window`-family action against the real session/window
// state repl.rs owns directly (see ExecResult::Window's doc comment in
// exec.rs for why this can't live inside Shell itself). Redraws the
// compositor afterward for every action, including the rejected-close
// case (its error message needs to actually become visible once
// promoted, same as any other captured output).
#[allow(clippy::too_many_arguments)]
// One-time transition, shared by two independent triggers: every
// window-family command (via apply_window_action -- see its own former
// comment, preserved in spirit here) and, as of bishedit, Ctrl+Space
// entering normal-mode navigation (run_normal_mode_navigation), which
// bypasses the command-dispatch path entirely and so has to trigger this
// itself rather than relying on exec.rs's run_window having already done
// it. `promote_if_needed` (the actual alt-screen switch) is idempotent
// and shared (`Rc<Cell<bool>>`) across every session forked from the same
// root, so calling it here even when exec.rs already did is harmless --
// covers both callers with one unconditional call rather than needing to
// know which case this is. The sink flip (every session's shell writing
// into its own grid instead of straight to the real terminal) still only
// happens once, guarded by `sinks_are_grid` same as before.
fn ensure_promoted(sessions: &mut HashMap<SessionId, SessionState>, sinks_are_grid: &mut bool) {
    if *sinks_are_grid {
        return;
    }
    if let Some(s) = sessions.values_mut().next() {
        s.shell.promote_if_needed();
    }
    for s in sessions.values_mut() {
        let screen = s.screen.clone();
        s.shell.set_sink_grid(screen);
    }
    *sinks_are_grid = true;
}

// Freezes the currently-focused window's idle prompt into its own grid
// (see freeze_idle_prompt's own doc comment) if it's genuinely idle (top
// frame a Session, not a still-running detached job) -- same guard
// split_focused_pane/focus_pane_direction already use for exactly this
// before they move focus elsewhere. Called unconditionally up front by
// apply_window_action (and by run_normal_mode_navigation's own
// GotoFirstWindow/GotoLastWindow handling, which doesn't go through
// apply_window_action at all -- see its own call site), since *any*
// window action might be the one that switches focus away: without this,
// a window's own live prompt -- only ever drawn straight to the real
// terminal by editor::read_line, never captured anywhere on its own --
// is lost the moment focus moves away and something later navigates
// back before typing anything new there (a fresh command's own explicit
// prompt+line grid-feed self-heals the display regardless, which is why
// this went unnoticed until normal mode's own `<C-w>gg`/`<C-w>G` made it
// possible to land on a window and *not* immediately type something).
fn freeze_focused_idle_prompt(sessions: &mut HashMap<SessionId, SessionState>, windows: &[WindowEntry], current_window: usize) {
    if matches!(windows[current_window].stack().last(), Some(Frame::Session(_))) {
        let sid = windows[current_window].owning_session();
        freeze_idle_prompt(sessions.get_mut(&sid).unwrap());
    }
}

fn apply_window_action(
    action: WindowAction,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: &mut usize,
    next_session_id: &mut SessionId,
    next_window_id: &mut u32,
    sinks_are_grid: &mut bool,
    term_rows: usize,
    term_cols: usize,
) {
    ensure_promoted(sessions, sinks_are_grid);
    freeze_focused_idle_prompt(sessions, windows, *current_window);

    match action {
        WindowAction::Next => {
            *current_window = (*current_window + 1) % windows.len();
        }
        WindowAction::Previous => {
            *current_window = (*current_window + windows.len() - 1) % windows.len();
        }
        WindowAction::New => {
            let parent_id = windows[*current_window].owning_session();
            let child_history = sessions[&parent_id].history.fork();
            let mut child_shell = sessions[&parent_id].shell.new_virtual_child();
            let screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(term_rows), term_cols)));
            child_shell.set_sink_grid(screen.clone());
            let child_cwd = child_shell.cwd.clone();
            let sid = *next_session_id;
            *next_session_id += 1;
            sessions.insert(
                sid,
                SessionState {
                    shell: child_shell,
                    buffer: String::new(),
                    // A fork of the parent's own History (see its doc
                    // comment): the new window/pane's Up/Down includes
                    // everything the parent could already see, but from
                    // here on diverges independently -- neither side's
                    // later commands leak into the other's.
                    history: child_history,
                    screen,
                    warned_stopped_jobs: false,
                    dir_history: vec![child_cwd],
                    dir_history_index: 0,
                    command_transcript: Vec::new(),
                },
            );
            let wid = *next_window_id;
            *next_window_id += 1;
            windows.push(WindowEntry::single(wid, Frame::Session(sid)));
            *current_window = windows.len() - 1;
        }
        WindowAction::Close => {
            if windows[*current_window].stack().len() > 1 {
                // Popping a frame off the focused pane's own stack, not
                // closing anything -- always fine regardless of how
                // many windows/panes exist, and never orphans a session
                // since what's revealed underneath was already a live
                // frame.
                windows[*current_window].stack_mut().pop();
            } else if windows[*current_window].panes.len() > 1 {
                // The focused pane's stack is down to just its own
                // session with nothing else to reveal -- but it's one
                // of several panes in a split window, so close just
                // this pane (collapsing the split) rather than falling
                // through to "close the whole window".
                close_focused_pane(&mut windows[*current_window]);
                close_orphaned_sessions(sessions, windows);
            } else if windows.len() == 1 {
                let sid = windows[*current_window].owning_session();
                sessions[&sid].shell.sink_err("bish: window close: cannot close the last window -- exit the shell instead\n");
                compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
                return;
            } else {
                windows.remove(*current_window);
                if *current_window >= windows.len() {
                    *current_window = windows.len() - 1;
                }
                close_orphaned_sessions(sessions, windows);
            }
        }
        WindowAction::FgSession(target_id) => {
            let target_frame = windows.iter().find(|w| w.id == target_id).map(|w| *w.stack().last().unwrap());
            let cur_sid = windows[*current_window].owning_session();
            match target_frame {
                Some(frame @ Frame::Session(_)) => windows[*current_window].stack_mut().push(frame),
                // A running job is a one-place-at-a-time resource (see
                // Frame's doc comment) -- unlike a session, it can't
                // legitimately be shown in two windows at once.
                Some(Frame::Job(_)) => {
                    sessions[&cur_sid].shell.sink_err("bish: window: fg: that window is running a job, not a session\n");
                }
                None => {
                    sessions[&cur_sid].shell.sink_err(&format!("bish: window: fg: no such window: {}\n", target_id));
                }
            }
        }
        WindowAction::Split { horizontal } => {
            split_focused_pane(sessions, windows, *current_window, next_session_id, horizontal, term_rows, term_cols);
        }
        WindowAction::FocusPane(direction) => {
            focus_pane_direction(&mut windows[*current_window], sessions, direction, term_rows, term_cols);
        }
        WindowAction::Balance => {
            balance_panes(&mut windows[*current_window].layout);
        }
        WindowAction::SizeUp => {
            resize_focused_pane(&mut windows[*current_window], RESIZE_STEP);
        }
        WindowAction::SizeDown => {
            resize_focused_pane(&mut windows[*current_window], -RESIZE_STEP);
        }
        WindowAction::SetSize(spec) => {
            set_focused_pane_size(&mut windows[*current_window], spec, term_rows, term_cols);
        }
    }
    compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
}

// Creates a new pane in the current window by splitting the focused one
// in two: the new half holds a freshly cloned session (the same
// session-cloning primitive `window new` uses -- see WindowAction::New
// above) and takes focus, matching tmux's own "split creates a new
// shell and focuses it" convention. See PaneLayout's doc comment for
// how `horizontal` maps to the divider's orientation, and
// insert_sibling for why repeated same-direction splits stay flat (N
// evenly sized panes) rather than progressively halving.
#[allow(clippy::too_many_arguments)]
fn split_focused_pane(
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut [WindowEntry],
    current_window: usize,
    next_session_id: &mut SessionId,
    horizontal: bool,
    term_rows: usize,
    term_cols: usize,
) {
    let parent_id = windows[current_window].owning_session();
    let child_history = sessions[&parent_id].history.fork();
    let mut child_shell = sessions[&parent_id].shell.new_virtual_child();
    let screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(term_rows), term_cols)));
    child_shell.set_sink_grid(screen.clone());
    let child_cwd = child_shell.cwd.clone();
    let sid = *next_session_id;
    *next_session_id += 1;
    sessions.insert(
        sid,
        SessionState {
            shell: child_shell,
            buffer: String::new(),
            // See WindowAction::New's own comment on forking the
            // parent's History instead of starting from "now".
            history: child_history,
            screen,
            warned_stopped_jobs: false,
            dir_history: vec![child_cwd],
            dir_history_index: 0,
            command_transcript: Vec::new(),
        },
    );

    let window = &mut windows[current_window];
    let new_pane_id = window.next_pane_id;
    window.next_pane_id += 1;
    window.panes.push(Pane { id: new_pane_id, stack: vec![Frame::Session(sid)] });

    let focused_id = window.focused_pane;
    let old_layout = std::mem::replace(&mut window.layout, PaneLayout::Leaf(0));
    window.layout = insert_sibling(old_layout, focused_id, new_pane_id, horizontal);

    // The pane being split is about to lose focus to its new sibling --
    // freeze its current idle prompt into its own grid first, or it'll
    // render blank (see freeze_idle_prompt's own doc comment). Only
    // when it's genuinely idle at its own prompt (top frame a Session):
    // splitting can also be reached right after detaching from a job
    // (M10c's Ctrl+Space drops into command mode without popping the
    // Frame::Job -- see run_fg_job_frame's Detached arm), and that
    // pane's grid is already being live-written by the still-running
    // job (service_background_jobs) -- freezing a prompt there would
    // splice a bogus prompt line into the middle of its real output.
    if matches!(windows[current_window].stack().last(), Some(Frame::Session(_))) {
        freeze_idle_prompt(sessions.get_mut(&parent_id).unwrap());
    }
    windows[current_window].focused_pane = new_pane_id;
}

// Captures a pane's *idle* prompt (nothing submitted, just sitting
// there waiting for input) into its own session's grid. Needed only at
// the moment a pane is about to lose focus (see this function's call
// sites): the currently focused pane's live prompt has only ever been
// drawn straight to the real terminal by editor::read_line, never
// captured anywhere -- fine as long as it's the only thing on screen
// (the plain pre-panes case), but the moment a *second*, simultaneously
// visible pane exists, whatever real redraw happens next (sourced only
// from grids -- see render_compositor_frame's own doc comment) would
// show this pane blank unless its idle prompt has been fed into its
// grid first. Mirrors exactly what the main loop's own Line-outcome
// handler already does for a *submitted* command line, just for the
// "about to be left idle, nothing submitted yet" case instead -- and
// deliberately omits that other feed's trailing "\r\n", so the frozen
// grid's cursor lands right after the prompt text, matching a genuinely
// idle prompt waiting for input rather than one that just finished a
// line. Leading "\r\x1b[K": a pane can lose focus more than once over
// its lifetime (e.g. `window k` then later `window j` back), and each
// time this runs the grid's cursor is still sitting wherever the
// *previous* freeze (or submitted line) left it -- without returning
// to this row's start and clearing first, a second freeze would append
// another copy of the prompt right after the first instead of
// overwriting it, showing the same pane's prompt twice once focus
// returns there. Same reasoning, and the same "safe here, not for the
// real terminal" caveat, as the Line-outcome handler's own echo fix.
fn freeze_idle_prompt(session: &mut SessionState) {
    let prompt_str = if session.buffer.is_empty() { prompt::render(&session.shell) } else { prompt::continuation() };
    let framed = format!("\r\x1b[K{}", prompt_str);
    session.screen.borrow_mut().feed(framed.as_bytes());
}

// Same idea as freeze_idle_prompt, but also feeds `text` -- the
// in-progress, not-yet-submitted buffer content -- right after the
// prompt, so run_normal_mode_navigation's pane view shows exactly what's
// already been typed instead of just the bare prompt (see editor::
// ReadOutcome::NormalMode's own doc comment: Ctrl+Space is no longer
// empty-buffer-only). Returns the prompt string it used, so the caller
// can compute `editor::visible_len` on it to know which column `text`
// actually starts at (needed to position the resulting ScreenBuffer's
// cursor correctly -- this function only feeds bytes into the grid, it
// doesn't touch any ScreenBuffer itself).
fn freeze_input_with_text(session: &mut SessionState, text: &str) -> String {
    let prompt_str = if session.buffer.is_empty() { prompt::render(&session.shell) } else { prompt::continuation() };
    let framed = format!("\r\x1b[K{}{}", prompt_str, text);
    session.screen.borrow_mut().feed(framed.as_bytes());
    prompt_str
}

// Finds `target`'s Leaf within the tree and turns it into a 2-way
// Split holding the original pane plus `new_id` -- UNLESS `target` is
// already a direct child of a Split whose own orientation matches
// `horizontal`, in which case `new_id` is simply inserted as another
// sibling of that same Split instead of nesting. That distinction is
// what keeps repeated same-direction splits an even N-way division
// (compute_regions splits one Split's area evenly among however many
// children it has) rather than each new split only ever halving
// whatever was there before.
fn insert_sibling(layout: PaneLayout, target: PaneId, new_id: PaneId, horizontal: bool) -> PaneLayout {
    match layout {
        PaneLayout::Leaf(id) if id == target => PaneLayout::Split {
            horizontal,
            children: vec![SplitChild { layout: PaneLayout::Leaf(id), weight: 1.0 }, SplitChild { layout: PaneLayout::Leaf(new_id), weight: 1.0 }],
        },
        PaneLayout::Leaf(id) => PaneLayout::Leaf(id),
        PaneLayout::Split { horizontal: h, children } => {
            let direct_child_idx = children.iter().position(|c| matches!(&c.layout, PaneLayout::Leaf(id) if *id == target));
            if let Some(idx) = direct_child_idx {
                if h == horizontal {
                    let mut children = children;
                    children.insert(idx + 1, SplitChild { layout: PaneLayout::Leaf(new_id), weight: 1.0 });
                    return PaneLayout::Split { horizontal: h, children };
                }
            }
            let children = children
                .into_iter()
                .map(|c| SplitChild { layout: insert_sibling(c.layout, target, new_id, horizontal), weight: c.weight })
                .collect();
            PaneLayout::Split { horizontal: h, children }
        }
    }
}

// Removes `target`'s Leaf from the tree, collapsing any Split left with
// only one child down to that child directly (so a 2-way split closing
// one side goes back to a plain unsplit Leaf, and a 3-way split closing
// one side stays a 2-way split, not a redundant 1-child Split node --
// the survivors keep whatever weights they already had, same as
// closing a pane never touches its siblings' sizes).
fn remove_from_layout(layout: PaneLayout, target: PaneId) -> Option<PaneLayout> {
    match layout {
        PaneLayout::Leaf(id) => {
            if id == target {
                None
            } else {
                Some(PaneLayout::Leaf(id))
            }
        }
        PaneLayout::Split { horizontal, children } => {
            let new_children: Vec<SplitChild> = children
                .into_iter()
                .filter_map(|c| remove_from_layout(c.layout, target).map(|layout| SplitChild { layout, weight: c.weight }))
                .collect();
            match new_children.len() {
                0 => None,
                1 => new_children.into_iter().next().map(|c| c.layout),
                _ => Some(PaneLayout::Split { horizontal, children: new_children }),
            }
        }
    }
}

fn first_leaf(layout: &PaneLayout) -> PaneId {
    match layout {
        PaneLayout::Leaf(id) => *id,
        PaneLayout::Split { children, .. } => first_leaf(&children[0].layout),
    }
}

// Closes the currently focused pane of a window that has more than one
// -- called only once WindowAction::Close has already confirmed the
// focused pane's own stack has nothing left to reveal underneath (see
// its call site). Picks the new focused pane deterministically (the
// layout tree's own first leaf) rather than trying to guess a
// spatially "nearest" one -- simple and predictable, matching this
// first pane-support pass's plain-layout scope.
fn close_focused_pane(window: &mut WindowEntry) {
    let closing = window.focused_pane;
    let old_layout = std::mem::replace(&mut window.layout, PaneLayout::Leaf(0));
    window.layout = remove_from_layout(old_layout, closing).expect("closing one of >1 panes always leaves at least one behind");
    window.panes.retain(|p| p.id != closing);
    window.focused_pane = first_leaf(&window.layout);
}

// A session stops being referenced when the last window whose stack
// contained it (at any depth, not just the top -- `window fg` can leave
// it buried under another frame in a different window) closes. Called
// only from the "window itself is removed" branch of Close, above --
// no other action can ever orphan a session (New/FgSession only add
// references; Next/Previous don't touch the stack at all).
fn close_orphaned_sessions(sessions: &mut HashMap<SessionId, SessionState>, windows: &[WindowEntry]) {
    let referenced: std::collections::HashSet<SessionId> = windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .flat_map(|p| {
            p.stack.iter().filter_map(|f| match f {
                Frame::Session(id) => Some(*id),
                Frame::Job(_) => None,
            })
        })
        .collect();
    sessions.retain(|id, _| referenced.contains(id));
}

// Full redraw of the currently-focused window's grid plus the tab bar.
// Always a full clear+redraw rather than a cell/row-level diff: M9a's
// discrete-event redraws (a command finished, a window switch, a resize)
// are infrequent enough that flicker isn't a real concern, and a full
// redraw is trivially correct (self-healing against anything that wrote
// to the real terminal directly in between, like editor.rs's Ctrl-L).
// Poll-driven `fg` (see drive_fg_job below) is the first truly
// continuous redraw loop this codebase has -- still full-redraw for now,
// since a real cell/row diff is an optimization, not a correctness
// requirement, and this keeps both redraw paths sharing one
// implementation (render_compositor_frame) rather than risking them
// drifting apart.
fn compositor_redraw(sessions: &HashMap<SessionId, SessionState>, windows: &[WindowEntry], current_window: usize, term_rows: usize, term_cols: usize) {
    let tab_bar = tab_bar_line(sessions, windows, current_window);
    let layout = snapshot_window(&windows[current_window], sessions, term_rows, term_cols);
    render_compositor_frame(&layout, &tab_bar, term_rows);
}

// How drive_fg_job's blocking loop ended.
enum FgOutcome {
    // The job itself exited or was killed (e.g. by a forwarded Ctrl-C).
    Exited(i32),
    // The user hit the detach key (see the Ctrl+Space comment below)
    // instead of the job exiting. The job is left completely untouched --
    // still running, still the top Frame::Job of whatever window it
    // belongs to -- it's the caller's job to decide what to do next (M10c:
    // fall into command mode so `window next`/`previous` can switch away
    // from it while it keeps running in the background).
    Detached,
    // The job stopped (M11b: a forwarded Ctrl-Z reached its own pty's
    // line discipline, or an explicit `kill -STOP`) rather than exiting.
    // Unlike Detached, the caller needs to do real bookkeeping here --
    // see run_fg_job_frame's own handling -- since this job is no longer
    // "the thing this window is actively watching," it's a genuine
    // Stopped job now (exec.rs's Shell::park_stopped_fg_job).
    Stopped,
}

// Drives a job pushed as a Frame::Job: reads its pty master and feeds
// bytes into `screen`, calls `redraw` after every batch of output, and
// forwards raw bytes read from bish's own stdin straight to the job's pty
// master unmodified -- the job's own pty slave termios (never touched by
// this shell) handles things like Ctrl-C triggering real SIGINT delivery
// to the job's process group exactly like a real terminal would, no
// signal-forwarding code needed here. Moved here from exec.rs's old
// Shell::drive_pending_fg (M9b) now that the job itself is owned by
// repl.rs (see FgJob's doc comment) rather than stashed inside a
// session's Shell.
//
// `on_idle` is called on every tick that finds neither job output nor
// stdin input ready (M10c) -- repl.rs's caller uses it to keep other
// windows' backgrounded jobs alive (see service_background_jobs) while
// this one owns the terminal, the same way editor::read_line's own
// on_idle does for a window sitting at a plain prompt.
//
// A raw NUL byte (Ctrl+Space in most terminals) is intercepted before
// being forwarded and means "detach": plan.md long anticipated Ctrl+Space
// as a control-mode trigger alongside ':', and reusing it here for
// "hand control back to the window manager without touching the job"
// keeps that a single, consistent gesture rather than inventing a second
// one. Still no detach-and-*resume*-a-*stopped* job (matching this
// codebase's existing "no SIGTSTP/Ctrl-Z, no genuine Stopped jobs" scope,
// see exec.rs's run_bg doc comment) -- detaching here never stops the
// job, it just stops *this shell* from actively watching it, exactly
// like switching a real terminal multiplexer pane away from a running
// program.
fn drive_fg_job(job: &mut exec::FgJob, screen: &Rc<RefCell<vt100::Screen>>, mut redraw: impl FnMut(), mut on_idle: impl FnMut()) -> FgOutcome {
    use std::io::{Read, Write};
    use std::os::unix::io::AsRawFd;

    pty::set_nonblocking(job.pty_master().as_raw_fd());
    let _raw_guard = term::RawGuard::enable(0).ok();

    let mut buf = [0u8; 4096];
    // Caps how many chunks get drained per outer-loop tick before
    // forcing a redraw/stdin-check/exit-check. A firehose job (`yes` is
    // the extreme case) can keep the pty's read buffer topped up
    // indefinitely -- reading "until WouldBlock" against a producer like
    // that means WouldBlock may never actually arrive, starving this
    // loop's other responsibilities forever. Bounding the drain
    // guarantees this loop always comes back around.
    const MAX_READS_PER_TICK: u32 = 16;
    let outcome = loop {
        // Drain *before* checking whether the job has ended -- a fast,
        // freshly-spawned foreground command (M11b: `echo`, `pwd`, ...
        // aren't builtins, so they hit this exact path now, not just a
        // job that was already backgrounded for a while before being
        // fg'd) can write its entire output and exit within microseconds
        // of being spawned. Checking poll_untraced first would let this
        // loop break on the very first iteration, before ever draining
        // the pty -- discarding output that's already sitting in the
        // kernel's pty buffer, silently. Draining first (every
        // iteration, not just once) means that buffer always gets a
        // chance to be read before this loop can decide to stop,
        // regardless of how fast the job finishes.
        let mut made_progress = false;
        for _ in 0..MAX_READS_PER_TICK {
            match job.pty_master().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    screen.borrow_mut().feed(&buf[..n]);
                    made_progress = true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        if made_progress {
            redraw();
        }

        match job.poll_untraced() {
            exec::FgWait::Exited(status) => break FgOutcome::Exited(status),
            exec::FgWait::Stopped => break FgOutcome::Stopped,
            exec::FgWait::Running => {}
        }

        // Short poll timeout: paces this loop (avoiding a CPU-spinning
        // busy-wait) while staying responsive to both new job output and
        // new keystrokes.
        let ready = term::stdin_ready(10);
        if ready {
            let n = unsafe { raw_read(0, buf.as_mut_ptr(), buf.len()) };
            if n > 0 {
                if n == 1 && buf[0] == 0 {
                    break FgOutcome::Detached;
                }
                // Ctrl-Z (0x1a): explicitly signaled rather than
                // forwarded -- see FgJob::send_stop's doc comment for
                // why the natural "just forward the byte, let the job's
                // own pty generate SIGTSTP" trick that works for Ctrl-C
                // doesn't work here. Not forwarded as a literal byte
                // either: a real terminal wouldn't hand VSUSP through to
                // the program's stdin as data, only deliver the signal
                // it causes.
                if n == 1 && buf[0] == 0x1a {
                    job.send_stop();
                    continue;
                }
                // A lone ESC (0x1b) might be a genuine Escape keypress,
                // or it might be the first byte of a multi-byte escape
                // sequence (arrow/function/Home/End keys, ...) whose
                // remaining bytes just haven't arrived in this same
                // read yet -- common enough over tmux/nested terminals
                // that this reliably lost arrow keys inside a fg'd job
                // like vim: the lone ESC got forwarded on its own tick,
                // and by the time the rest arrived (a tick or more
                // later, since forwarding here happens immediately per
                // read rather than gathering a whole sequence first),
                // vim's own ESC-vs-sequence disambiguation timeout
                // (ttimeoutlen, ~50ms by default) had already lapsed,
                // so it took the ESC as a real Escape and then
                // interpreted the rest as separate normal-mode keys
                // instead of recognizing the sequence. Waiting briefly
                // here for the rest, and forwarding the whole thing as
                // one write, matches what a real terminal effectively
                // guarantees (a keyboard driver emits an escape
                // sequence's bytes as one contiguous burst).
                let mut seq = buf[..n as usize].to_vec();
                if n == 1 && buf[0] == 0x1b && term::stdin_ready(50) {
                    let more = unsafe { raw_read(0, buf.as_mut_ptr(), buf.len()) };
                    if more > 0 {
                        seq.extend_from_slice(&buf[..more as usize]);
                    }
                }
                // The real keyboard always sends arrow keys in the plain
                // CSI form (ESC [ A/B/C/D), whether they arrived here as
                // one single read or were just gathered above -- re-
                // encode to SS3 (ESC O A/B/C/D) if the job has switched
                // into DECCKM/application cursor-key mode (see Screen::
                // app_cursor_keys' own doc comment for why bish has to
                // do this translation itself rather than the real
                // terminal doing it).
                if screen.borrow().app_cursor_keys {
                    if let [0x1b, b'[', letter @ b'A'..=b'D'] = seq.as_slice() {
                        seq = vec![0x1b, b'O', *letter];
                    }
                }
                let _ = job.pty_master().write_all(&seq);
            } else {
                break FgOutcome::Exited(job.wait());
            }
        } else {
            on_idle();
        }
    };
    drop(_raw_guard);
    redraw();
    outcome
}

// Keeps every OTHER pane's fg'd job alive while (skip_window,
// its focused pane) is the one actually being watched (via
// drive_fg_job) or typed into (via editor::read_line) -- called from
// both of those as their on_idle hook (M10c). A Frame::Job can end up
// in any pane, not just the focused one of its window (the user can
// navigate focus away via `window` h/j/k/l while it's still running),
// so every pane of every window is checked here, not just each
// window's single focused-pane stack. Non-blocking, bounded the same
// way drive_fg_job's own drain is: a firehose producer in a
// backgrounded pane shouldn't be able to make this take arbitrarily
// long before returning control to whichever of the two loops above
// called it.
fn service_background_jobs(
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut [WindowEntry],
    job_frames: &mut HashMap<JobFrameId, exec::FgJob>,
    skip_window: usize,
) {
    use std::io::Read;

    const MAX_READS_PER_TICK: u32 = 16;
    let mut buf = [0u8; 4096];
    for i in 0..windows.len() {
        let pane_ids: Vec<PaneId> = windows[i].panes.iter().map(|p| p.id).collect();
        for pane_id in pane_ids {
            if i == skip_window && pane_id == windows[i].focused_pane {
                continue;
            }
            let job_frame_id = match windows[i].pane(pane_id).stack.last() {
                Some(Frame::Job(id)) => *id,
                _ => continue,
            };
            let sid = windows[i].pane(pane_id).owning_session();
            let screen = sessions[&sid].screen.clone();
            let job = match job_frames.get_mut(&job_frame_id) {
                Some(j) => j,
                None => continue,
            };
            for _ in 0..MAX_READS_PER_TICK {
                match job.pty_master().read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => screen.borrow_mut().feed(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            match job.poll_untraced() {
                exec::FgWait::Exited(status) => {
                    windows[i].pane_mut(pane_id).stack.pop();
                    job_frames.remove(&job_frame_id);
                    sessions.get_mut(&sid).unwrap().shell.last_status = status;
                }
                exec::FgWait::Stopped => {
                    windows[i].pane_mut(pane_id).stack.pop();
                    let job = job_frames.remove(&job_frame_id).unwrap();
                    let (id, cmd_text) = sessions.get_mut(&sid).unwrap().shell.park_stopped_fg_job(job);
                    sessions[&sid].shell.sink_err(&format!("\n[{}]+  Stopped                 {}\n", id, cmd_text));
                    sessions.get_mut(&sid).unwrap().shell.last_status = 148;
                }
                exec::FgWait::Running => {}
            }
        }
    }
}

// Raw stdin read for drive_fg_job. Renamed via link_name so the local
// name doesn't collide with anything already in scope (drive_fg_job also
// imports the `Read` trait, a different namespace, but this keeps the
// pattern consistent with pty.rs's own `c_open` reasoning). Deliberately
// not going through std::io::Stdin here (its internal buffering could
// swallow bytes term::stdin_ready's poll() wouldn't know about) -- same
// reasoning as editor.rs's own read_byte.
unsafe extern "C" {
    #[link_name = "read"]
    fn raw_read(fd: i32, buf: *mut u8, count: usize) -> isize;
}

// A screen-coordinate rectangle within the compositor's content area
// (everything above the pinned tab-bar row), 0-indexed. Produced by
// compute_regions from a window's PaneLayout tree.
#[derive(Clone, Copy)]
struct Rect {
    row: usize,
    col: usize,
    rows: usize,
    cols: usize,
}

// One leaf pane's rendering info, resolved from a window's layout tree
// against the real terminal size: which rectangle it occupies, a live
// (Rc-shared, not a content copy -- see run_fg_job_frame's own comment
// on why that matters for a job's redraw callback) reference to its
// owning session's grid, and whether it's the focused pane (only one
// ever is), which decides where the real cursor ends up.
struct PaneSnapshot {
    rect: Rect,
    screen: Rc<RefCell<vt100::Screen>>,
    focused: bool,
}

// Everything render_compositor_frame needs for one window: its panes'
// resolved rectangles/screens plus the divider line segments between
// them (see compute_regions).
struct CompositorLayout {
    panes: Vec<PaneSnapshot>,
    dividers: Vec<(Rect, bool)>,
}

// Floor under any single child's weight when computing shares below --
// keeps a pane resized all the way down (or a pathological/negative
// weight from ever existing in the first place) from making its own
// share, or another sibling's via a tiny total, degenerate. The actual
// on-screen size still can't go below 1 row/col either way (see the
// `.max(1)` below), this only guards the weight arithmetic itself.
const MIN_PANE_WEIGHT: f64 = 0.05;

// Walks `layout`, splitting `area` among each Split's children in
// proportion to their own weight (reserving one row/col between
// siblings for a divider line) down to each Leaf's own rectangle --
// every child's default weight is 1.0 (see insert_sibling), so a plain,
// never-resized split still divides evenly; `window +`/`-`/`size` (see
// resize_focused_pane/set_focused_pane_size) is what actually changes
// any of this. Whichever child's own share divides the available space
// least evenly (last in iteration order) absorbs the rounding
// remainder, so the total always adds back up to `area`'s own size
// exactly. `dividers` collects the reserved divider strips separately
// (row=true for a horizontal divider line, running left-right; false
// for a vertical one, running top-bottom) so the caller can draw them
// after every pane's own content, rather than each Split trying to draw
// into space a child might otherwise want.
fn compute_regions(layout: &PaneLayout, area: Rect, out: &mut Vec<(PaneId, Rect)>, dividers: &mut Vec<(Rect, bool)>) {
    match layout {
        PaneLayout::Leaf(id) => out.push((*id, area)),
        PaneLayout::Split { horizontal, children } => {
            let n = children.len();
            if n == 0 {
                return;
            }
            let total_weight: f64 = children.iter().map(|c| c.weight.max(MIN_PANE_WEIGHT)).sum();
            if *horizontal {
                // Panes stacked top/bottom; the divider is the horizontal
                // line between them.
                let divider_rows = n - 1;
                let usable = area.rows.saturating_sub(divider_rows);
                let mut row = area.row;
                let mut allocated = 0usize;
                for (i, child) in children.iter().enumerate() {
                    let h = if i + 1 == n {
                        usable.saturating_sub(allocated).max(1)
                    } else {
                        (((usable as f64) * child.weight.max(MIN_PANE_WEIGHT) / total_weight).round() as usize).max(1)
                    };
                    compute_regions(&child.layout, Rect { row, col: area.col, rows: h, cols: area.cols }, out, dividers);
                    row += h;
                    allocated += h;
                    if i + 1 < n {
                        dividers.push((Rect { row, col: area.col, rows: 1, cols: area.cols }, true));
                        row += 1;
                    }
                }
            } else {
                // Panes side by side; the divider is the vertical line
                // between them.
                let divider_cols = n - 1;
                let usable = area.cols.saturating_sub(divider_cols);
                let mut col = area.col;
                let mut allocated = 0usize;
                for (i, child) in children.iter().enumerate() {
                    let w = if i + 1 == n {
                        usable.saturating_sub(allocated).max(1)
                    } else {
                        (((usable as f64) * child.weight.max(MIN_PANE_WEIGHT) / total_weight).round() as usize).max(1)
                    };
                    compute_regions(&child.layout, Rect { row: area.row, col, rows: area.rows, cols: w }, out, dividers);
                    col += w;
                    allocated += w;
                    if i + 1 < n {
                        dividers.push((Rect { row: area.row, col, rows: area.rows, cols: 1 }, false));
                        col += 1;
                    }
                }
            }
        }
    }
}

// (col_origin, width): the real terminal column the given window's
// *focused* pane's own column 0 sits at, and how many columns of that
// row belong to it -- see editor::read_line's own doc comment for why
// both matter. (0, term_cols) -- the whole real terminal row --
// whenever there's nothing to offset by: not yet promoted
// (sinks_are_grid false -- read_line is drawing straight to the plain,
// unpaned real terminal), or promoted but this window isn't split (a
// single pane always owns the terminal's whole own row regardless of
// the window's own position in a next/previous cycle, since only one
// window is ever full-screen at a time).
fn focused_col_origin(window: &WindowEntry, sinks_are_grid: bool, term_rows: usize, term_cols: usize) -> (usize, usize) {
    if !sinks_are_grid || window.panes.len() <= 1 {
        return (0, term_cols);
    }
    let rect = pane_rect(window, window.focused_pane, term_rows, term_cols);
    (rect.col, rect.cols)
}

// The screen rectangle `pane_id` currently occupies within `window`,
// resolved against the real terminal size -- the read-only half of
// what snapshot_window computes, for callers (focused_col_origin, pane
// focus-change handling) that only need one pane's geometry rather than
// every pane's live screen reference too.
fn pane_rect(window: &WindowEntry, pane_id: PaneId, term_rows: usize, term_cols: usize) -> Rect {
    let area = Rect { row: 0, col: 0, rows: content_rows(term_rows), cols: term_cols };
    let mut regions = Vec::new();
    let mut dividers = Vec::new();
    compute_regions(&window.layout, area, &mut regions, &mut dividers);
    regions.into_iter().find(|(id, _)| *id == pane_id).map(|(_, r)| r).expect("pane id always present in its own window's layout")
}

// How much one `window +`/`sizeup` or `-`/`sizedown` press changes the
// focused pane's own weight -- see SplitChild's own doc comment for why
// changing just one pane's weight (not its siblings') is enough to
// resize the whole split: compute_regions always divides by weight
// *share*, so growing one pane's weight relative to an unchanged total
// for the others already shrinks them proportionally.
const RESIZE_STEP: f64 = 0.2;

// Finds the Split node that directly contains `target` as one of its
// own children (not a further-nested grandchild), returning that
// Split's own orientation, a mutable handle onto its children, and
// target's index within them -- everything resize_focused_pane/
// set_focused_pane_size need to read or adjust *only* the target's own
// weight. None if the window isn't split at all (target is the whole
// layout, a bare Leaf with no enclosing Split).
fn find_parent_split_mut(layout: &mut PaneLayout, target: PaneId) -> Option<(bool, &mut Vec<SplitChild>, usize)> {
    match layout {
        PaneLayout::Leaf(_) => None,
        PaneLayout::Split { horizontal, children } => {
            if let Some(idx) = children.iter().position(|c| matches!(&c.layout, PaneLayout::Leaf(id) if *id == target)) {
                return Some((*horizontal, children, idx));
            }
            for child in children.iter_mut() {
                if let Some(found) = find_parent_split_mut(&mut child.layout, target) {
                    return Some(found);
                }
            }
            None
        }
    }
}

// `window +`/`sizeup` (delta > 0) and `-`/`sizedown` (delta < 0): grows
// or shrinks the focused pane's own weight within its immediate parent
// split by RESIZE_STEP, floored at MIN_PANE_WEIGHT so repeated presses
// can never zero it out (compute_regions' own `.max(1)` on the
// resulting *rendered* size is the other half of that guarantee -- a
// pane can get very small, never invisible). A no-op if the window
// isn't split.
fn resize_focused_pane(window: &mut WindowEntry, delta: f64) {
    let focused = window.focused_pane;
    if let Some((_, children, idx)) = find_parent_split_mut(&mut window.layout, focused) {
        children[idx].weight = (children[idx].weight + delta).max(MIN_PANE_WEIGHT);
    }
}

// `window =`/`balance`: resets every pane's weight throughout the
// whole layout tree back to the default 1.0, undoing any accumulated
// `+`/`-`/`size` adjustments at every level, not just the focused
// pane's own immediate split.
fn balance_panes(layout: &mut PaneLayout) {
    if let PaneLayout::Split { children, .. } = layout {
        for child in children.iter_mut() {
            child.weight = 1.0;
            balance_panes(&mut child.layout);
        }
    }
}

// `window size <N>`/`<N>%`/`<N>/<M>`: sets the focused pane's size
// directly, along whichever axis its immediate parent split actually
// divides (rows for a stacked split, columns for a side-by-side one).
// A no-op if the window isn't split.
//
// Panes are sized by *weight*, not a fixed row/col count (see
// SplitChild's own doc comment), so "set the size" here means solving
// for whatever weight produces the requested share: given the parent's
// other children's weights sum to `sum_others` and stay fixed, the
// weight that makes the focused pane's own share equal `fraction` of
// the total is `fraction * sum_others / (1 - fraction)` (algebra: from
// `new / (new + sum_others) = fraction`). `fraction` itself comes from
// whichever form was given -- % and N/M are already a fraction
// directly; a bare character count is converted by first backing out
// the parent split's own total usable space from the focused pane's
// *currently rendered* size and weight ratio (`axis_size * total_weight
// / old_weight`) -- an approximation (it ignores compute_regions' own
// "last child absorbs the rounding remainder" rule), close enough for
// an interactively-adjusted size and self-correcting on the very next
// resize/redraw.
fn set_focused_pane_size(window: &mut WindowEntry, spec: exec::SizeSpec, term_rows: usize, term_cols: usize) {
    let focused = window.focused_pane;
    let current_rect = pane_rect(window, focused, term_rows, term_cols);
    let Some((horizontal, children, idx)) = find_parent_split_mut(&mut window.layout, focused) else {
        return;
    };
    let axis_size = if horizontal { current_rect.rows } else { current_rect.cols };
    let old_weight = children[idx].weight.max(MIN_PANE_WEIGHT);
    let total_weight: f64 = children.iter().map(|c| c.weight.max(MIN_PANE_WEIGHT)).sum();
    let sum_others = (total_weight - old_weight).max(MIN_PANE_WEIGHT);
    let fraction = match spec {
        exec::SizeSpec::Percent(p) => p / 100.0,
        exec::SizeSpec::Fraction(f) => f,
        exec::SizeSpec::Characters(n) => {
            if axis_size == 0 {
                return;
            }
            let approx_total_usable = (axis_size as f64) * total_weight / old_weight;
            (n as f64) / approx_total_usable
        }
    };
    let fraction = fraction.clamp(0.05, 0.95);
    let new_weight = fraction * sum_others / (1.0 - fraction);
    children[idx].weight = new_weight.max(MIN_PANE_WEIGHT);
}

// Resolves a window's layout against the real terminal size into a
// CompositorLayout, resizing every involved session's grid (Screen::
// resize, same primitive SIGWINCH already uses) to match its pane's
// actual rectangle along the way -- a session's grid always matches
// whatever pane last displayed it, the same way a plain unsplit
// window's grid already tracked the whole content area.
fn snapshot_window(window: &WindowEntry, sessions: &HashMap<SessionId, SessionState>, term_rows: usize, term_cols: usize) -> CompositorLayout {
    let area = Rect { row: 0, col: 0, rows: content_rows(term_rows), cols: term_cols };
    let mut regions = Vec::new();
    let mut dividers = Vec::new();
    compute_regions(&window.layout, area, &mut regions, &mut dividers);

    let panes = regions
        .into_iter()
        .map(|(pane_id, rect)| {
            let sid = window.pane(pane_id).owning_session();
            let screen = sessions[&sid].screen.clone();
            let (srows, scols) = screen.borrow().size();
            if (srows, scols) != (rect.rows, rect.cols) {
                screen.borrow_mut().resize(rect.rows, rect.cols);
            }
            PaneSnapshot { rect, screen, focused: pane_id == window.focused_pane }
        })
        .collect();

    CompositorLayout { panes, dividers }
}

// The actual drawing: shared by compositor_redraw (reads the tab bar
// live from `sessions`) and drive_pending_fg's redraw callback (which
// can't hold a live borrow of `sessions` for its whole poll loop -- see
// that call site's comment -- so it passes a tab bar string snapshotted
// once, up front, instead). Every pane in `layout` is drawn into its
// own rectangle, followed by the divider lines between them (see
// compute_regions) -- an unsplit window is just the single-pane case of
// this, no divider lines drawn at all.
fn render_compositor_frame(layout: &CompositorLayout, tab_bar: &str, term_rows: usize) {
    let mut out = String::new();
    out.push_str("\x1b[2J\x1b[H");

    for pane in &layout.panes {
        let screen = pane.screen.borrow();
        for r in 0..pane.rect.rows {
            out.push_str(&format!("\x1b[{};{}H", pane.rect.row + r + 1, pane.rect.col + 1));
            render_row(&mut out, &screen, r, pane.rect.cols);
        }
    }

    for (rect, horizontal) in &layout.dividers {
        draw_divider(&mut out, *rect, *horizontal);
    }

    // Tab bar pinned to the terminal's real last row.
    out.push_str(&format!("\x1b[{};1H\x1b[K", term_rows));
    out.push_str(tab_bar);

    let focused = layout.panes.iter().find(|p| p.focused).expect("exactly one pane is always focused");
    let screen = focused.screen.borrow();
    let (cur_row, cur_col) = screen.cursor();
    out.push_str(&format!("\x1b[{};{}H", focused.rect.row + cur_row + 1, focused.rect.col + cur_col + 1));
    out.push_str(if screen.cursor_visible { "\x1b[?25h" } else { "\x1b[?25l" });

    print!("{}", out);
    let _ = io::stdout().flush();
}

// Draws a plain single-line divider (box-drawing characters, no color)
// along a reserved strip from compute_regions. Nested splits can leave
// a T-junction where one divider meets another; those get whichever
// line is drawn second rather than a proper junction glyph -- a small,
// accepted cosmetic gap for this first pane-support pass rather than
// tracking junction geometry across independently-drawn divider
// segments.
fn draw_divider(out: &mut String, rect: Rect, horizontal: bool) {
    if horizontal {
        out.push_str(&format!("\x1b[{};{}H", rect.row + 1, rect.col + 1));
        out.push_str(&"─".repeat(rect.cols));
    } else {
        for r in 0..rect.rows {
            out.push_str(&format!("\x1b[{};{}H", rect.row + r + 1, rect.col + 1));
            out.push('│');
        }
    }
}

// vim/tmux Ctrl-w-hjkl-style spatial pane focus: moves to the nearest
// other leaf in the requested direction. Among candidates whose
// perpendicular span (row range for Left/Right, column range for
// Up/Down) actually overlaps the focused pane's own -- i.e. a pane
// genuinely alongside it, not just diagonally off in that general
// direction -- the nearest by primary-axis distance wins; a candidate
// with no such overlap is only ever picked if nothing overlapping
// exists. Plain center-to-center distance (summing both axes) would
// pick a diagonal neighbor over a directly-adjacent one whenever the
// diagonal one's center happened to be closer in Manhattan terms --
// wrong the moment panes of very different sizes are adjacent (e.g. a
// wide top pane above a narrow bottom-right one: "left" from the
// bottom-right pane must reach the bottom-left pane beside it, not the
// top pane above-and-to-the-left of it). A no-op if the window isn't
// split (single leaf) or nothing qualifies (e.g. `left` from the
// leftmost pane).
fn focus_pane_direction(window: &mut WindowEntry, sessions: &mut HashMap<SessionId, SessionState>, direction: PaneDirection, term_rows: usize, term_cols: usize) {
    let area = Rect { row: 0, col: 0, rows: content_rows(term_rows), cols: term_cols };
    let mut regions = Vec::new();
    let mut dividers = Vec::new();
    compute_regions(&window.layout, area, &mut regions, &mut dividers);
    if regions.len() <= 1 {
        return;
    }
    let current_rect = regions.iter().find(|(id, _)| *id == window.focused_pane).map(|(_, r)| *r).unwrap();
    let (cx, cy) = rect_center(&current_rect);

    // (id, primary-axis distance, whether its perpendicular span
    // overlaps the focused pane's).
    let mut best: Option<(PaneId, i64, bool)> = None;
    for (id, rect) in &regions {
        if *id == window.focused_pane {
            continue;
        }
        let (ox, oy) = rect_center(rect);
        let (dx, dy) = (ox as i64 - cx as i64, oy as i64 - cy as i64);
        let in_direction = match direction {
            PaneDirection::Left => dx < 0,
            PaneDirection::Right => dx > 0,
            PaneDirection::Up => dy < 0,
            PaneDirection::Down => dy > 0,
        };
        if !in_direction {
            continue;
        }
        let (dist, aligned) = match direction {
            PaneDirection::Left | PaneDirection::Right => (dx.abs(), ranges_overlap(current_rect.row, current_rect.rows, rect.row, rect.rows)),
            PaneDirection::Up | PaneDirection::Down => (dy.abs(), ranges_overlap(current_rect.col, current_rect.cols, rect.col, rect.cols)),
        };
        let better = match best {
            None => true,
            Some((_, best_dist, best_aligned)) => (aligned && !best_aligned) || (aligned == best_aligned && dist < best_dist),
        };
        if better {
            best = Some((*id, dist, aligned));
        }
    }
    if let Some((id, _, _)) = best {
        // The currently focused pane is about to lose focus -- freeze
        // its idle prompt into its own grid first (see
        // freeze_idle_prompt's own doc comment), same as split does --
        // but only if it's genuinely idle (top frame a Session, not a
        // still-running detached job -- see split_focused_pane's own
        // comment on why that case must skip this).
        if matches!(window.stack().last(), Some(Frame::Session(_))) {
            let old_sid = window.owning_session();
            freeze_idle_prompt(sessions.get_mut(&old_sid).unwrap());
        }
        window.focused_pane = id;
    }
}

fn ranges_overlap(a_start: usize, a_len: usize, b_start: usize, b_len: usize) -> bool {
    a_start < b_start + b_len && b_start < a_start + a_len
}

fn rect_center(rect: &Rect) -> (usize, usize) {
    (rect.col + rect.cols / 2, rect.row + rect.rows / 2)
}

fn render_row(out: &mut String, screen: &vt100::Screen, row: usize, cols: usize) {
    let mut last: Option<(vt100::Color, vt100::Color, vt100::CellAttrs)> = None;
    for c in 0..cols {
        let cell = screen.cell(row, c);
        let key = (cell.fg, cell.bg, cell.attrs);
        if last != Some(key) {
            out.push_str(&vt100::sgr_codes(cell.fg, cell.bg, cell.attrs));
            last = Some(key);
        }
        out.push(cell.ch);
    }
    out.push_str("\x1b[0m");
}

// A read-only bishedit::Buffer view over a pane's own rendered content --
// its scrollback (oldest first) followed by its live grid, addressed as
// one flat sequence of lines (see plan.md's own description of this
// adapter). Its own (line, col) navigation cursor is kept separate from
// the Screen's real cursor -- motions here never touch the live
// session's actual content or cursor, purely read-only navigation over a
// snapshot view. Marks (m/`/') are its own small addition on top of the
// trait's baseline; nothing else in this milestone needs per-buffer
// storage like this yet.
struct ScreenBuffer {
    screen: Rc<RefCell<vt100::Screen>>,
    cursor: (usize, usize),
    vtop: usize,
    vheight: usize,
    marks: HashMap<char, (usize, usize)>,
}

impl ScreenBuffer {
    fn new(screen: Rc<RefCell<vt100::Screen>>, vheight: usize) -> ScreenBuffer {
        let (sb_len, cur_row, cur_col) = {
            let s = screen.borrow();
            let (row, col) = s.cursor();
            (s.scrollback.len(), row, col)
        };
        // Starts where the live cursor currently is -- translated into
        // this combined addressing, where scrollback lines come first --
        // the same convention tmux copy-mode uses (enter at the current
        // cursor position, not always at the very top).
        let cursor = (sb_len + cur_row, cur_col);
        let vheight = vheight.max(1);
        let vtop = cursor.0.saturating_sub(vheight - 1);
        ScreenBuffer { screen, cursor, vtop, vheight, marks: HashMap::new() }
    }

    // `line`'s own raw cell count -- the live grid's current width for a
    // grid row, or that scrollback row's width *at the time it scrolled
    // off* for a scrollback row, which can differ from the live grid's
    // current width if the terminal was resized since. `char_at`/
    // `line_len` trim trailing blanks off of this.
    fn raw_len(&self, line: usize) -> usize {
        let s = self.screen.borrow();
        let sb_len = s.scrollback.len();
        if line < sb_len {
            s.scrollback[line].len()
        } else {
            s.size().1
        }
    }

    fn raw_char_at(&self, line: usize, col: usize) -> Option<char> {
        let s = self.screen.borrow();
        let sb_len = s.scrollback.len();
        if line < sb_len {
            s.scrollback[line].get(col).map(|c| c.ch)
        } else {
            let row = line - sb_len;
            let (rows, cols) = s.size();
            if row < rows && col < cols {
                Some(s.cell(row, col).ch)
            } else {
                None
            }
        }
    }
}

impl BisheditBuffer for ScreenBuffer {
    // Deliberately *not* scrollback.len() + the grid's full (fixed) row
    // count: a vt100::Grid is always allocated at its full height
    // regardless of how much has actually been written into it, so any
    // rows below wherever the terminal's own live cursor currently sits
    // are genuinely blank padding that scrolling hasn't reached yet, not
    // real content -- counting them let G/j/Ctrl-D/etc. navigate past the
    // actual end of the pane's content into empty space. The live
    // cursor's row is always real content by the time this is read: the
    // one and only ScreenBuffer constructor call site (run_normal_mode_
    // navigation) always calls freeze_idle_prompt immediately before
    // building this, which guarantees something (the prompt text) is
    // written at wherever the cursor is.
    fn line_count(&self) -> usize {
        let s = self.screen.borrow();
        let (cursor_row, _) = s.cursor();
        (s.scrollback.len() + cursor_row + 1).max(1)
    }

    fn line_len(&self, line: usize) -> usize {
        let raw = self.raw_len(line);
        (0..raw).rev().find(|&c| self.raw_char_at(line, c) != Some(' ')).map(|c| c + 1).unwrap_or(0)
    }

    fn char_at(&self, line: usize, col: usize) -> Option<char> {
        if col < self.line_len(line) {
            self.raw_char_at(line, col)
        } else {
            None
        }
    }

    fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    fn set_cursor(&mut self, line: usize, col: usize) {
        self.cursor = (line, col);
    }

    fn viewport_top(&self) -> usize {
        self.vtop
    }

    fn set_viewport_top(&mut self, line: usize) {
        self.vtop = line;
    }

    fn viewport_height(&self) -> usize {
        self.vheight
    }

    fn set_mark(&mut self, name: char, pos: (usize, usize)) {
        self.marks.insert(name, pos);
    }

    fn get_mark(&self, name: char) -> Option<(usize, usize)> {
        self.marks.get(&name).copied()
    }
}

// Adjusts `buf`'s viewport so its navigation cursor's line is visible,
// scrolling as little as possible -- matching vim's own scrolling, which
// only jumps when the cursor would otherwise move off-screen, not
// recentering on every motion.
fn scroll_to_show_cursor(buf: &mut ScreenBuffer) {
    let (line, _) = buf.cursor();
    let height = buf.viewport_height();
    if line < buf.viewport_top() {
        buf.set_viewport_top(line);
    } else if line >= buf.viewport_top() + height {
        buf.set_viewport_top(line + 1 - height);
    }
}

fn render_normal_mode_row(out: &mut String, buf: &ScreenBuffer, line: usize, cols: usize) {
    let mut last: Option<(vt100::Color, vt100::Color, vt100::CellAttrs)> = None;
    let s = buf.screen.borrow();
    let sb_len = s.scrollback.len();
    for c in 0..cols {
        let cell = if line < sb_len {
            s.scrollback[line].get(c).copied().unwrap_or_default()
        } else {
            let row = line - sb_len;
            let (rows, scols) = s.size();
            if row < rows && c < scols {
                s.cell(row, c)
            } else {
                vt100::Cell::default()
            }
        };
        let key = (cell.fg, cell.bg, cell.attrs);
        if last != Some(key) {
            out.push_str(&vt100::sgr_codes(cell.fg, cell.bg, cell.attrs));
            last = Some(key);
        }
        out.push(cell.ch);
    }
    out.push_str("\x1b[0m");
}

// The focused pane's own rectangle is split between the scrollback view
// and a one-row status bar pinned to the rectangle's own last row (see
// render_normal_mode_frame) -- this is how many rows are left for the
// view itself. `.max(1)`: a pane too short to spare a row for status
// still gets *some* content rather than a zero-height, panicking view.
fn normal_mode_content_rows(rect: Rect) -> usize {
    rect.rows.saturating_sub(1).max(1)
}

// The status bar's left side: the search/command line while one is being
// typed (`command_line`, e.g. ":q" or "/foo" as typed so far -- this is
// how `:`/`/`/`?` input actually becomes visible; previously nothing was
// drawn while typing them at all, easily read as "doesn't work"), else
// whatever `vk` has to say about the current key sequence -- a pending
// count/prefix in progress (e.g. "20g" mid-`20gg`), or failing that a
// brief flash of the motion that was just applied (e.g. "[20k]"), else
// just the bare mode indicator. A pending search ('/'/'?') is shown
// alone, without the "-- NORMAL --" prefix, matching vim's own
// command-line convention of replacing the mode indicator outright while
// typing one.
fn normal_mode_status_left(vk: &VimKeys, command_line: Option<&str>) -> String {
    if let Some(cmd) = command_line {
        return cmd.to_string();
    }
    let pending = vk.pending_display();
    if pending.starts_with('/') || pending.starts_with('?') {
        return pending.to_string();
    }
    if !pending.is_empty() {
        return format!("-- NORMAL -- {}", pending);
    }
    let last = vk.last_motion_display();
    if !last.is_empty() {
        return format!("-- NORMAL -- [{}]", last);
    }
    "-- NORMAL --".to_string()
}

fn normal_mode_status_text(buf: &ScreenBuffer, vk: &VimKeys, command_line: Option<&str>, cols: usize) -> String {
    let left = normal_mode_status_left(vk, command_line);
    let (line, col) = buf.cursor();
    let total = buf.line_count();
    let right = format!("{},{}  {}/{}", line + 1, col + 1, line + 1, total);

    let left_len = left.chars().count();
    let right_len = right.chars().count();
    let mut text = left;
    if left_len + right_len < cols {
        text.push_str(&" ".repeat(cols - left_len - right_len));
        text.push_str(&right);
    }
    let text_len = text.chars().count();
    match text_len.cmp(&cols) {
        std::cmp::Ordering::Less => text.push_str(&" ".repeat(cols - text_len)),
        std::cmp::Ordering::Greater => text = text.chars().take(cols).collect(),
        std::cmp::Ordering::Equal => {}
    }
    text
}

// Draws `buf`'s current viewport plus a one-row status bar into `rect`
// (the focused pane's own rectangle -- see pane_rect), reusing sgr_codes
// the same way render_row does for a live pane. Lines past the end of
// the buffer's content are left blank -- vim's own "~" convention for
// that is one more piece of scope this first pass leaves out. Positions
// the real terminal cursor at the navigation cursor's own screen
// location afterward -- not the status bar, so the blinking cursor stays
// where it's actually useful (showing position in the content) even
// while the status bar shows a pending command/search line taking input
// from the very same keystream.
fn render_normal_mode_frame(buf: &ScreenBuffer, rect: Rect, vk: &VimKeys, command_line: Option<&str>) {
    let content_rows = normal_mode_content_rows(rect);
    let total = buf.line_count();
    let mut out = String::new();
    for r in 0..content_rows {
        let line = buf.viewport_top() + r;
        out.push_str(&format!("\x1b[{};{}H", rect.row + r + 1, rect.col + 1));
        if line < total {
            render_normal_mode_row(&mut out, buf, line, rect.cols);
        } else {
            out.push_str(&" ".repeat(rect.cols));
        }
    }

    let status_row = rect.row + content_rows;
    out.push_str(&format!("\x1b[{};{}H", status_row + 1, rect.col + 1));
    out.push_str("\x1b[7m");
    out.push_str(&normal_mode_status_text(buf, vk, command_line, rect.cols));
    out.push_str("\x1b[0m");

    let (cl, cc) = buf.cursor();
    let screen_row = cl.saturating_sub(buf.viewport_top()).min(content_rows.saturating_sub(1));
    let screen_col = cc.min(rect.cols.saturating_sub(1));
    out.push_str(&format!("\x1b[{};{}H", rect.row + screen_row + 1, rect.col + screen_col + 1));
    out.push_str("\x1b[?25h");
    print!("{}", out);
    let _ = io::stdout().flush();
}

// `GotoFirstWindow`/`GotoLastWindow` aren't included -- they don't have a
// `WindowAction` equivalent (an absolute tab-position jump, not a
// repeatable action) and are handled directly in run_normal_mode_
// navigation's own KeyOutcome::Window match, which never calls this for
// them.
fn window_cmd_to_action(cmd: WindowCmd) -> WindowAction {
    match cmd {
        WindowCmd::Next => WindowAction::Next,
        WindowCmd::Previous => WindowAction::Previous,
        WindowCmd::New => WindowAction::New,
        WindowCmd::Close => WindowAction::Close,
        WindowCmd::Split => WindowAction::Split { horizontal: false },
        WindowCmd::VSplit => WindowAction::Split { horizontal: true },
        WindowCmd::FocusLeft => WindowAction::FocusPane(PaneDirection::Left),
        WindowCmd::FocusDown => WindowAction::FocusPane(PaneDirection::Down),
        WindowCmd::FocusUp => WindowAction::FocusPane(PaneDirection::Up),
        WindowCmd::FocusRight => WindowAction::FocusPane(PaneDirection::Right),
        WindowCmd::Balance => WindowAction::Balance,
        WindowCmd::GotoFirstWindow | WindowCmd::GotoLastWindow => {
            unreachable!("handled directly in run_normal_mode_navigation, never reaches here")
        }
    }
}

// What's covering normal mode's own view right now, on top of the
// ordinary render_normal_mode_frame -- neither is an ephemeral "blocks
// until dismissed" prompt the way the old design's overlay-wait was;
// both just sit on screen until the *next* keypress arrives, at which
// point run_normal_mode_navigation's own loop resolves it away (a full
// compositor_redraw + render_normal_mode_frame) and then goes on to
// process that same keypress normally -- so the key that clears an
// Output overlay is never "spent" on dismissing it, it still moves the
// cursor/opens a new ':' line/whatever it would have done anyway. The
// one exception: from Output specifically, Ctrl-L doesn't clear -- it
// upgrades to Transcript instead (see the resolution loop at the top of
// run_normal_mode_navigation), matching command mode's own prompt-time
// Ctrl-L toggle onto the exact same session-wide command_transcript.
#[derive(Clone, Copy, PartialEq)]
enum PendingView {
    None,
    // A just-run command's result (CommandModeOutcome::Ran) -- shown via
    // render_command_output_overlay. Also raised (with empty output) for
    // a non-zero exit that produced no output at all, so a silent
    // failure still gets a bare colored bar instead of no feedback.
    Output,
    Transcript,
}

// bishedit M1's first (and, so far, only) consumer: Ctrl+Space
// (editor::ReadOutcome::NormalMode, unconditional now -- see its own doc
// comment) enters this -- read-only cursor navigation over the focused
// pane's own rendered content (scrollback included), vim's normal-mode
// motions applied via bishedit::motion/vimkeys. `:` hands off to the
// real command-mode feature (handle_command_mode -- the same one the
// M10c job-detach path uses) -- the only way command mode is reached now
// that the old direct ':'-at-the-shell-prompt shortcut is gone (see
// editor::ReadOutcome's own doc comment). `ZZ` and typing exactly
// "q"/"q!" as a command both exit normal mode -- `ZZ` directly here,
// "q"/"q!" via run_command_mode special-casing them as vim's own Ex quit
// commands rather than trying to run them as shell builtins (see its own
// doc comment). Cancelling out of the command line (Ctrl-C/Esc/empty-
// Backspace/empty-Enter) returns to this navigation instead, matching
// real vim staying in Normal mode after an aborted ':' command. An
// ordinary command that actually ran (CommandModeOutcome::Ran) also
// returns to this navigation -- command mode itself is one-shot again
// (see its own doc comment) -- but keeps that command's result on
// screen as a PendingView::Output overlay until the very next keypress.
// `<C-w>{cmd}` (vimkeys' KeyOutcome::Window) reuses the exact same
// exec.rs `WindowAction`/repl.rs `apply_window_action` machinery the
// shell's own `window` command already drives, applied `count` times --
// per plan.md, running one always exits normal mode too (e.g.
// `<C-space><C-w>n` jumps to the next window and drops straight into its
// live prompt, not back into this pane's own normal mode) -- matching
// what a real command-mode `Action` outcome now does as well, for the
// same reason.
//
// `i`/`a`/`I`/`A`/`s`/`S`/`C` (vimkeys' `KeyOutcome::EnterInsert`) all
// return to the live prompt, same as `ZZ` -- but per this session's own
// "as if we were just looking around" design, they act on `initial_text`/
// `initial_cursor` (a snapshot of exactly what was typed and where the
// cursor was the *moment* Ctrl+Space was pressed), never on wherever
// this function's own navigation cursor has since wandered off to in the
// scrollback. So freely glancing around at prior output, then pressing
// e.g. `A`, always resumes editing at the end of the *original* line --
// not at the end of whatever line the nav cursor happens to be sitting
// on. `apply_insert_cmd` (bishedit::vimkeys, shared with editor.rs's own
// line-local Ctrl-E mode -- see that mode's own doc comment for the
// contrast) does the actual text/cursor transformation. `:q` (a same-
// window CommandModeOutcome::Quit) restores the same original text/
// cursor, matching `ZZ`/`i`; an actual focus change (CommandModeOutcome::
// Action, or KeyOutcome::Window) does not -- there's no per-session slot
// to stash "unsubmitted line text" in for whatever window ends up
// focused, so it's simply not carried over (matches today's behavior:
// before this session, Ctrl+Space was empty-buffer-only, so a focus
// change never had anything to lose in the first place).
//
// Not yet resumable via the Frame stack (see plan.md's own scoping
// note): switching to another window mid-navigation and coming back
// later is a natural follow-up, staged the same way job control itself
// was staged across M10a-c. This pass is a simple, blocking "enter,
// navigate, exit" loop -- other windows' backgrounded jobs are still
// kept alive via on_idle (service_background_jobs), but their panes
// aren't repainted live while this loop owns the screen; any staleness
// resolves itself once normal mode exits and the next compositor_redraw
// runs.
#[allow(clippy::too_many_arguments)]
fn run_normal_mode_navigation(
    session_id: SessionId,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: &mut usize,
    next_session_id: &mut SessionId,
    next_window_id: &mut u32,
    sinks_are_grid: &mut bool,
    job_frames: &mut HashMap<JobFrameId, exec::FgJob>,
    cmd_history: &mut History,
    initial_text: String,
    initial_cursor: usize,
    term_rows: usize,
    term_cols: usize,
) -> io::Result<Option<(String, usize)>> {
    let rect = pane_rect(&windows[*current_window], windows[*current_window].focused_pane, term_rows, term_cols);
    let original_chars: Vec<char> = initial_text.chars().collect();
    // Same reasoning as freeze_idle_prompt's other call sites (splitting,
    // switching pane focus): this session's live prompt has only ever
    // been drawn straight to the real terminal by editor::read_line,
    // never captured into its own grid -- fine as long as nothing needs
    // to read that grid back, which is exactly what's about to happen.
    // Ctrl+Space doesn't change focus (unlike those other call sites), so
    // without this the very first entry into normal mode in a session
    // that's never lost focus before would render as a blank pane, not
    // even showing the current prompt (or, now that this isn't empty-
    // buffer-only, whatever had already been typed).
    let prompt_str = freeze_input_with_text(sessions.get_mut(&session_id).unwrap(), &initial_text);
    let screen = sessions[&session_id].screen.clone();
    let mut buf = ScreenBuffer::new(screen, normal_mode_content_rows(rect));
    // Explicitly positioned rather than trusting wherever ScreenBuffer::
    // new's own default (or the vt100 grid's cursor, wherever feed()
    // happened to leave it -- at the *end* of what was just fed, which is
    // only right if the original cursor was already at the end of
    // initial_text too) lands: the navigation cursor should start exactly
    // where editing was interrupted, matching real vim entering Normal
    // mode from Insert.
    let last_line = buf.line_count().saturating_sub(1);
    buf.set_cursor(last_line, editor::visible_len(&prompt_str) + initial_cursor);
    let mut vk = VimKeys::new();

    let _guard = term::RawGuard::enable(0)?;
    // Repaints the whole screen first -- necessary the very first time
    // normal mode ever triggers promotion (the alternate screen buffer
    // starts out blank), harmless otherwise -- then this pane's own
    // rectangle on top of that with the scrollback view.
    compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
    render_normal_mode_frame(&buf, rect, &vk, None);
    let mut pending_view = PendingView::None;

    let resume: Option<(String, usize)> = 'nav: loop {
        let mut key = match editor::read_key_idle(&mut || {
            service_background_jobs(sessions, windows, job_frames, *current_window);
        })? {
            Some(k) => k,
            None => break 'nav None,
        };

        // Resolve whatever PendingView is currently covering the screen
        // before this key does anything else -- see PendingView's own
        // doc comment. A loop, not a single check: Ctrl-L on Output
        // upgrades to Transcript rather than clearing, which still needs
        // a *further* real keypress to resolve away in turn.
        loop {
            match pending_view {
                PendingView::None => break,
                PendingView::Output if key == Key::CtrlL => {
                    pending_view = PendingView::Transcript;
                    render_command_transcript(&sessions[&session_id].command_transcript, term_rows, term_cols);
                    key = match editor::read_key_idle(&mut || {
                        service_background_jobs(sessions, windows, job_frames, *current_window);
                    })? {
                        Some(k) => k,
                        None => break 'nav None,
                    };
                }
                PendingView::Output | PendingView::Transcript => {
                    pending_view = PendingView::None;
                    compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
                    render_normal_mode_frame(&buf, rect, &vk, None);
                    break;
                }
            }
        }

        match key {
            Key::Char('Z') => {
                let k2 = editor::read_key_idle(&mut || {
                    service_background_jobs(sessions, windows, job_frames, *current_window);
                })?;
                if k2 == Some(Key::Char('Z')) {
                    break 'nav Some((initial_text.clone(), initial_cursor));
                }
                continue;
            }
            // ':' isn't routed through vimkeys -- it hands off to the
            // real command-mode feature (see this function's own doc
            // comment), global now rather than pinned to this pane's own
            // rect (see run_command_mode's own doc comment on the
            // "global command mode" redesign) -- run_command_mode
            // positions its own prompt row itself at the top of its own
            // loop, so there's nothing to position here first.
            Key::Char(':') => {
                match handle_command_mode(
                    session_id,
                    sessions,
                    windows,
                    current_window,
                    next_session_id,
                    next_window_id,
                    cmd_history,
                    sinks_are_grid,
                    job_frames,
                    term_rows,
                    term_cols,
                ) {
                    // Matches vim: an aborted/cancelled ':' command drops
                    // back into Normal mode, not out of it entirely.
                    CommandModeOutcome::Cancelled => {
                        render_normal_mode_frame(&buf, rect, &vk, None);
                        continue;
                    }
                    // Same window/session, same as `ZZ`/`i` -- restore
                    // the original text/cursor (see this function's own
                    // doc comment).
                    CommandModeOutcome::Quit => {
                        break 'nav Some((initial_text.clone(), initial_cursor));
                    }
                    // Focus may have changed (apply_window_action already
                    // handled that) -- nothing to resume here (see this
                    // function's own doc comment on why a focus change
                    // doesn't carry the original text over).
                    CommandModeOutcome::Action(_) => {
                        return Ok(None);
                    }
                    // Back to normal mode too, per this session's own
                    // "go back to the original mode with the last output
                    // shown until next keypress" design -- but keep the
                    // result visible via PendingView::Output rather than
                    // just render_normal_mode_frame's ordinary status
                    // bar (see PendingView's own doc comment for how
                    // it's resolved on the very next key). handle_
                    // command_mode has already done the baseline
                    // compositor_redraw this paints on top of.
                    CommandModeOutcome::Ran { output, status } => {
                        if !output.is_empty() || status != 0 {
                            render_command_output_overlay(&output, status, term_rows, term_cols);
                            pending_view = PendingView::Output;
                        } else {
                            render_normal_mode_frame(&buf, rect, &vk, None);
                        }
                        continue;
                    }
                }
            }
            _ => {}
        }

        match vk.feed(key) {
            KeyOutcome::Motion(m, count) => {
                motion::apply_motion(&mut buf, m, count);
                scroll_to_show_cursor(&mut buf);
                render_normal_mode_frame(&buf, rect, &vk, None);
            }
            KeyOutcome::EnterInsert(cmd) => {
                let (new_chars, new_cursor) = crate::bishedit::vimkeys::apply_insert_cmd(&original_chars, initial_cursor, cmd);
                break 'nav Some((new_chars.into_iter().collect(), new_cursor));
            }
            KeyOutcome::Window(cmd @ (WindowCmd::GotoFirstWindow | WindowCmd::GotoLastWindow), count) => {
                // No WindowAction equivalent -- an absolute tab-position
                // jump, not a repeatable action -- so this sets
                // current_window directly instead of going through
                // apply_window_action, matching Motion::GotoFirstLine/
                // GotoLastLine's own default-vs-explicit-target split (see
                // KeyOutcome::Window's own doc comment). Still needs the
                // same freeze apply_window_action itself does up front
                // (see freeze_focused_idle_prompt's own doc comment) --
                // this is the one window-focus-changing path that doesn't
                // go through apply_window_action at all. Focus-changing,
                // same as any other Window outcome -- nothing to resume.
                freeze_focused_idle_prompt(sessions, windows, *current_window);
                let default = if cmd == WindowCmd::GotoFirstWindow { 1 } else { windows.len() };
                let target = count.unwrap_or(default);
                *current_window = target.saturating_sub(1).min(windows.len().saturating_sub(1));
                compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
                return Ok(None);
            }
            KeyOutcome::Window(cmd, count) => {
                let action = window_cmd_to_action(cmd);
                for _ in 0..count.unwrap_or(1).max(1) {
                    apply_window_action(
                        action,
                        sessions,
                        windows,
                        current_window,
                        next_session_id,
                        next_window_id,
                        sinks_are_grid,
                        term_rows,
                        term_cols,
                    );
                }
                // apply_window_action already ends with its own
                // compositor_redraw -- nothing left to draw before
                // returning to the (possibly now different) window's own
                // live prompt. Focus-changing -- nothing to resume.
                return Ok(None);
            }
            // Rendered on every keystroke, not just a resolved Motion --
            // the status bar needs to show a pending count/prefix (e.g.
            // "20g" mid-`20gg`) and a search's in-progress text live, not
            // just the end result once a motion actually applies.
            KeyOutcome::Pending | KeyOutcome::None => {
                render_normal_mode_frame(&buf, rect, &vk, None);
            }
        }
    };

    compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
    Ok(resume)
}

// (window_id, is_the_current_window, that window's session's cwd) -- an
// owned snapshot so drive_pending_fg's redraw callback can build a tab
// bar without holding a live borrow of `sessions` for its whole poll
// loop (see that call site's comment for why it can't).
fn tab_bar_snapshot(sessions: &HashMap<SessionId, SessionState>, windows: &[WindowEntry], current_window: usize) -> Vec<(u32, bool, String)> {
    // Same abbreviation the prompt itself uses (~-substitution, parent
    // components shortened to their first character) -- computed once
    // per redraw rather than per window, since it only depends on $HOME.
    let home = std::env::var("HOME").unwrap_or_default();
    windows
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let cwd = sessions[&w.owning_session()].shell.cwd.to_string_lossy();
            (w.id, i == current_window, prompt::shorten_path(&cwd, &home))
        })
        .collect()
}

fn tab_bar_line(sessions: &HashMap<SessionId, SessionState>, windows: &[WindowEntry], current_window: usize) -> String {
    render_tab_bar(&tab_bar_snapshot(sessions, windows, current_window))
}

fn render_tab_bar(snapshot: &[(u32, bool, String)]) -> String {
    let mut line = String::new();
    for (id, current, cwd) in snapshot {
        if *current {
            line.push_str(&format!("\x1b[7m [{}] {} \x1b[0m ", id, cwd));
        } else {
            line.push_str(&format!(" [{}] {} ", id, cwd));
        }
    }
    line
}

// Records a new "current" directory in this session's back/forward
// history (Alt+Left/Right -- see SessionState::dir_history's own doc
// comment), browser-style: anything reachable by "forward" from here is
// discarded first, since navigating to `dir` fresh (not by walking that
// existing forward stack) makes it stale, the same way a browser drops
// its forward history the instant you follow a new link after going
// back.
fn push_dir_history(session: &mut SessionState, dir: std::path::PathBuf) {
    session.dir_history.truncate(session.dir_history_index + 1);
    session.dir_history.push(dir);
    session.dir_history_index = session.dir_history.len() - 1;
}

// Alt+Left/Right/Up (editor::DirNav -- see its own doc comment). Back/
// Forward walk dir_history without touching it (push_dir_history is
// only for a *fresh* cd); Up computes the real parent of the current
// cwd and treats arriving there as an ordinary fresh navigation (does
// push, and drops any forward history), matching what typing `cd ..`
// itself would do.
fn navigate_dir(session: &mut SessionState, kind: editor::DirNav) {
    match kind {
        editor::DirNav::Back => {
            if session.dir_history_index == 0 {
                return;
            }
            session.dir_history_index -= 1;
            let target = session.dir_history[session.dir_history_index].clone();
            session.shell.cd_to(&target);
        }
        editor::DirNav::Forward => {
            if session.dir_history_index + 1 >= session.dir_history.len() {
                return;
            }
            session.dir_history_index += 1;
            let target = session.dir_history[session.dir_history_index].clone();
            session.shell.cd_to(&target);
        }
        editor::DirNav::Up => {
            if let Some(parent) = session.shell.cwd.parent() {
                let parent = parent.to_path_buf();
                session.shell.cd_to(&parent);
                push_dir_history(session, session.shell.cwd.clone());
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

// A single command mode has actually run (successfully parsed and
// executed -- a command_mode_violation or syntax error never produces
// one of these, matching how `history.record` itself only ever records
// something that parsed). `command` is the buffer text as it was
// actually submitted (post history-expansion, same text that went to
// `history.record`); `output` is the combined stdout+stderr captured
// via OutputSink::Capture (embedded newlines and all -- see render_
// command_output_overlay/render_command_transcript for how multi-line
// entries are laid out); `status` is Shell::last_status right after the
// attempt. Stored in SessionState::command_transcript -- see its own
// doc comment for why this outlives any one ':' invocation.
struct TranscriptEntry {
    command: String,
    output: String,
    status: i32,
}

// Command mode's own row, immediately above the tab bar (see render_
// compositor_frame's own "pinned to the terminal's real last row"
// comment) -- 0-indexed. Global (see run_command_mode's own doc
// comment): not tied to any particular pane's rect, unlike the
// normal-mode status bar (render_normal_mode_frame).
fn command_mode_row(term_rows: usize) -> usize {
    term_rows.saturating_sub(2)
}

// How many rows are free above command mode's own prompt row for the
// output overlay/transcript view to grow into.
fn command_mode_content_rows(term_rows: usize) -> usize {
    command_mode_row(term_rows)
}

// Dark background/light foreground for a successful (exit 0) command's
// output; error background/dark foreground for a failed one -- see
// run_command_mode's own doc comment for the request this implements.
// Deliberately distinct from every other color this crate uses
// (prompt.rs's OK_COLOR/ERR_COLOR only ever recolor a short glyph, never
// a whole line's background) so the overlay reads as its own distinct
// UI surface, not just a tinted prompt.
const OUTPUT_BG: &str = "\x1b[48;5;236m";
const OUTPUT_FG: &str = "\x1b[38;5;253m";
const ERROR_BG: &str = "\x1b[41m";
const ERROR_FG: &str = "\x1b[30m";
// The transcript view's own command-echo lines (render_command_
// transcript): the terminal's ordinary background, bold foreground --
// distinguishes a ":"-command line from the output rows following it
// without needing a third background color.
const CMD_ECHO_BG: &str = "\x1b[49m";
const CMD_ECHO_FG: &str = "\x1b[1m";

// Pads or truncates `text` to exactly `cols` visible columns (plain
// chars only -- callers never feed this pre-colored text) and wraps it
// in the given SGR bg/fg pair, reset at the end -- shared by the output
// overlay and the transcript view so both always paint a full-width,
// unambiguously-edged line rather than leaving stray real-terminal
// content peeking out past a short one.
fn styled_full_width_line(text: &str, bg: &str, fg: &str, cols: usize) -> String {
    let mut out = String::new();
    out.push_str(bg);
    out.push_str(fg);
    let len = text.chars().count();
    if len >= cols {
        out.push_str(&text.chars().take(cols).collect::<String>());
    } else {
        out.push_str(text);
        out.push_str(&" ".repeat(cols - len));
    }
    out.push_str("\x1b[0m");
    out
}

// Shows one command's captured output anchored directly above command
// mode's own prompt row, growing upward -- see run_command_mode's own
// doc comment for the overall UI this is part of. Capped to however
// many rows are actually free above the prompt (command_mode_content_
// rows); a longer output is truncated from the top, with a one-line "N
// more lines above" marker taking the truncated content's place, rather
// than spilling into (and overwriting) the tab bar or wrapping
// unpredictably. Blanks every row above the shown content first --
// self-healing against a previous, taller overlay having left rows
// higher up the screen painted, matching this crate's existing "always
// a full repaint, never a diff" convention (see compositor_redraw's own
// doc comment).
fn render_command_output_overlay(output: &str, status: i32, term_rows: usize, term_cols: usize) {
    let (bg, fg) = if status == 0 { (OUTPUT_BG, OUTPUT_FG) } else { (ERROR_BG, ERROR_FG) };
    // A caller-checked non-zero exit with genuinely empty output (see
    // run_normal_mode_navigation's own doc comment on this) still gets
    // one bare colored row -- `output.lines()` on an empty string yields
    // nothing at all, which would otherwise mean no bar shown.
    let all_lines: Vec<&str> = if output.is_empty() { vec![""] } else { output.lines().collect() };
    let available = command_mode_content_rows(term_rows).max(1);
    let prompt_row = command_mode_row(term_rows);

    let mut shown: Vec<String> = Vec::new();
    if all_lines.len() > available {
        let hidden = all_lines.len() - (available - 1);
        shown.push(format!("... {} more line{} above ...", hidden, if hidden == 1 { "" } else { "s" }));
        shown.extend(all_lines[all_lines.len() - (available - 1)..].iter().map(|s| s.to_string()));
    } else {
        shown.extend(all_lines.iter().map(|s| s.to_string()));
    }

    let start_row = prompt_row.saturating_sub(shown.len());
    let mut out = String::new();
    for r in 0..start_row {
        out.push_str(&format!("\x1b[{};1H", r + 1));
        out.push_str(&" ".repeat(term_cols));
    }
    for (i, line) in shown.iter().enumerate() {
        out.push_str(&format!("\x1b[{};1H", start_row + i + 1));
        out.push_str(&styled_full_width_line(line, bg, fg, term_cols));
    }
    print!("{}", out);
    let _ = io::stdout().flush();
}

// Fills every row above command mode's own prompt row with `transcript`
// (a session's whole SessionState::command_transcript -- every command
// mode has actually run in this session, across every separate ':'
// invocation, not just the current one), tail-aligned (the most recent
// entries at the bottom, right above the prompt) so whatever just ran
// is always visible without scrolling. Toggled by Ctrl-L, either while
// composing a command mode line (run_command_mode) or from the
// PendingView::Output overlay right after one's just run (run_normal_
// mode_navigation) -- see PendingView's own doc comment for how the
// latter is resolved away again.
fn render_command_transcript(transcript: &[TranscriptEntry], term_rows: usize, term_cols: usize) {
    let available = command_mode_content_rows(term_rows).max(1);
    let prompt_row = command_mode_row(term_rows);

    let mut lines: Vec<(String, &'static str, &'static str)> = Vec::new();
    for entry in transcript {
        lines.push((format!(": {}", entry.command), CMD_ECHO_BG, CMD_ECHO_FG));
        let (bg, fg) = if entry.status == 0 { (OUTPUT_BG, OUTPUT_FG) } else { (ERROR_BG, ERROR_FG) };
        for line in entry.output.lines() {
            lines.push((line.to_string(), bg, fg));
        }
    }

    let shown = if lines.len() > available { &lines[lines.len() - available..] } else { &lines[..] };
    let start_row = prompt_row.saturating_sub(shown.len());
    let mut out = String::new();
    for r in 0..start_row {
        out.push_str(&format!("\x1b[{};1H", r + 1));
        out.push_str(&" ".repeat(term_cols));
    }
    for (i, (text, bg, fg)) in shown.iter().enumerate() {
        out.push_str(&format!("\x1b[{};1H", start_row + i + 1));
        out.push_str(&styled_full_width_line(text, bg, fg, term_cols));
    }
    print!("{}", out);
    let _ = io::stdout().flush();
}

// How run_command_mode's one-shot loop ended -- see its own doc comment
// for what each case means. Not Copy (Ran carries a String) -- handle_
// command_mode's own match only ever binds Copy data out of it (Action's
// WindowAction; Ran's own fields are matched by callers, not by handle_
// command_mode itself), so it can still match by value and return the
// same `outcome` afterward without needing Clone/Copy.
#[derive(Debug)]
enum CommandModeOutcome {
    Action(WindowAction),
    // The user typed vim's own ":q"/":q!" -- see run_command_mode's own
    // doc comment for why these are special-cased rather than run as
    // shell commands.
    Quit,
    Cancelled,
    // An ordinary command ran to completion (regardless of its own exit
    // status) -- already appended to the session's own command_
    // transcript by the time this is returned. `output` is the combined
    // stdout+stderr it produced (may be empty); `status` is Shell::
    // last_status right after it ran. The caller (run_normal_mode_
    // navigation) is the one that actually shows this -- see its own
    // doc comment on PendingView for how.
    Ran { output: String, status: i32 },
}

// Command mode: reached only via bishedit normal mode's own ':' now (see
// editor::ReadOutcome::NormalMode's doc comment and run_normal_mode_
// navigation's own ':' handling) -- and, unchanged, via the M10c job-
// detach path. Has its own history, separate from the shell's, and only
// ever runs builtins directly -- `command NAME` is the escape hatch for
// externals (see restrict_to_builtins in exec.rs).
// Renders its own prompt via prompt::command_mode_prompt -- a bare ':',
// deliberately *not* a variant of the normal shell prompt (see that
// function's own doc comment). Global, not pane-relative: always the
// row directly above the tab bar (command_mode_row), spanning the
// terminal's whole width, independent of whichever pane/window happened
// to be focused when it was entered.
// One-shot, matching vim's ':' Ex command line: running an ordinary
// command (Ran) drops straight back to the caller, same as every other
// non-continuing outcome -- what to actually show for it is the
// caller's job (run_normal_mode_navigation keeps it on screen until the
// next keypress; see PendingView), not something worth looping this
// function itself over. An empty line, Ctrl-C, Ctrl-D, or Esc
// (regardless of what's been typed -- see read_line's esc_cancels
// parameter) all cancel out the same way, with nothing run -- and so
// does Backspace at an empty buffer (matching real vim: Backspace on an
// empty Ex line drops back to Normal mode -- see read_line's own
// Key::Backspace arm). Typing exactly "q" or "q!" as the whole
// (complete, non-continuing) line is recognized directly, *before* ever
// reaching the lex/parse/run pipeline -- these are vim's own Ex quit
// commands, not shell builtins. A command_mode_violation or syntax
// error deliberately does *not* return -- it's shown directly (sink_err)
// and the buffer clears, so a typo can be retried at the same prompt
// without re-entering command mode from scratch; neither one is
// recorded to the session's command_transcript (matching how `history`
// itself only ever records something that actually parsed).
// Ctrl-L (ctrl_l_reports, below) toggles a view of the session's own
// command_transcript -- accumulated across every previous ':'
// invocation, not just this one -- while composing this line; toggled
// off the same way, or left as-is when this function returns (the
// caller's own compositor_redraw, wherever it ends up happening, clears
// it).
#[allow(clippy::too_many_arguments)]
fn run_command_mode(
    session_id: SessionId,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: usize,
    history: &mut History,
    job_frames: &mut HashMap<JobFrameId, exec::FgJob>,
    term_rows: usize,
    term_cols: usize,
) -> CommandModeOutcome {
    let mut buffer = String::new();
    let mut transcript_visible = false;
    let prompt_row = command_mode_row(term_rows) + 1;
    // Set only when Ctrl+Space fires below (see that arm's own comment) --
    // consumed by the very next read_line call, then left None again.
    let mut pending_initial: Option<(String, usize)> = None;
    loop {
        let prompt_str = if buffer.is_empty() { prompt::command_mode_prompt() } else { prompt::continuation() };
        print!("\x1b[{};1H", prompt_row);
        let _ = io::stdout().flush();

        // esc_cancels: true -- like a vim ':' command line, Esc (and an
        // empty-buffer Backspace) should back out of command mode the
        // same as Ctrl-C, regardless of what's been typed. ctrl_l_
        // reports: true -- command mode gives Ctrl-L its own meaning
        // (toggling the transcript view, below) rather than the
        // ordinary shell prompt's "clear the real screen."
        // HighlightContext::default() (cwd/known_functions both None) --
        // no single clearly-current session at this call site, and command
        // mode types window-management subcommands, not shell command
        // lines. cwd being None skips file/dir Link detection entirely;
        // known_functions being None doesn't skip command-validity
        // checking (that still runs against builtins/PATH), it just can't
        // recognize a session-specific function as valid there -- a minor,
        // accepted gap given what command mode is actually used for.
        // Flag/subcommand/printf highlighting need neither field, so those
        // work exactly as they do at the ordinary prompt.
        // No meaningful shell-completion context here either, same
        // reasoning as HighlightContext::default() above -- window-
        // management subcommands, not shell command lines. menu_capable:
        // false -- irrelevant with no provider, but also correct on its
        // own merits (see the main prompt loop's own doc comment).
        match editor::read_line(
            &prompt_str,
            history,
            true,
            true,
            pending_initial.take(),
            0,
            term_cols,
            HighlightContext::default(),
            None,
            false,
            &mut || {
                service_background_jobs(sessions, windows, job_frames, current_window);
            },
        ) {
            Ok(ReadOutcome::Eof) | Ok(ReadOutcome::Interrupted) => return CommandModeOutcome::Cancelled,
            // Directory navigation doesn't mean much inside command
            // mode's own restricted context -- just ignore it and keep
            // showing this same prompt, rather than wiring dir_history
            // all the way through here too.
            Ok(ReadOutcome::DirNav(_)) => {}
            // Entering bishedit normal mode from inside command mode
            // isn't a thing (you're already navigating a screen, not a
            // live prompt) -- but unlike DirNav, this can no longer just
            // be ignored outright: Ctrl+Space is unconditional now (see
            // its own doc comment), so simply dropping `text`/`cursor`
            // here would silently discard whatever had already been
            // typed the instant this fired mid-line. Feeding it back as
            // the next read_line call's own `initial` keeps that text
            // right where it was -- the same "don't lose in-progress
            // typing" guarantee every other caller of this gets.
            Ok(ReadOutcome::NormalMode { text, cursor }) => {
                pending_initial = Some((text, cursor));
            }
            Ok(ReadOutcome::CtrlL) => {
                transcript_visible = !transcript_visible;
                if transcript_visible {
                    render_command_transcript(&sessions[&session_id].command_transcript, term_rows, term_cols);
                } else {
                    compositor_redraw(sessions, windows, current_window, term_rows, term_cols);
                }
            }
            Ok(ReadOutcome::Line(line)) => {
                // Same history expansion as the normal shell prompt (see
                // history::expand's own doc comment), scoped the same
                // way: only the first line of a fresh command, against
                // command mode's own separate history. Command mode
                // disallows subshells outright (command_mode_violation
                // below), so an unrecognized leading `!` becomes
                // `command <rest>` here instead of `(<rest>)` -- the
                // same "force it to run as an external" escape hatch
                // command mode already has, not a second one.
                let line = if buffer.is_empty() {
                    match history::expand(&line, history) {
                        Ok(history::Expansion::Substituted(s)) => s,
                        Ok(history::Expansion::UnrecognizedBang(rest)) => format!("command {}", rest),
                        Err(msg) => {
                            sessions[&session_id].shell.sink_err(&format!("{}\n", msg));
                            continue;
                        }
                    }
                } else {
                    line
                };
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(&line);

                let trimmed = buffer.trim().to_string();
                if trimmed.is_empty() {
                    return CommandModeOutcome::Cancelled;
                }
                if trimmed == "q" || trimmed == "q!" {
                    // Recorded same as any other command that actually
                    // ran -- vim's own Ex quit commands, not shell
                    // builtins, but still something the user typed and
                    // command mode acted on, so leaving it out of the
                    // transcript would read as if it never happened.
                    sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                        command: trimmed,
                        output: String::new(),
                        status: 0,
                    });
                    return CommandModeOutcome::Quit;
                }

                match Lexer::new(&buffer).tokenize() {
                    Ok(toks) => match Parser::new(toks).parse_program() {
                        Ok(prog) => {
                            if let Some(msg) = command_mode_violation(&prog) {
                                sessions[&session_id].shell.sink_err(&format!("bish: {}\n", msg));
                                buffer.clear();
                            } else {
                                // No meaningful shell context here -- this
                                // is command mode's own, entirely
                                // separate history of window-management
                                // commands, not shell command lines, so
                                // there's no cwd worth tagging it with
                                // (same reasoning its own read_line call
                                // already uses for HighlightContext::default()
                                // and a None completion provider).
                                history.record(&buffer, None);
                                let (result, captured_text) = {
                                    let session = sessions.get_mut(&session_id).unwrap();
                                    let captured = Rc::new(RefCell::new(String::new()));
                                    session.shell.set_sink_capture(captured.clone());
                                    session.shell.restrict_to_builtins = true;
                                    let result = session.shell.run_program(&prog);
                                    session.shell.restrict_to_builtins = false;
                                    session.shell.set_sink_grid(session.screen.clone());
                                    let text = captured.borrow().clone();
                                    (result, text)
                                };
                                buffer.clear();
                                match result {
                                    ExecResult::Window(action) => {
                                        // Recorded too, same reasoning as
                                        // "q"/"q!" above -- a `window`
                                        // command genuinely ran (its
                                        // WindowAction side effect is
                                        // applied by the caller, handle_
                                        // command_mode, which has the
                                        // session/window state this
                                        // function doesn't), it just
                                        // doesn't drop CommandModeOutcome::
                                        // Ran's own overlay treatment since
                                        // it leaves normal mode entirely
                                        // rather than returning to it.
                                        sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                            command: trimmed,
                                            output: captured_text,
                                            status: 0,
                                        });
                                        return CommandModeOutcome::Action(action);
                                    }
                                    // `fg`'s poll loop needs repl.rs's own
                                    // compositor state, which this
                                    // restricted read-eval loop doesn't
                                    // drive (see Shell::discard_pending_
                                    // fg's doc comment) -- reject it here
                                    // instead of silently leaving the job
                                    // stashed and never driven.
                                    ExecResult::Fg => {
                                        sessions[&session_id].shell.sink_err("bish: fg: not supported in command mode -- use it from the normal shell prompt\n");
                                        sessions.get_mut(&session_id).unwrap().shell.discard_pending_fg();
                                        return CommandModeOutcome::Cancelled;
                                    }
                                    _ => {
                                        let status = sessions[&session_id].shell.last_status;
                                        sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                            command: trimmed,
                                            output: captured_text.clone(),
                                            status,
                                        });
                                        return CommandModeOutcome::Ran { output: captured_text, status };
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            if !is_incomplete(&e) {
                                sessions[&session_id].shell.sink_err(&format!("bish: syntax error: {}\n", e));
                                buffer.clear();
                            }
                        }
                    },
                    Err(e) => {
                        if !is_incomplete(&e) {
                            sessions[&session_id].shell.sink_err(&format!("bish: syntax error: {}\n", e));
                            buffer.clear();
                        }
                    }
                }
            }
            Err(e) => {
                sessions[&session_id].shell.sink_err(&format!("bish: error reading input: {}\n", e));
                return CommandModeOutcome::Cancelled;
            }
        }
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

