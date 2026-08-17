use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::rc::Rc;

use crate::bishedit::completion;
use crate::bishedit::highlight::{self, HighlightContext};
use crate::bishedit::motion;
use crate::bishedit::registers::{RegisterShape, RegisterValue, Registers};
use crate::bishedit::suggestion;
use crate::bishedit::textbuffer::TextBuffer;
use crate::bishedit::vimkeys::{KeyOutcome, Op, VimKeys, WindowCmd};
use crate::bishedit::Buffer as BisheditBuffer;
use crate::editor::{self, Key, ReadOutcome};
use crate::exec::{self, ExecResult, PaneDirection, Shell, WindowAction};
use crate::fileeditor;
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
type EditFrameId = u32;

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
// frame onto another window's stack. Edit is `e`'s own equivalent of
// Job -- a builtin editor session (fileeditor::EditSession) that can
// likewise be detached (Ctrl+Space) and resumed later, holding an id
// into `edit_frames` for exactly the same Copy-ness reason (a
// TextBuffer's own content is definitely not Copy either).
#[derive(Clone, Copy, PartialEq)]
enum Frame {
    Session(SessionId),
    Job(JobFrameId),
    Edit(EditFrameId),
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
                    Frame::Job(_) | Frame::Edit(_) => false,
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
    // The whole-shell register table (yank/put/<C-r>) -- one instance,
    // shared globally across every window/pane/session, matching both vim
    // (registers are global to the editor instance, not per-buffer) and
    // tmux (paste buffers are global to the server, not per-pane). See
    // bishedit::registers::Registers' own doc comment.
    let mut registers = Registers::new();

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
    // Same idea as job_frames, for `e`'s own detachable editor sessions
    // (Frame::Edit) -- see Frame's own doc comment.
    let mut edit_frames: HashMap<EditFrameId, fileeditor::EditSession> = HashMap::new();
    let mut next_edit_frame_id: EditFrameId = 1;
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
                &mut registers,
                term_rows,
                term_cols,
            );
            let _ = io::stdout().flush();
            continue;
        }

        // Same idea, for a window whose top frame is a still-detached
        // `Frame::Edit` -- either just pushed by the pending_edit
        // handling below, or the user switched back to a window they'd
        // earlier detached an `e` session from.
        if let Frame::Edit(edit_frame_id) = *windows[current_window].stack().last().unwrap() {
            run_edit_frame(
                edit_frame_id,
                session_id,
                &mut sessions,
                &mut windows,
                &mut current_window,
                &mut next_session_id,
                &mut next_window_id,
                &mut job_frames,
                &mut edit_frames,
                &mut cmd_history,
                &mut sinks_are_grid,
                &mut registers,
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
        // Same pattern again -- session_history is already the exact
        // snapshot the suggestions engine itself needs (see History's
        // own doc comment on why a clone here is cheap: an O(1) Rc-clone
        // of its own tail, not a copy).
        let shell_suggestion = suggestion::HistorySuggestionProvider { history: &session_history, cwd: Some(cwd_snapshot.as_path()) };
        // Relative-cursor-row menu tricks are only safe on a single real
        // terminal -- a promoted/split-pane session risks spilling the
        // extra row into a neighboring pane or the tab bar. Grid/promoted
        // mode gets its own, absolute-positioned path instead (see
        // redraw_with_completion_row's own doc comment), scoped to an
        // *unsplit* window only for now -- a split window's own
        // neighbor-pane clamping is a bigger, separate design, left for
        // later. The session's own vt100::Screen cursor already tracks
        // which row the upcoming prompt is about to occupy correctly (fed
        // a trailing "\r\n" every time a line is submitted), with none of
        // real-terminal relative movement's own scrolling ambiguity, so
        // no live query is needed to learn it.
        let row_origin = if sinks_are_grid && windows[current_window].panes.len() <= 1 {
            let rect = pane_rect(&windows[current_window], windows[current_window].focused_pane, term_rows, term_cols);
            let cursor_row = sessions[&session_id].screen.borrow().cursor().0;
            Some((rect.row + cursor_row, rect.row + rect.rows - 1))
        } else {
            None
        };
        let menu_capable = !sinks_are_grid || row_origin.is_some();

        match editor::read_line(
            &prompt_str,
            &session_history,
            false,
            sinks_are_grid,
            pending_initial.take(),
            col_origin,
            width,
            highlight_ctx,
            Some(&shell_completion),
            Some(&shell_suggestion),
            menu_capable,
            row_origin,
            &mut registers,
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
            // ctrl_l_reports is now `sinks_are_grid` for this call: at
            // the plain, unwindowed prompt (sinks_are_grid false) it's
            // still false, so Ctrl-L keeps meaning "clear the real
            // screen" exactly as before, handled inside read_line itself
            // -- this arm is unreachable there, same as it always was.
            // Once windowed/promoted, read_line's own raw "\x1b[H\x1b[2J"
            // clear would instead wipe the compositor's own frame -- pane
            // borders, tab bar and all -- with nothing ever repainting it
            // afterward, so it's reported here instead. The actual
            // clearing has to happen on the *session's own* grid (`\x1b[H
            // \x1b[2J` fed into it, exactly what erase_in_display(2) does
            // for a real terminal -- see vt100.rs's own doc comment: mode
            // 2 clears the live grid only, scrollback untouched, matching
            // real Ctrl-L never discarding scroll-back history), not the
            // real terminal directly -- a plain compositor_redraw() alone
            // just repaints this pane's *current* content unchanged, which
            // reads as "Ctrl-L does nothing." Clearing resets this
            // session's own cursor to (0, 0) too, which is what then
            // makes the *next* redraw show the live prompt back at the
            // top of the pane -- compositor_redraw's own doc comment
            // (self-healing full redraw) and render_compositor_frame's
            // cursor positioning (always the focused pane's own screen
            // cursor) both already key off exactly this field, so nothing
            // else needs to change for either the unsplit or split case.
            Ok(ReadOutcome::CtrlL) => {
                if sinks_are_grid {
                    sessions.get_mut(&session_id).unwrap().screen.borrow_mut().feed(b"\x1b[H\x1b[2J");
                    compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                }
            }
            Ok(ReadOutcome::Interrupted) => {
                // Ctrl-C abandons whatever multi-line construct was
                // pending, same as bash, and starts fresh at a new prompt.
                let session = sessions.get_mut(&session_id).unwrap();
                session.buffer.clear();
                // Same re-arming as a real Line -- Ctrl-C isn't an
                // immediate repeated Ctrl-D either.
                session.warned_stopped_jobs = false;
                // editor::read_line's own Key::CtrlC arm prints a bare
                // "^C\r\n" with no awareness of pane boundaries or the
                // tab bar -- on a pane's own last row this can land on
                // (or scroll into) the tab bar's row. compositor_redraw
                // is a full, absolutely-positioned repaint of every pane
                // plus the tab bar, so it's fully self-healing regardless
                // of what state that bare print left the real terminal
                // in -- same reasoning DirNav just below already applies.
                if sinks_are_grid {
                    compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                }
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
                    &mut registers,
                    NavStart::Prompt { text, cursor },
                    term_rows,
                    term_cols,
                ) {
                    Ok((NavExit::Resume(t, c), _)) => pending_initial = Some((t, c)),
                    Ok((NavExit::Detached | NavExit::Quit, _)) => pending_initial = None,
                    Err(e) => {
                        sessions.get(&session_id).unwrap().shell.sink_err(&format!("bish: error reading input: {}\n", e));
                        break;
                    }
                }
            }
            Ok(ReadOutcome::Line(line)) => {
                let mut window_action = None;
                let mut fg_pending = false;
                let mut edit_pending = false;
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
                        let highlighted = highlight::render_line(&line, highlight_ctx);
                        let echoed = format!("\r\x1b[K{}{}\r\n", prompt_str, highlighted);
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
                                    ExecResult::Edit => edit_pending = true,
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
                        &mut registers,
                        term_rows,
                        term_cols,
                    );
                } else if edit_pending {
                    // `e [file]` -- see ExecResult::Edit's own doc
                    // comment. ensure_promoted first, unconditionally:
                    // unlike a job (which only gets a pty, and so needs
                    // repl.rs's own compositor rendering, once already
                    // promoted -- see exec.rs's own `use_pty` gate), `e`
                    // always renders through the compositor/pane-rect
                    // system from the start, one code path, no separate
                    // "plain unpromoted real terminal" branch to
                    // maintain -- same reasoning Ctrl+Space's own
                    // normal-mode navigation already established.
                    ensure_promoted(&mut sessions, &mut sinks_are_grid);
                    let path = sessions.get_mut(&session_id).unwrap().shell.take_pending_edit();
                    let rect = pane_rect(&windows[current_window], windows[current_window].focused_pane, term_rows, term_cols);
                    match fileeditor::EditSession::open(path.as_deref(), normal_mode_content_rows(rect)) {
                        Ok(session) => {
                            let edit_frame_id = next_edit_frame_id;
                            next_edit_frame_id += 1;
                            edit_frames.insert(edit_frame_id, session);
                            windows[current_window].stack_mut().push(Frame::Edit(edit_frame_id));

                            run_edit_frame(
                                edit_frame_id,
                                session_id,
                                &mut sessions,
                                &mut windows,
                                &mut current_window,
                                &mut next_session_id,
                                &mut next_window_id,
                                &mut job_frames,
                                &mut edit_frames,
                                &mut cmd_history,
                                &mut sinks_are_grid,
                                &mut registers,
                                term_rows,
                                term_cols,
                            );
                        }
                        Err(e) => {
                            sessions.get_mut(&session_id).unwrap().shell.sink_err(&format!("bish: e: {}\n", e));
                            compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                        }
                    }
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
    registers: &mut Registers,
    term_rows: usize,
    term_cols: usize,
    editing: Option<&mut TextBuffer>,
    seed: Option<String>,
) -> CommandModeOutcome {
    let outcome = run_command_mode(session_id, sessions, windows, *current_window, cmd_history, job_frames, registers, term_rows, term_cols, editing, seed);
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
    registers: &mut Registers,
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
            // Normal mode, not command mode directly -- matches what the
            // detach key already does from a genuinely idle prompt (see
            // editor::ReadOutcome::NormalMode), rather than a second,
            // inconsistent "Ctrl+Space sometimes means normal mode,
            // sometimes means command mode" behavior depending on what
            // happened to be running. `:` still reaches command mode from
            // here (run_normal_mode_navigation's own doc comment), so
            // `window next`/`previous` away from this job stays exactly
            // as reachable as before, just one keystroke further in.
            // NavStart::JobDetach: this pane's top frame is Frame::Job,
            // not a live prompt, so whatever this returns for "resume
            // editing here" is meaningless and gets silently discarded
            // anyway once the main loop sees the same Frame::Job still
            // on top and re-drives this job instead of ever calling
            // read_line with it.
            let _ = run_normal_mode_navigation(
                session_id,
                sessions,
                windows,
                current_window,
                next_session_id,
                next_window_id,
                sinks_are_grid,
                job_frames,
                cmd_history,
                registers,
                NavStart::JobDetach,
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

// `e`'s own counterpart to run_fg_job_frame, above -- drives whatever
// editor session is behind `edit_frame_id`, freshly opened by the
// pending_edit handling below or already sitting there from an earlier
// detach. `run_normal_mode_navigation` is what actually drives it (see
// `NavBuffer`'s own doc comment -- an editor pane is that same one real
// Normal mode, not a separate loop) -- this function's only job is
// providing `rect`, moving the session's state in and back out, and
// reacting to how driving it ended.
#[allow(clippy::too_many_arguments)]
fn run_edit_frame(
    edit_frame_id: EditFrameId,
    session_id: SessionId,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: &mut usize,
    next_session_id: &mut SessionId,
    next_window_id: &mut u32,
    job_frames: &mut HashMap<JobFrameId, exec::FgJob>,
    edit_frames: &mut HashMap<EditFrameId, fileeditor::EditSession>,
    cmd_history: &mut History,
    sinks_are_grid: &mut bool,
    registers: &mut Registers,
    term_rows: usize,
    term_cols: usize,
) {
    // Taken out of edit_frames (rather than borrowed via get_mut) so
    // on_idle below can freely borrow edit_frames -- moot today (nothing
    // in service_background_jobs touches it), but matches
    // run_fg_job_frame's own reasoning for job_frames exactly, and keeps
    // the two symmetric.
    let session = edit_frames.remove(&edit_frame_id).expect("Frame::Edit always has a live edit_frames entry");
    let rect = pane_rect(&windows[*current_window], windows[*current_window].focused_pane, term_rows, term_cols);
    // "%": refreshed here, once, whenever this function starts driving a
    // session -- see fileeditor::set_last_filename's own doc comment for
    // the other place (a successful :w/:wq/:x) it needs the same
    // refresh.
    fileeditor::set_last_filename(&session.buffer, registers);
    let outcome = run_normal_mode_navigation(
        session_id,
        sessions,
        windows,
        current_window,
        next_session_id,
        next_window_id,
        sinks_are_grid,
        job_frames,
        cmd_history,
        registers,
        NavStart::Edit(session.buffer, Box::new(session.vk)),
        term_rows,
        term_cols,
    );
    match outcome {
        Ok((NavExit::Quit, _)) => {
            windows[*current_window].stack_mut().pop();
            if *sinks_are_grid {
                compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
            }
        }
        // Freezes this pane's own grid with the editor's current state
        // -- exactly once, right here, rather than mirroring
        // freeze_idle_prompt's own multiple call sites (one at every
        // place focus *might* move away): unlike a live prompt, a
        // detached editor session's content is genuinely static (nothing
        // runs in the background the way a job does) from this exact
        // moment until it's re-entered, at which point run_normal_mode_
        // navigation takes over rendering to the real terminal directly
        // again -- so one freeze, right when it stops being driven, is
        // both necessary and sufficient.
        Ok((NavExit::Detached, Some((buffer, vk)))) => {
            fileeditor::freeze_editor_frame(&sessions[&session_id].screen, &buffer, &vk, rect);
            edit_frames.insert(edit_frame_id, fileeditor::EditSession { buffer, vk });
        }
        Ok((NavExit::Resume(..), _)) | Ok((NavExit::Detached, None)) => {
            unreachable!("NavStart::Edit never produces NavExit::Resume, and always hands its buffer back")
        }
        Err(e) => {
            // A real I/O error reading a key -- same treatment the main
            // loop itself gives one (see its own read_line Err arm):
            // sink it and drop the session; there's no good way to
            // resume from here.
            windows[*current_window].stack_mut().pop();
            sessions.get_mut(&session_id).unwrap().shell.sink_err(&format!("bish: e: error reading input: {}\n", e));
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
                // Same one-place-at-a-time reasoning as Job just above --
                // an open editor session can't be shown in two windows
                // simultaneously either.
                Some(Frame::Edit(_)) => {
                    sessions[&cur_sid].shell.sink_err("bish: window: fg: that window is running an editor, not a session\n");
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
                Frame::Job(_) | Frame::Edit(_) => None,
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

const MOUSE_REPORTING_ENABLE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_REPORTING_DISABLE: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

const BRACKETED_PASTE_ENABLE: &str = "\x1b[?2004h";
const BRACKETED_PASTE_DISABLE: &str = "\x1b[?2004l";

// Turns real-terminal mouse reporting on/off (SGR extended coordinates,
// mode 1006, plus button-event tracking, mode 1002) to match whatever
// the job's own program has asked for on its virtual screen (vt100::
// Screen::mouse_reporting, set by its own DECSET 1000/1002/1003/1006
// request -- see that field's own doc comment: all four collapse into
// one flag there already, so this can't mirror "just clicks" vs. "every
// motion" any more precisely than that; 1002 is a reasonable default
// either way, tmux's own). A no-op if the wanted state already matches
// `enabled`. Called from drive_fg_job only -- the one place a job's own
// pty output (and thus a fresh DECSET request) is actually read while
// this job also owns real stdin, so it's the only place enabling this on
// the *real* terminal is ever correct (see drive_fg_job's own doc
// comment on why it's turned back off again before returning,
// unconditionally, rather than left however the job happened to leave
// it).
fn sync_mouse_reporting(enabled: &mut bool, screen: &Rc<RefCell<vt100::Screen>>) {
    let wants = screen.borrow().mouse_reporting;
    if wants == *enabled {
        return;
    }
    print!("{}", if wants { MOUSE_REPORTING_ENABLE } else { MOUSE_REPORTING_DISABLE });
    let _ = io::stdout().flush();
    *enabled = wants;
}

// Same idea as sync_mouse_reporting, for DECSET 2004 (bracketed paste --
// vt100::Screen::bracketed_paste, set by the job's own \x1b[?2004h/l
// request): without this, a job like vim asking its controlling
// terminal to bracket pastes (so it knows to suspend autoindent for the
// duration, rather than treating pasted text as fast typing) was asking
// bish's own vt100::Screen -- which faithfully tracked the flag but
// never actually told the *real* terminal to start bracketing pastes.
// The real terminal would then hand a pasted block to bish as an
// ordinary burst of characters, indistinguishable from typing, which
// drive_fg_job's raw stdin-forwarding loop passes straight through to
// the job -- so vim received it as unbracketed "typing" and mangled
// every line with its own autoindent, even though vim did everything
// right on its end. Mirrors sync_mouse_reporting's own on/off/cleanup
// shape exactly, called from the same two spots.
fn sync_bracketed_paste(enabled: &mut bool, screen: &Rc<RefCell<vt100::Screen>>) {
    let wants = screen.borrow().bracketed_paste;
    if wants == *enabled {
        return;
    }
    print!("{}", if wants { BRACKETED_PASTE_ENABLE } else { BRACKETED_PASTE_DISABLE });
    let _ = io::stdout().flush();
    *enabled = wants;
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
//
// Real mouse events (an SGR sequence typed at bish's own stdin) reach the
// job below via the same raw-forwarding this loop already does for every
// other byte -- see sync_mouse_reporting's own doc comment for the
// on/off toggle that makes the real terminal actually send them. A real
// paste reaches the job the same way, already bracketed in
// \x1b[200~/\x1b[201~ if it asked for that too -- see
// sync_bracketed_paste's own doc comment.
fn drive_fg_job(job: &mut exec::FgJob, screen: &Rc<RefCell<vt100::Screen>>, mut redraw: impl FnMut(), mut on_idle: impl FnMut()) -> FgOutcome {
    use std::io::{Read, Write};
    use std::os::unix::io::AsRawFd;

    pty::set_nonblocking(job.pty_master().as_raw_fd());
    let _raw_guard = term::RawGuard::enable(0).ok();

    // Checked once up front (in case the job already asked for mouse
    // reporting before this particular drive call started -- e.g.
    // resuming a previously-detached job) and again after every batch of
    // freshly-read output below, since that's exactly when a fresh
    // DECSET request could have just arrived.
    let mut mouse_enabled = false;
    sync_mouse_reporting(&mut mouse_enabled, screen);
    let mut bracketed_paste_enabled = false;
    sync_bracketed_paste(&mut bracketed_paste_enabled, screen);

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
            sync_mouse_reporting(&mut mouse_enabled, screen);
            sync_bracketed_paste(&mut bracketed_paste_enabled, screen);
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
    // Unconditionally, regardless of which `break` above got here: the
    // real terminal has no notion of "this job's own mouse mode," so
    // leaving it enabled past this point (however the job itself might
    // still feel about it) would leak mouse escape sequences into
    // whatever reads stdin next -- the shell prompt, bishedit's own
    // normal-mode navigation, .... Harmless (and cheap) to send even if
    // it was never actually turned on.
    if mouse_enabled {
        print!("{MOUSE_REPORTING_DISABLE}");
        let _ = io::stdout().flush();
    }
    // Same unconditional cleanup, and for the same reason: bish's own
    // prompt (editor::read_line) never recognizes \x1b[200~/\x1b[201~ --
    // left enabled, a paste right after this job exits would land in the
    // next read_line call as literal garbage bytes instead of a bracketed
    // block it knows to ignore as markers.
    if bracketed_paste_enabled {
        print!("{BRACKETED_PASTE_DISABLE}");
        let _ = io::stdout().flush();
    }
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
// compute_regions from a window's PaneLayout tree. pub(crate): fileeditor
// needs this shape too (a pane's own rect, exactly the same meaning) --
// see its own `drive`/`build_editor_frame` signatures.
#[derive(Clone, Copy)]
pub(crate) struct Rect {
    pub(crate) row: usize,
    pub(crate) col: usize,
    pub(crate) rows: usize,
    pub(crate) cols: usize,
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
    // Visual mode's own committed selections (see vimkeys.rs's own
    // `visual` field doc comment for the active, not-yet-committed one --
    // that lives in `VimKeys`, not here, since it's just an anchor plus
    // whatever this buffer's own live cursor already is). In commit
    // order: `Z` pushes one, `y` reads every entry here (plus the active
    // one) to build one concatenated yank, and both `y` and Escape clear
    // this back to empty. A plain `Vec<motion::MotionRange>` rather than
    // a new type -- `extract_text` already knows how to read one of these
    // regardless of who built it.
    selections: Vec<motion::MotionRange>,
}

// The scrollback length to use for `ScreenBuffer`'s own combined
// addressing (see its own doc comment) -- `s.scrollback.len()` normally,
// but `0` whenever the grid currently showing is the *alternate* screen
// (`s.using_alternate`, set for the duration of a fullscreen program like
// vim/less that switched into it -- see `switch_alt_screen`'s own doc
// comment). Real scrollback belongs to the *primary* grid only (a real
// terminal never lets you scroll into history while an alt-screen program
// owns the display either); treating it as if it sat "above" the alt
// screen's own rows -- what plain `s.scrollback.len()` would do -- mixes
// stale pre-alt-screen content into the addressing ahead of the program's
// own rows, which is exactly what made normal mode read as "hides the
// content of the app": with any pre-existing scrollback in that pane, the
// combined cursor position (scrollback + the program's own cursor row)
// could land the viewport almost entirely inside the stale scrollback
// instead of the program's own screen.
fn addressable_scrollback_len(s: &vt100::Screen) -> usize {
    if s.using_alternate {
        0
    } else {
        s.scrollback.len()
    }
}

impl ScreenBuffer {
    fn new(screen: Rc<RefCell<vt100::Screen>>, vheight: usize) -> ScreenBuffer {
        let (sb_len, cur_row, cur_col) = {
            let s = screen.borrow();
            let (row, col) = s.cursor();
            (addressable_scrollback_len(&s), row, col)
        };
        // Starts where the live cursor currently is -- translated into
        // this combined addressing, where scrollback lines come first --
        // the same convention tmux copy-mode uses (enter at the current
        // cursor position, not always at the very top).
        let cursor = (sb_len + cur_row, cur_col);
        let vheight = vheight.max(1);
        let vtop = cursor.0.saturating_sub(vheight - 1);
        ScreenBuffer { screen, cursor, vtop, vheight, marks: HashMap::new(), selections: Vec::new() }
    }

    // `line`'s own raw cell count -- the live grid's current width for a
    // grid row, or that scrollback row's width *at the time it scrolled
    // off* for a scrollback row, which can differ from the live grid's
    // current width if the terminal was resized since. `char_at`/
    // `line_len` trim trailing blanks off of this.
    fn raw_len(&self, line: usize) -> usize {
        let s = self.screen.borrow();
        let sb_len = addressable_scrollback_len(&s);
        if line < sb_len {
            s.scrollback[line].len()
        } else {
            s.size().1
        }
    }

    fn raw_char_at(&self, line: usize, col: usize) -> Option<char> {
        let s = self.screen.borrow();
        let sb_len = addressable_scrollback_len(&s);
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
        (addressable_scrollback_len(&s) + cursor_row + 1).max(1)
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

// The one Normal-mode-navigation loop (`run_normal_mode_navigation`)
// drives either a read-only view over a pane's own scrollback/live grid
// (`ReadOnly`, backed by `ScreenBuffer`) or a real, mutable file buffer
// (`Editable`, backed by `bishedit::textbuffer::TextBuffer`, while this
// pane's top frame is a `Frame::Edit`) -- an editor pane is not a
// different mode, it's this same Normal mode over a buffer that happens
// to support mutation too. `impl BisheditBuffer` below delegates every
// navigation method to whichever variant is live, which is what lets
// every already buffer-generic helper (`motion::apply_motion`,
// `editor::apply_motion_or_reselect`, `editor::yank_motion`, ...) work
// unmodified against either one -- only the mutating `KeyOutcome`s
// (`Put`, `Operator(Delete/Change)`, ...) need to match on this enum
// directly, since mutation was never part of the shared `Buffer` trait
// (see `TextBuffer`'s own doc comment on why that's deliberate).
enum NavBuffer {
    ReadOnly(ScreenBuffer),
    Editable(TextBuffer),
}

impl NavBuffer {
    fn selections(&self) -> &Vec<motion::MotionRange> {
        match self {
            NavBuffer::ReadOnly(b) => &b.selections,
            NavBuffer::Editable(b) => &b.selections,
        }
    }

    fn selections_mut(&mut self) -> &mut Vec<motion::MotionRange> {
        match self {
            NavBuffer::ReadOnly(b) => &mut b.selections,
            NavBuffer::Editable(b) => &mut b.selections,
        }
    }

    fn as_editable_mut(&mut self) -> Option<&mut TextBuffer> {
        match self {
            NavBuffer::ReadOnly(_) => None,
            NavBuffer::Editable(b) => Some(b),
        }
    }
}

impl BisheditBuffer for NavBuffer {
    fn line_count(&self) -> usize {
        match self {
            NavBuffer::ReadOnly(b) => b.line_count(),
            NavBuffer::Editable(b) => b.line_count(),
        }
    }

    fn line_len(&self, line: usize) -> usize {
        match self {
            NavBuffer::ReadOnly(b) => b.line_len(line),
            NavBuffer::Editable(b) => b.line_len(line),
        }
    }

    fn char_at(&self, line: usize, col: usize) -> Option<char> {
        match self {
            NavBuffer::ReadOnly(b) => b.char_at(line, col),
            NavBuffer::Editable(b) => b.char_at(line, col),
        }
    }

    fn cursor(&self) -> (usize, usize) {
        match self {
            NavBuffer::ReadOnly(b) => b.cursor(),
            NavBuffer::Editable(b) => b.cursor(),
        }
    }

    fn set_cursor(&mut self, line: usize, col: usize) {
        match self {
            NavBuffer::ReadOnly(b) => b.set_cursor(line, col),
            NavBuffer::Editable(b) => b.set_cursor(line, col),
        }
    }

    fn viewport_top(&self) -> usize {
        match self {
            NavBuffer::ReadOnly(b) => b.viewport_top(),
            NavBuffer::Editable(b) => b.viewport_top(),
        }
    }

    fn set_viewport_top(&mut self, line: usize) {
        match self {
            NavBuffer::ReadOnly(b) => b.set_viewport_top(line),
            NavBuffer::Editable(b) => b.set_viewport_top(line),
        }
    }

    fn viewport_height(&self) -> usize {
        match self {
            NavBuffer::ReadOnly(b) => b.viewport_height(),
            NavBuffer::Editable(b) => b.viewport_height(),
        }
    }

    fn set_mark(&mut self, name: char, pos: (usize, usize)) {
        match self {
            NavBuffer::ReadOnly(b) => b.set_mark(name, pos),
            NavBuffer::Editable(b) => b.set_mark(name, pos),
        }
    }

    fn get_mark(&self, name: char) -> Option<(usize, usize)> {
        match self {
            NavBuffer::ReadOnly(b) => b.get_mark(name),
            NavBuffer::Editable(b) => b.get_mark(name),
        }
    }
}

// Adjusts `buf`'s viewport so its navigation cursor's line is visible,
// scrolling as little as possible -- matching vim's own scrolling, which
// only jumps when the cursor would otherwise move off-screen, not
// recentering on every motion.
fn scroll_to_show_cursor(buf: &mut impl BisheditBuffer) {
    let (line, _) = buf.cursor();
    let height = buf.viewport_height();
    if line < buf.viewport_top() {
        buf.set_viewport_top(line);
    } else if line >= buf.viewport_top() + height {
        buf.set_viewport_top(line + 1 - height);
    }
}

// The (start, end) char-column range `range` covers on this one `line`,
// if any -- `None` for a line outside `range.from.0..=range.to.0`
// entirely. `Linewise` covers the whole row; `Inclusive` (Visual mode's
// own charwise shape -- see `active_visual_range`'s own doc comment) is
// full-width on any line strictly between the endpoints and clamped to
// the true from/to column on the first/last line, mirroring exactly how
// `motion::extract_text`'s own char-walk already treats a multi-line
// charwise range -- `to.1 + 1` since Visual charwise is inclusive of the
// character the cursor's on, unlike most charwise motions.
fn selection_columns_in_line(range: &motion::MotionRange, line: usize, cols: usize) -> Option<(usize, usize)> {
    if line < range.from.0 || line > range.to.0 {
        return None;
    }
    if range.shape == motion::MotionShape::Linewise {
        return Some((0, cols));
    }
    let start = if line == range.from.0 { range.from.1 } else { 0 };
    let end = if line == range.to.0 { range.to.1 + 1 } else { cols };
    Some((start, end))
}

// `search_matches`: (start, end) char-column ranges on this one row to
// render in reverse video -- vim's own `hlsearch` treatment, and the same
// mechanism (toggling `CellAttrs::reverse`) editor.rs's own Ctrl-E search
// highlighting uses. Applied by flipping the bit on whatever attrs a cell
// already carries (e.g. `ls --color` output keeps its own fg/bg, just
// with reverse forced on) rather than replacing them outright. Also
// where Visual mode's own selection highlighting piggybacks -- see
// render_normal_mode_frame's own doc comment on why that's simply more
// entries on this same list rather than a separate rendering pass.
fn render_normal_mode_row(out: &mut String, buf: &ScreenBuffer, line: usize, cols: usize, search_matches: &[(usize, usize)]) {
    let mut last: Option<(vt100::Color, vt100::Color, vt100::CellAttrs)> = None;
    let s = buf.screen.borrow();
    let sb_len = addressable_scrollback_len(&s);
    for c in 0..cols {
        let mut cell = if line < sb_len {
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
        if search_matches.iter().any(|&(start, end)| c >= start && c < end) {
            cell.attrs.reverse = true;
        }
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

// "-- NORMAL --"'s own mode-indicator text, swapped for Visual mode's two
// shapes while `vk.is_visual()` -- matching real vim's own mode-line
// convention (`-- VISUAL --` / `-- VISUAL LINE --`).
fn mode_label(vk: &VimKeys) -> &'static str {
    match vk.visual_anchor() {
        Some((RegisterShape::Char, _)) => "-- VISUAL --",
        Some((RegisterShape::Line, _)) => "-- VISUAL LINE --",
        None => "-- NORMAL --",
    }
}

// The status bar's left side: the search/command line while one is being
// typed (`command_line`, e.g. ":q" or "/foo" as typed so far -- this is
// how `:`/`/`/`?` input actually becomes visible; previously nothing was
// drawn while typing them at all, easily read as "doesn't work"), else
// whatever `vk` has to say about the current key sequence -- a pending
// count/prefix in progress (e.g. "20g" mid-`20gg`), or failing that a
// brief flash of the motion that was just applied (e.g. "[20k]"), else
// just the bare mode indicator (`mode_label`, above). A pending search
// ('/'/'?') is shown alone, without the mode-indicator prefix, matching
// vim's own command-line convention of replacing the mode indicator
// outright while typing one.
fn normal_mode_status_left(vk: &VimKeys, command_line: Option<&str>) -> String {
    if let Some(cmd) = command_line {
        return cmd.to_string();
    }
    let pending = vk.pending_display();
    if pending.starts_with('/') || pending.starts_with('?') {
        return pending.to_string();
    }
    let label = mode_label(vk);
    if !pending.is_empty() {
        return format!("{} {}", label, pending);
    }
    let last = vk.last_motion_display();
    if !last.is_empty() {
        return format!("{} [{}]", label, last);
    }
    label.to_string()
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

// The search pattern to highlight (see render_normal_mode_row's own doc
// comment) -- the in-progress `/`/`?` text while one is being typed
// (incsearch-style live feedback for free), else the last resolved
// search's own pattern, else nothing. Same rule editor.rs's own
// normal_mode_prompt_and_matches uses for Ctrl-E's line-local Normal
// mode, duplicated rather than shared: the two contexts drive different
// `Buffer` impls (`LineBuffer` there, `ScreenBuffer` here), and this is
// only a few lines either way.
fn active_search_pattern(vk: &VimKeys, buf: &ScreenBuffer) -> Option<String> {
    let pending = vk.pending_display();
    if let Some(rest) = pending.strip_prefix('/').or_else(|| pending.strip_prefix('?')) {
        return if rest.is_empty() { None } else { Some(rest.to_string()) };
    }
    if vk.last_search_is_word() {
        motion::word_under_cursor(buf, buf.cursor())
    } else {
        let text = vk.last_search_text();
        if text.is_empty() { None } else { Some(text.to_string()) }
    }
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
    let pattern = active_search_pattern(vk, buf);
    // Every selection currently on screen -- every committed one, plus
    // whatever's actively being extended right now (if Visual mode is
    // active) -- computed once up front rather than per row, same as
    // `pattern` just above.
    let active = active_visual_range(vk, buf);
    let mut out = String::new();
    for r in 0..content_rows {
        let line = buf.viewport_top() + r;
        out.push_str(&format!("\x1b[{};{}H", rect.row + r + 1, rect.col + 1));
        if line < total {
            // Only the currently-visible rows are ever scanned for
            // matches -- correct, not just an optimization: a match
            // outside the viewport can't be seen either way, and this
            // avoids scanning potentially large scrollback on every
            // redraw.
            let mut matches = match &pattern {
                Some(p) => motion::find_matches_in_line(buf, line, p),
                None => Vec::new(),
            };
            // Selections render exactly like a search match -- reverse
            // video, same `render_normal_mode_row` mechanism -- by simply
            // becoming more entries on this same list. No new visual
            // style for this pass (see this feature's own plan doc).
            for range in buf.selections.iter().chain(active.iter()) {
                if let Some(cols) = selection_columns_in_line(range, line, rect.cols) {
                    matches.push(cols);
                }
            }
            render_normal_mode_row(&mut out, buf, line, rect.cols, &matches);
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
// repeatable action) and are handled directly in dispatch_window_cmd's
// own match, which never calls this for them.
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
            unreachable!("handled directly in dispatch_window_cmd, never reaches here")
        }
    }
}

// Actually applies a resolved `<C-w>{cmd}` -- called from run_normal_
// mode_navigation's own single `KeyOutcome::Window` arm, regardless of
// which `NavBuffer` variant produced it (a `<C-w>` typed inside a
// `Frame::Edit` pane resolves the exact same `KeyOutcome::Window` a
// read-only pane's own Normal mode does -- see `NavBuffer`'s own doc
// comment). Doesn't redraw/return anything itself beyond what
// apply_window_action (or the GotoFirst/GotoLast branch) already does on
// its own -- the caller exits its whole loop afterward either way
// (nothing left to resume, a window-focus change).
#[allow(clippy::too_many_arguments)]
fn dispatch_window_cmd(
    cmd: WindowCmd,
    count: Option<usize>,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: &mut usize,
    next_session_id: &mut SessionId,
    next_window_id: &mut u32,
    sinks_are_grid: &mut bool,
    term_rows: usize,
    term_cols: usize,
) {
    match cmd {
        // No WindowAction equivalent -- an absolute tab-position jump,
        // not a repeatable action -- so this sets current_window
        // directly instead of going through apply_window_action,
        // matching Motion::GotoFirstLine/GotoLastLine's own default-vs-
        // explicit-target split (see KeyOutcome::Window's own doc
        // comment). Still needs the same freeze apply_window_action
        // itself does up front (see freeze_focused_idle_prompt's own
        // doc comment) -- this is the one window-focus-changing path
        // that doesn't go through apply_window_action at all.
        WindowCmd::GotoFirstWindow | WindowCmd::GotoLastWindow => {
            freeze_focused_idle_prompt(sessions, windows, *current_window);
            let default = if cmd == WindowCmd::GotoFirstWindow { 1 } else { windows.len() };
            let target = count.unwrap_or(default);
            *current_window = target.saturating_sub(1).min(windows.len().saturating_sub(1));
            compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
        }
        _ => {
            let action = window_cmd_to_action(cmd);
            for _ in 0..count.unwrap_or(1).max(1) {
                apply_window_action(action, sessions, windows, current_window, next_session_id, next_window_id, sinks_are_grid, term_rows, term_cols);
            }
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
// What `run_normal_mode_navigation` is being entered to drive -- decides
// both which `NavBuffer` variant it builds (see that enum's own doc
// comment) and what its own `NavExit` return values actually mean once
// it's done.
enum NavStart {
    // Ctrl+Space from a live shell prompt, mid-typing `text` with the
    // cursor at `cursor` -- resumable (see `NavExit::Resume`).
    Prompt { text: String, cursor: usize },
    // Ctrl+Space detaching a running foreground job -- read-only, same
    // as `Prompt`, but nothing to resume into (this pane's top frame
    // stays `Frame::Job`, not a live prompt, so a caller-side "resume
    // this text" would have nothing to apply it to).
    JobDetach,
    // Driving a `Frame::Edit` pane -- `TextBuffer`/`VimKeys` moved in for
    // the duration, handed back (mutated) via the paired
    // `Option<(TextBuffer, VimKeys)>` this function returns alongside
    // its `NavExit`, regardless of which one that turns out to be.
    // `VimKeys` is boxed only to keep this enum's own size close to its
    // other two (unit-ish) variants -- `Prompt`'s inline `String` is by
    // far the common case, and clippy flags an enum whose largest
    // variant dwarfs the rest, since every `NavStart` value pays that
    // largest variant's stack size regardless of which one it is.
    Edit(TextBuffer, Box<VimKeys>),
}

// What happened when `run_normal_mode_navigation` returns -- interpreted
// differently depending on which `NavStart` produced it: only `Prompt`
// ever produces `Resume` (hand typing back to the live prompt's own
// editor); only `Edit` ever produces `Quit` (`:q`/`:wq`/`:x`/`:q!`/`ZZ`,
// popping the `Frame::Edit`); `Detached` covers everything else for any
// start -- focus moved via `<C-w>`/a `:window` action, or EOF -- exactly
// `Ok(None)`'s existing meaning before this was named.
enum NavExit {
    Resume(String, usize),
    Detached,
    Quit,
}

// Packages this loop's own `buf`/`vk` locals back up for the caller once
// it's done with them -- `Some` iff `buf` is `NavBuffer::Editable` (i.e.
// iff this call started from `NavStart::Edit`), so `run_edit_frame` can
// re-stash (or, on `NavExit::Quit`, just drop) the session; `None` for
// either `ReadOnly` start, where there's nothing to hand back (both of
// this function's other two callers already discard this half of the
// return value entirely).
fn nav_buffer_into_edit_state(buf: NavBuffer, vk: VimKeys) -> Option<(TextBuffer, VimKeys)> {
    match buf {
        NavBuffer::Editable(tb) => Some((tb, vk)),
        NavBuffer::ReadOnly(_) => None,
    }
}

// Dispatches this loop's own per-keystroke redraw to whichever renderer
// actually matches `buf`'s concrete backing -- `ScreenBuffer`'s own
// `render_normal_mode_frame`, or `TextBuffer`'s own `fileeditor::
// render_editor_frame` (gutter, syntax highlighting, Insert/Replace mode
// labels, dirty flag -- everything a real file editor's Normal mode needs
// that a read-only scrollback view never did). Always renders Normal
// mode specifically -- Insert/Replace mode's own rendering happens
// inside `fileeditor::run_insert_mode`'s own nested loop instead, which
// is the only place either of those modes is ever live.
fn render_nav_frame(buf: &NavBuffer, vk: &VimKeys, rect: Rect) {
    match buf {
        NavBuffer::ReadOnly(sb) => render_normal_mode_frame(sb, rect, vk, None),
        NavBuffer::Editable(tb) => fileeditor::render_editor_frame(tb, vk, fileeditor::EditorMode::Normal, rect),
    }
}

// Visual mode's own `Z`/`y`/`d`/`c`/`p`/`S`/Escape's shared first step:
// if there's a currently-active (not yet committed) selection, commit it
// -- a no-op if `vk` isn't actually mid a Visual selection right now
// (`active_visual_range` returns `None`).
fn commit_active_selection(vk: &VimKeys, buf: &mut NavBuffer) {
    if let Some(range) = active_visual_range(vk, &*buf) {
        buf.selections_mut().push(range);
    }
}

// The one real Normal-mode loop -- drives a read-only view over a pane's
// own scrollback/live grid (`NavStart::Prompt`/`JobDetach`, backed by
// `NavBuffer::ReadOnly`) *or* a real, mutable file buffer
// (`NavStart::Edit`, backed by `NavBuffer::Editable`, while this pane's
// top frame is a `Frame::Edit`) -- an editor pane is not a categorically
// different mode reached by detaching out of this one, it's this same
// loop over a buffer that happens to support mutation too (see
// `NavBuffer`'s own doc comment). Every already buffer-generic helper
// (`motion::apply_motion`, `editor::apply_motion_or_reselect`,
// `editor::yank_motion`/`yank_lines`, `active_visual_range`) runs
// unmodified against either variant; only the mutating `KeyOutcome`s
// (`Put`, `Operator`/`OperatorLines` for `Delete`/`Change`/case-ops,
// `EnterInsert`/`EnterReplace`, ...) match on `NavBuffer` directly, since
// mutation was never part of the shared `Buffer` trait.
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
    registers: &mut Registers,
    start: NavStart,
    term_rows: usize,
    term_cols: usize,
) -> io::Result<(NavExit, Option<(TextBuffer, VimKeys)>)> {
    let rect = pane_rect(&windows[*current_window], windows[*current_window].focused_pane, term_rows, term_cols);

    // Only `Prompt` ever has real text/cursor to resume into (see
    // `NavExit::Resume`'s own doc comment) -- kept as empty/0 for the
    // other two starts anyway so `EnterInsert`/`EnterReplace`/`ZZ`/`:q`'s
    // own `ReadOnly` handling below can stay one shared implementation:
    // for `JobDetach` those are silently discarded by this function's own
    // caller regardless (this pane's top frame is `Frame::Job`, not a
    // live prompt), and `Edit` never reaches that code path at all.
    let (initial_text, initial_cursor) = match &start {
        NavStart::Prompt { text, cursor } => (text.clone(), *cursor),
        NavStart::JobDetach | NavStart::Edit(..) => (String::new(), 0),
    };
    let original_chars: Vec<char> = initial_text.chars().collect();

    let (mut buf, mut vk) = match start {
        NavStart::Prompt { text, cursor } => {
            // Same reasoning as freeze_idle_prompt's other call sites
            // (splitting, switching pane focus): this session's live
            // prompt has only ever been drawn straight to the real
            // terminal by editor::read_line, never captured into its own
            // grid -- fine as long as nothing needs to read that grid
            // back, which is exactly what's about to happen. Ctrl+Space
            // doesn't change focus (unlike those other call sites), so
            // without this the very first entry into normal mode in a
            // session that's never lost focus before would render as a
            // blank pane, not even showing the current prompt (or
            // whatever had already been typed).
            let screen = sessions[&session_id].screen.clone();
            let mut sb = ScreenBuffer::new(screen, normal_mode_content_rows(rect));
            let prompt_str = freeze_input_with_text(sessions.get_mut(&session_id).unwrap(), &text);
            // Explicitly positioned rather than trusting wherever
            // ScreenBuffer::new's own default (or the vt100 grid's
            // cursor, wherever feed() happened to leave it -- at the
            // *end* of what was just fed, which is only right if the
            // original cursor was already at the end of `text` too)
            // lands: the navigation cursor should start exactly where
            // editing was interrupted, matching real vim entering Normal
            // mode from Insert.
            let last_line = sb.line_count().saturating_sub(1);
            sb.set_cursor(last_line, editor::visible_len(&prompt_str) + cursor);
            (NavBuffer::ReadOnly(sb), VimKeys::new())
        }
        // No repositioning at all: ScreenBuffer::new's own default
        // cursor is already the live grid's real cursor -- exactly where
        // the job's own on-screen cursor was left, which is the only
        // sensible place for navigation to start when there's no
        // synthetic prompt line to land after.
        NavStart::JobDetach => {
            let screen = sessions[&session_id].screen.clone();
            (NavBuffer::ReadOnly(ScreenBuffer::new(screen, normal_mode_content_rows(rect))), VimKeys::new())
        }
        NavStart::Edit(tb, vk0) => (NavBuffer::Editable(tb), *vk0),
    };

    let _guard = term::RawGuard::enable(0)?;
    // Repaints the whole screen first -- necessary the very first time
    // normal mode ever triggers promotion (the alternate screen buffer
    // starts out blank), harmless otherwise -- then this pane's own
    // rectangle on top of that with the current view.
    compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
    render_nav_frame(&buf, &vk, rect);
    let mut pending_view = PendingView::None;

    let result: (NavExit, Option<(TextBuffer, VimKeys)>) = 'nav: loop {
        let mut key = match editor::read_key_idle(&mut || {
            service_background_jobs(sessions, windows, job_frames, *current_window);
        })? {
            Some(k) => k,
            // EOF: nothing sensible to resume into for any start -- for
            // `Edit` specifically, that means dropping the session
            // rather than leaving it attached with no way to ever drive
            // it again.
            None => {
                let exit = if matches!(buf, NavBuffer::Editable(_)) { NavExit::Quit } else { NavExit::Detached };
                break 'nav (exit, nav_buffer_into_edit_state(buf, vk));
            }
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
                        None => {
                            let exit = if matches!(buf, NavBuffer::Editable(_)) { NavExit::Quit } else { NavExit::Detached };
                            break 'nav (exit, nav_buffer_into_edit_state(buf, vk));
                        }
                    };
                }
                PendingView::Output | PendingView::Transcript => {
                    pending_view = PendingView::None;
                    compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
                    render_nav_frame(&buf, &vk, rect);
                    break;
                }
            }
        }

        match key {
            // Visual mode's own `Z`/`y`/`d`/`c`/`p`/`S`/Escape/Ctrl-C --
            // kept at this same outer tier as `Z`(`Z`)/`:` below rather
            // than inside vimkeys.rs's own `feed()`, because "is there a
            // selection to act on" is `NavBuffer`-owned state
            // (`selections`) vimkeys.rs deliberately never sees (see its
            // own module doc comment). `vk.is_idle()` guards all of
            // them: mid a sub-prefix (`f`, a count, ...) these keys keep
            // their ordinary meaning instead (a find-char target, a
            // count digit, ...) rather than being stolen here.
            //
            // `Z` (single, not `ZZ`)/`y`/Escape/Ctrl-C: meaningful for
            // *either* buffer kind (committing/yanking/cancelling a
            // selection doesn't require mutating anything), so these
            // four apply the same way they always have for `ReadOnly`.
            // Ctrl-C only cancels for `Editable` though (`key ==
            // Key::Escape || matches!(buf, NavBuffer::Editable(_))`) --
            // `ReadOnly`'s own normal-mode-navigation never gave Ctrl-C
            // any meaning before this, and there's no reason to start
            // now just because `Editable` also aliases it to Escape (see
            // `fileeditor::run_insert_mode`'s own doc comment for why
            // that alias exists there).
            //
            // `d`/`c`/`p`/`S` mutate a selection, so they're gated to
            // `Editable` only (the extra `matches!` clause in each of
            // their guards) -- for `ReadOnly` they fall through
            // unchanged to `vk.feed`'s own ordinary operator-prefix
            // handling, exactly as before this unification.
            Key::Char('Z') if vk.is_idle() && vk.is_visual() => {
                commit_active_selection(&vk, &mut buf);
                let end_cursor = buf.cursor();
                vk.end_visual(end_cursor);
                render_nav_frame(&buf, &vk, rect);
                continue;
            }
            Key::Char('y') if vk.is_idle() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let register = vk.take_pending_register();
                let end_cursor = buf.cursor();
                let selections = buf.selections().clone();
                match &mut buf {
                    NavBuffer::ReadOnly(sb) => yank_selections(sb, &selections, registers, register),
                    NavBuffer::Editable(tb) => tb.yank_selections(registers, register),
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&buf, &vk, rect);
                continue;
            }
            Key::Char('d') if vk.is_idle() && matches!(buf, NavBuffer::Editable(_)) && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let register = vk.take_pending_register();
                let end_cursor = buf.cursor();
                if let NavBuffer::Editable(tb) = &mut buf {
                    tb.delete_selections(registers, register);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&buf, &vk, rect);
                continue;
            }
            Key::Char('c') if vk.is_idle() && matches!(buf, NavBuffer::Editable(_)) && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let register = vk.take_pending_register();
                let end_cursor = buf.cursor();
                let mut deleted = false;
                if let NavBuffer::Editable(tb) = &mut buf {
                    deleted = tb.delete_selections(registers, register);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                if deleted && let NavBuffer::Editable(tb) = &mut buf {
                    fileeditor::run_insert_mode(tb, &mut vk, rect, registers, &mut || service_background_jobs(sessions, windows, job_frames, *current_window), false)?;
                }
                render_nav_frame(&buf, &vk, rect);
                continue;
            }
            Key::Char('p') | Key::Char('P') if vk.is_idle() && matches!(buf, NavBuffer::Editable(_)) && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let register = vk.take_pending_register();
                let end_cursor = buf.cursor();
                if let NavBuffer::Editable(tb) = &mut buf {
                    tb.put_over_selections(registers, register);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&buf, &vk, rect);
                continue;
            }
            Key::Char('S') if vk.is_idle() && matches!(buf, NavBuffer::Editable(_)) && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let end_cursor = buf.cursor();
                if let NavBuffer::Editable(tb) = &mut buf
                    && let Some(Key::Char(ch)) = editor::read_key_idle(&mut || {
                        service_background_jobs(sessions, windows, job_frames, *current_window);
                    })?
                {
                    fileeditor::surround_selections(tb, ch);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&buf, &vk, rect);
                continue;
            }
            Key::Escape | Key::CtrlC if vk.is_idle() && (key == Key::Escape || matches!(buf, NavBuffer::Editable(_))) && (vk.is_visual() || !buf.selections().is_empty()) => {
                let end_cursor = buf.cursor();
                vk.end_visual(end_cursor);
                buf.selections_mut().clear();
                render_nav_frame(&buf, &vk, rect);
                continue;
            }
            Key::Char('Z') => {
                let k2 = editor::read_key_idle(&mut || {
                    service_background_jobs(sessions, windows, job_frames, *current_window);
                })?;
                if k2 != Some(Key::Char('Z')) {
                    continue;
                }
                if matches!(buf, NavBuffer::Editable(_)) {
                    // `ZZ`: vim's own alias for `:x` -- save and quit.
                    let mut saved = true;
                    if let NavBuffer::Editable(tb) = &mut buf {
                        match tb.save(None) {
                            Ok(()) => fileeditor::set_last_filename(tb, registers),
                            Err(e) => {
                                sessions[&session_id].shell.sink_err(&format!("bish: E212: Can't open file for writing: {e}\n"));
                                saved = false;
                            }
                        }
                    }
                    if saved {
                        break 'nav (NavExit::Quit, nav_buffer_into_edit_state(buf, vk));
                    }
                    render_nav_frame(&buf, &vk, rect);
                    continue;
                }
                break 'nav (NavExit::Resume(initial_text.clone(), initial_cursor), nav_buffer_into_edit_state(buf, vk));
            }
            // ':' isn't routed through vimkeys -- it hands off to the
            // real command-mode feature, global rather than pinned to
            // this pane's own rect (run_command_mode positions its own
            // prompt row itself). `buf.as_editable_mut()` is what makes
            // `w`/`wq`/`x`/`q`/`q!` mean anything there (see run_
            // command_mode's own `editing` parameter doc comment) -- the
            // *same* call either way, not a separate file-command parser
            // pre-empting it.
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
                    registers,
                    term_rows,
                    term_cols,
                    buf.as_editable_mut(),
                    None,
                ) {
                    // Matches vim: an aborted/cancelled ':' command drops
                    // back into Normal mode, not out of it entirely.
                    CommandModeOutcome::Cancelled => {
                        render_nav_frame(&buf, &vk, rect);
                        continue;
                    }
                    // Same `CommandModeOutcome::Quit` `"q"`/`"q!"` always
                    // produced -- what it means depends on which buffer
                    // this call is driving (see run_command_mode's own
                    // `editing` doc comment): for `Editable`, `:q`/`:q!`/
                    // `:wq`/`:x` genuinely quit the file; for `ReadOnly`,
                    // same as `ZZ`/`i` -- restore the original text/
                    // cursor rather than quitting anything.
                    CommandModeOutcome::Quit => {
                        if matches!(buf, NavBuffer::Editable(_)) {
                            break 'nav (NavExit::Quit, nav_buffer_into_edit_state(buf, vk));
                        }
                        break 'nav (NavExit::Resume(initial_text.clone(), initial_cursor), nav_buffer_into_edit_state(buf, vk));
                    }
                    // Focus may have changed (apply_window_action already
                    // handled that) -- nothing to resume here (see this
                    // function's own doc comment on why a focus change
                    // doesn't carry the original text over).
                    CommandModeOutcome::Action(_) => {
                        return Ok((NavExit::Detached, nav_buffer_into_edit_state(buf, vk)));
                    }
                    // Back to normal mode too, per this session's own
                    // "go back to the original mode with the last output
                    // shown until next keypress" design -- but keep the
                    // result visible via PendingView::Output rather than
                    // just render_nav_frame's ordinary status bar (see
                    // PendingView's own doc comment for how it's resolved
                    // on the very next key). handle_command_mode has
                    // already done the baseline compositor_redraw this
                    // paints on top of.
                    CommandModeOutcome::Ran { output, status } => {
                        if !output.is_empty() || status != 0 {
                            render_command_output_overlay(&output, status, term_rows, term_cols);
                            pending_view = PendingView::Output;
                        } else {
                            render_nav_frame(&buf, &vk, rect);
                        }
                        continue;
                    }
                }
            }
            _ => {}
        }

        match vk.feed(key) {
            KeyOutcome::Motion(m, count) => {
                editor::apply_motion_or_reselect(&mut vk, &mut buf, m, count);
                scroll_to_show_cursor(&mut buf);
                render_nav_frame(&buf, &vk, rect);
            }
            // `ReadOnly`: hands typing back to the live prompt's own
            // editor (`apply_insert_cmd` against `original_chars`,
            // `NavExit::Resume` -- the mechanism that makes Ctrl+Space
            // feel like a temporary excursion out of Insert mode).
            // `Editable`: there's no live prompt underneath to resume --
            // this pane's own Normal mode already *is* the resting
            // state, so Insert mode is instead a nested sub-loop
            // (`fileeditor::run_insert_mode`) that returns straight back
            // here once it's done, same as any other mutating
            // `KeyOutcome`.
            KeyOutcome::EnterInsert(cmd) => {
                if matches!(buf, NavBuffer::Editable(_)) {
                    if let NavBuffer::Editable(tb) = &mut buf {
                        fileeditor::resolve_insert_start(tb, cmd);
                        fileeditor::run_insert_mode(tb, &mut vk, rect, registers, &mut || service_background_jobs(sessions, windows, job_frames, *current_window), false)?;
                    }
                    render_nav_frame(&buf, &vk, rect);
                } else {
                    let (new_chars, new_cursor) = crate::bishedit::vimkeys::apply_insert_cmd(&original_chars, initial_cursor, cmd);
                    break 'nav (NavExit::Resume(new_chars.into_iter().collect(), new_cursor), nav_buffer_into_edit_state(buf, vk));
                }
            }
            // `R`: `ReadOnly` degrades to a plain insert entry right at
            // the cursor (same simplification editor.rs's own identical
            // arm documents -- true Replace-mode overtype-as-you-type
            // behavior would need to live in the shell's own core typing
            // loop, which this excursion resumes into once it breaks out
            // here). `Editable` gets the real thing, via `run_insert_mode`'s
            // own `replace: true`.
            KeyOutcome::EnterReplace => {
                if matches!(buf, NavBuffer::Editable(_)) {
                    if let NavBuffer::Editable(tb) = &mut buf {
                        fileeditor::run_insert_mode(tb, &mut vk, rect, registers, &mut || service_background_jobs(sessions, windows, job_frames, *current_window), true)?;
                    }
                    render_nav_frame(&buf, &vk, rect);
                } else {
                    let (new_chars, new_cursor) =
                        crate::bishedit::vimkeys::apply_insert_cmd(&original_chars, initial_cursor, crate::bishedit::vimkeys::InsertCmd::Before);
                    break 'nav (NavExit::Resume(new_chars.into_iter().collect(), new_cursor), nav_buffer_into_edit_state(buf, vk));
                }
            }
            // `v`/`V`: arms Visual mode with the buffer's own current
            // cursor as the anchor (vimkeys.rs can't read that itself --
            // see `EnterVisual`'s own doc comment). Rendering (the
            // reverse-video highlight) and what `y`/`Z`/Escape/`d`/`c`/
            // `p`/`S` do from here on are handled by the guarded arms
            // above, at the top of this same loop.
            KeyOutcome::EnterVisual(shape) => {
                vk.begin_visual(shape, buf.cursor());
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::ReselectVisual => {
                if let Some((shape, anchor, cursor)) = vk.last_visual() {
                    buf.set_cursor(cursor.0, cursor.1);
                    vk.begin_visual(shape, anchor);
                }
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::Jump { forward } => {
                let current = buf.cursor();
                let target = if forward { vk.jump_forward(current) } else { vk.jump_back(current) };
                if let Some((row, col)) = target {
                    let row = row.min(buf.line_count() - 1);
                    let col = col.min(buf.line_len(row));
                    buf.set_cursor(row, col);
                    scroll_to_show_cursor(&mut buf);
                }
                render_nav_frame(&buf, &vk, rect);
            }
            // Yank works for either buffer kind (`op == Op::Yank` is
            // checked first, unconditionally) -- copying text out of a
            // pane's own scrollback/output is exactly what `ReadOnly`'s
            // view is for, and it's an ordinary read against `Editable`
            // too. `Delete`/`Change`/case-ops only mutate, so they're
            // gated to `Editable`; for `ReadOnly` they fall into the same
            // no-op as before this unification.
            KeyOutcome::Operator(op, motion, count, register) => {
                if op == Op::Yank {
                    editor::yank_motion(&mut buf, registers, motion, count, register);
                } else if let NavBuffer::Editable(tb) = &mut buf {
                    match op {
                        Op::Delete => {
                            fileeditor::delete_motion(tb, registers, motion, count, register);
                        }
                        Op::Change => {
                            let m = fileeditor::redirect_cw_to_ce(tb, &motion);
                            if fileeditor::delete_motion(tb, registers, m, count, register) {
                                fileeditor::run_insert_mode(tb, &mut vk, rect, registers, &mut || service_background_jobs(sessions, windows, job_frames, *current_window), false)?;
                            }
                        }
                        Op::Lowercase | Op::Uppercase | Op::CaseToggle => {
                            fileeditor::case_operator_motion(tb, motion, count, fileeditor::case_kind_for_op(op));
                        }
                        Op::Yank => unreachable!("handled above"),
                    }
                }
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::OperatorLines(op, count, register) => {
                if op == Op::Yank {
                    editor::yank_lines(&buf, registers, count, register);
                } else if let NavBuffer::Editable(tb) = &mut buf {
                    match op {
                        Op::Delete => fileeditor::delete_lines(tb, registers, count, register),
                        Op::Change => {
                            fileeditor::delete_lines(tb, registers, count, register);
                            fileeditor::run_insert_mode(tb, &mut vk, rect, registers, &mut || service_background_jobs(sessions, windows, job_frames, *current_window), false)?;
                        }
                        Op::Lowercase | Op::Uppercase | Op::CaseToggle => fileeditor::case_operator_lines(tb, count, fileeditor::case_kind_for_op(op)),
                        Op::Yank => unreachable!("handled above"),
                    }
                }
                render_nav_frame(&buf, &vk, rect);
            }
            // `p`/`P`/`x`/`J`/`gJ`/`ys`/`ds`/`cs`/`r`/`~`/`o`/`O`: all
            // mutate, so all a no-op for `ReadOnly` (same as before this
            // unification -- a view over already-rendered scrollback,
            // not an editable buffer), each calling the matching
            // `fileeditor::` helper for `Editable`.
            KeyOutcome::Put { before, count, register } => {
                if let NavBuffer::Editable(tb) = &mut buf {
                    fileeditor::put(tb, registers, before, count, register);
                }
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::DeleteCharForward { count, register } => {
                if let NavBuffer::Editable(tb) = &mut buf {
                    fileeditor::delete_char_forward(tb, registers, count, register);
                }
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::Join { count, with_space } => {
                if let NavBuffer::Editable(tb) = &mut buf {
                    tb.join_lines(count.unwrap_or(1).max(1), with_space);
                }
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::AddSurround { target, ch } => {
                if let NavBuffer::Editable(tb) = &mut buf {
                    fileeditor::add_surround(tb, target, ch);
                }
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::DeleteSurround { ch } => {
                if let NavBuffer::Editable(tb) = &mut buf {
                    fileeditor::delete_surround(tb, ch);
                }
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::ChangeSurround { ch, replacement } => {
                if let NavBuffer::Editable(tb) = &mut buf {
                    fileeditor::change_surround(tb, ch, replacement);
                }
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::ReplaceChar { ch, count } => {
                if let NavBuffer::Editable(tb) = &mut buf {
                    fileeditor::replace_char(tb, ch, count.unwrap_or(1).max(1));
                }
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::ToggleCase { count } => {
                if let NavBuffer::Editable(tb) = &mut buf {
                    fileeditor::toggle_case(tb, count.unwrap_or(1).max(1));
                }
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::AdjustNumber { delta } => {
                if let NavBuffer::Editable(tb) = &mut buf {
                    fileeditor::adjust_number(tb, delta);
                }
                render_nav_frame(&buf, &vk, rect);
            }
            KeyOutcome::OpenLine { above } => {
                if let NavBuffer::Editable(tb) = &mut buf {
                    fileeditor::open_line(tb, above);
                    fileeditor::run_insert_mode(tb, &mut vk, rect, registers, &mut || service_background_jobs(sessions, windows, job_frames, *current_window), false)?;
                }
                render_nav_frame(&buf, &vk, rect);
            }
            // dispatch_window_cmd does the actual work (shared with
            // run_edit_frame's own identical need -- see its own doc
            // comment); this loop just exits afterward, same as any
            // other Window outcome -- a focus change, nothing to resume,
            // handing the buffer/vk back so an `Editable` caller can
            // re-stash them.
            KeyOutcome::Window(cmd, count) => {
                dispatch_window_cmd(cmd, count, sessions, windows, current_window, next_session_id, next_window_id, sinks_are_grid, term_rows, term_cols);
                return Ok((NavExit::Detached, nav_buffer_into_edit_state(buf, vk)));
            }
            // Rendered on every keystroke, not just a resolved Motion --
            // the status bar needs to show a pending count/prefix (e.g.
            // "20g" mid-`20gg`) and a search's in-progress text live, not
            // just the end result once a motion actually applies.
            KeyOutcome::Pending | KeyOutcome::None => {
                render_nav_frame(&buf, &vk, rect);
            }
        }
    };

    compositor_redraw(sessions, windows, *current_window, term_rows, term_cols);
    Ok(result)
}

// Visual mode's own active (not yet committed via `Z`) selection, if any
// -- `vk`'s own anchor (see its `visual_anchor`'s own doc comment)
// ordered against `buf`'s current cursor into a `MotionRange` ready for
// `extract_text`/rendering. `RegisterShape::Char` maps to
// `MotionShape::Inclusive` (vim's own visual charwise is inclusive of
// both ends, unlike most charwise motions), `::Line` to `::Linewise`.
pub(crate) fn active_visual_range(vk: &VimKeys, buf: &impl BisheditBuffer) -> Option<motion::MotionRange> {
    let (shape, anchor) = vk.visual_anchor()?;
    let cursor = buf.cursor();
    let motion_shape = if shape == RegisterShape::Line { motion::MotionShape::Linewise } else { motion::MotionShape::Inclusive };
    let (from, to) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
    Some(motion::MotionRange { shape: motion_shape, from, to })
}

// `y` in Visual mode: every selection in `buf.selections` (already in
// commit order -- see that field's own doc comment), concatenated with no
// separator -- a `Linewise` part already ends in `\n` (`extract_text`
// bakes that in), so it naturally lands on its own line; a charwise part
// butts directly against its neighbor, matching what selecting exactly
// that much text and nothing else implies. Shape is `Line` if *any* part
// was `Linewise`, else `Char` -- the same "at least a full line no matter
// what gets combined with it" rule `RegisterBackend::write`'s own
// append-shape already uses for `"A`-style concatenation. A no-op if
// `selections` is empty (nothing was ever committed or active) -- the
// caller already gates on that before ever getting here. Generic over
// `impl BisheditBuffer` (not just `ScreenBuffer`) so `NavBuffer`'s own
// `ReadOnly` case can call this directly, same as `motion::extract_text`
// itself already is.
fn yank_selections(buf: &impl BisheditBuffer, selections: &[motion::MotionRange], registers: &mut Registers, register: Option<char>) {
    if selections.is_empty() {
        return;
    }
    let mut text = String::new();
    let mut shape = RegisterShape::Char;
    for range in selections {
        text.push_str(&motion::extract_text(buf, range));
        if range.shape == motion::MotionShape::Linewise {
            shape = RegisterShape::Line;
        }
    }
    registers.record_yank(register, RegisterValue { text, shape });
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
    registers: &mut Registers,
    term_rows: usize,
    term_cols: usize,
    // `Some` iff the pane driving this call is a `Frame::Edit` (the
    // unified Normal-mode loop's own `NavBuffer::Editable` -- see that
    // enum's doc comment) -- what makes `w`/`w <path>`/`wq`/`x`/`q`/`q!`/
    // `diag`/`diag clear` mean anything here at all: they're the file's
    // own save/quit/diagnose commands, not general window-management
    // ones, so they only exist when there's an actual buffer for them to
    // act on.
    editing: Option<&mut TextBuffer>,
    // Seeds the very first prompt with already-typed text, cursor at its
    // end -- Ctrl+Space mid-typing (below) uses this to carry the
    // in-progress line into command mode's own next prompt rather than
    // losing it. `None` (every other call site) starts with the ordinary
    // empty buffer.
    seed: Option<String>,
) -> CommandModeOutcome {
    let mut editing = editing;
    let mut buffer = String::new();
    let mut transcript_visible = false;
    let prompt_row = command_mode_row(term_rows) + 1;
    // Set from `seed` on the very first iteration, or by Ctrl+Space
    // below (see that arm's own comment) on any later one -- consumed by
    // the very next read_line call, then left None again either way.
    let mut pending_initial: Option<(String, usize)> = seed.map(|s| {
        let len = s.chars().count();
        (s, len)
    });
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
            None,
            false,
            None,
            registers,
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
                // `":"`: vim's own last-ex-command register, recorded
                // regardless of what the command turns out to be
                // (matching vim: even a failed `:nonsense` becomes the
                // new `":`) -- now that `:` is genuinely one command
                // mode, it records here rather than only when a
                // `Frame::Edit` pane's own w/wq/x/q/q! handling ran.
                registers.set_last_ex_command(trimmed.clone());

                // `w`/`w <path>`/`wq`/`x`/`q`/`q!`: the file's own
                // save/quit commands -- only mean anything when this
                // call is driving a `Frame::Edit` pane (`editing` is
                // `Some`), checked *before* the general `q`/`q!` case
                // just below so a dirty file's own `q` can be refused
                // instead of falling into "leave command mode" -- the
                // caller (the unified Normal-mode loop's own `:` arm,
                // which already knows which `NavBuffer` variant it's
                // driving) is what gives a plain `CommandModeOutcome::
                // Quit` its actual meaning either way, same as
                // `dispatch_window_cmd`'s existing "one outcome, meaning
                // depends on the caller" pattern for `<C-w>`.
                if let Some(tb) = editing.as_deref_mut() {
                    let (cmd, arg) = match trimmed.split_once(' ') {
                        Some((c, a)) => (c, Some(a.trim()).filter(|a| !a.is_empty())),
                        None => (trimmed.as_str(), None),
                    };
                    match cmd {
                        "w" | "write" => match tb.save(arg.map(std::path::Path::new)) {
                            Ok(()) => {
                                fileeditor::set_last_filename(tb, registers);
                                sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                    command: trimmed,
                                    output: String::new(),
                                    status: 0,
                                });
                                return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                            }
                            Err(e) => {
                                sessions[&session_id].shell.sink_err(&format!("bish: E212: Can't open file for writing: {e}\n"));
                                buffer.clear();
                                continue;
                            }
                        },
                        "wq" | "x" => match tb.save(arg.map(std::path::Path::new)) {
                            Ok(()) => {
                                fileeditor::set_last_filename(tb, registers);
                                sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                    command: trimmed,
                                    output: String::new(),
                                    status: 0,
                                });
                                return CommandModeOutcome::Quit;
                            }
                            Err(e) => {
                                sessions[&session_id].shell.sink_err(&format!("bish: E212: Can't open file for writing: {e}\n"));
                                buffer.clear();
                                continue;
                            }
                        },
                        "q" if tb.is_dirty() => {
                            sessions[&session_id].shell.sink_err("bish: E37: No write since last change (add ! to override)\n");
                            buffer.clear();
                            continue;
                        }
                        "q" | "q!" => {
                            sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                command: trimmed,
                                output: String::new(),
                                status: 0,
                            });
                            return CommandModeOutcome::Quit;
                        }
                        // `diag`/`diagnose` (no argument): runs every
                        // diagnose tool configured for this buffer's
                        // language (fileeditor::diagnose_buffer -- see its
                        // own doc comment for why that's a list, not one
                        // hardcoded linter) and stashes the result on the
                        // buffer itself, exactly where build_editor_frame's
                        // gutter/underline rendering already reads it from
                        // (TextBuffer::diagnostics). Same "only means
                        // something with a real buffer" gating as w/wq/x/q
                        // above -- there's nothing to diagnose without one.
                        "diag" | "diagnose" if arg.is_none() => {
                            tb.diagnostics = fileeditor::diagnose_buffer(tb);
                            let n = tb.diagnostics.len();
                            let output = if n == 0 { "No problems found.".to_string() } else { format!("{n} problem{} found.", if n == 1 { "" } else { "s" }) };
                            sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: output.clone(), status: 0 });
                            return CommandModeOutcome::Ran { output, status: 0 };
                        }
                        // `diag clear`/`diagnose clear`: drops whatever
                        // `:diag` last found, same as it self-clears the
                        // instant a real edit would make its positions
                        // stale (see TextBuffer::diagnostics's own doc
                        // comment) -- this is just the explicit, no-edit
                        // version of that.
                        "diag" | "diagnose" if arg == Some("clear") => {
                            tb.diagnostics.clear();
                            sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: String::new(), status: 0 });
                            return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                        }
                        "diag" | "diagnose" => {
                            sessions[&session_id].shell.sink_err(&format!("bish: diag: unknown subcommand '{}' (expected: clear)\n", arg.unwrap_or_default()));
                            buffer.clear();
                            continue;
                        }
                        _ => {}
                    }
                }

                if trimmed == "q" || trimmed == "q!" {
                    // Only reachable when `editing` is `None` (the block
                    // above already returns for both cases otherwise) --
                    // this pane isn't an editor, so `q`/`q!` mean "leave
                    // command mode" instead (the caller resumes whatever
                    // it was navigating, same as `dispatch_window_cmd`'s
                    // reused-outcome pattern). Recorded same as any other
                    // command that actually ran -- vim's own Ex quit
                    // commands, not shell builtins, but still something
                    // the user typed and command mode acted on, so
                    // leaving it out of the transcript would read as if
                    // it never happened.
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
                                    // Same reasoning as Fg just above --
                                    // this restricted read-eval loop has
                                    // no way to drive an interactive
                                    // editor session either.
                                    ExecResult::Edit => {
                                        sessions[&session_id].shell.sink_err("bish: e: not supported in command mode -- use it from the normal shell prompt\n");
                                        sessions.get_mut(&session_id).unwrap().shell.take_pending_edit();
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


#[cfg(test)]
mod visual_mode_tests {
    use super::*;

    // A `ScreenBuffer` fed with `text` (`\n` normalized to `\r\n` first,
    // matching a real pty's own line endings) -- the same construction
    // `run_normal_mode_navigation` itself goes through
    // (`ScreenBuffer::new` over a freshly-fed `vt100::Screen`), just
    // without the prompt-freezing/rect machinery this module's own tests
    // have no use for.
    fn make_screen_buffer(text: &str) -> ScreenBuffer {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(20, 40)));
        screen.borrow_mut().feed(text.replace('\n', "\r\n").as_bytes());
        ScreenBuffer::new(screen, 20)
    }

    #[test]
    fn yank_selections_concatenates_two_charwise_ranges_with_no_separator() {
        let mut buf = make_screen_buffer("hello world\nfoo bar");
        buf.selections = vec![
            motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 0), to: (0, 4) }, // "hello"
            motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (1, 0), to: (1, 2) }, // "foo"
        ];
        let mut registers = Registers::new_for_test();
        yank_selections(&buf, &buf.selections.clone(), &mut registers, None);
        let value = registers.read(None);
        assert_eq!(value.text, "hellofoo");
        assert_eq!(value.shape, RegisterShape::Char);
    }

    #[test]
    fn yank_selections_shape_is_line_if_any_range_is_linewise() {
        let mut buf = make_screen_buffer("hello world\nfoo bar");
        buf.selections = vec![
            motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 0), to: (0, 4) }, // "hello"
            motion::MotionRange { shape: motion::MotionShape::Linewise, from: (1, 0), to: (1, 0) },  // whole "foo bar" line
        ];
        let mut registers = Registers::new_for_test();
        yank_selections(&buf, &buf.selections.clone(), &mut registers, None);
        let value = registers.read(None);
        assert_eq!(value.text, "hellofoo bar\n");
        assert_eq!(value.shape, RegisterShape::Line);
    }

    #[test]
    fn yank_selections_is_a_no_op_when_nothing_is_selected() {
        let buf = make_screen_buffer("hello");
        let mut registers = Registers::new_for_test();
        registers.write(None, RegisterValue { text: "unchanged".to_string(), shape: RegisterShape::Char });
        yank_selections(&buf, &buf.selections.clone(), &mut registers, None);
        assert_eq!(registers.read(None).text, "unchanged");
    }

    #[test]
    fn selection_columns_in_line_linewise_spans_full_width_and_nothing_outside_its_rows() {
        let range = motion::MotionRange { shape: motion::MotionShape::Linewise, from: (2, 3), to: (4, 1) };
        assert_eq!(selection_columns_in_line(&range, 1, 10), None);
        assert_eq!(selection_columns_in_line(&range, 2, 10), Some((0, 10)));
        assert_eq!(selection_columns_in_line(&range, 3, 10), Some((0, 10)));
        assert_eq!(selection_columns_in_line(&range, 4, 10), Some((0, 10)));
        assert_eq!(selection_columns_in_line(&range, 5, 10), None);
    }

    #[test]
    fn selection_columns_in_line_charwise_clamps_first_and_last_row_and_spans_full_width_between() {
        let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (2, 5), to: (4, 3) };
        assert_eq!(selection_columns_in_line(&range, 1, 10), None);
        assert_eq!(selection_columns_in_line(&range, 2, 10), Some((5, 10)));
        assert_eq!(selection_columns_in_line(&range, 3, 10), Some((0, 10)));
        assert_eq!(selection_columns_in_line(&range, 4, 10), Some((0, 4)));
        assert_eq!(selection_columns_in_line(&range, 5, 10), None);
    }

    #[test]
    fn selection_columns_in_line_single_line_charwise_clamps_both_ends() {
        let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (2, 3), to: (2, 6) };
        assert_eq!(selection_columns_in_line(&range, 2, 10), Some((3, 7)));
    }
}

#[cfg(test)]
mod alt_screen_addressing_tests {
    use super::*;

    // Regression test for a real bug caught during interactive
    // verification ("entering window/pane normal when a fullscreen app
    // (vim) is open hides the content of the app"): with real
    // pre-existing scrollback in a pane before a fullscreen program
    // starts, `addressable_scrollback_len` returning the *primary*
    // grid's real scrollback length while the *alternate* screen is
    // active would push the combined-addressing cursor position (and
    // thus the viewport) mostly or entirely into that stale scrollback
    // instead of the program's own alternate-screen rows.
    #[test]
    fn addressable_scrollback_len_is_zero_on_the_alternate_screen_even_with_real_scrollback() {
        let mut screen = vt100::Screen::new(5, 20);
        // Scroll the primary grid well past its own height so real
        // scrollback accumulates.
        for i in 0..20 {
            screen.feed(format!("line {i}\r\n").as_bytes());
        }
        assert!(screen.scrollback.len() > 0, "primary grid should have real scrollback by now");
        let primary_len = screen.scrollback.len();

        screen.feed(b"\x1b[?1049h"); // switch to the alternate screen (vim/less/... on startup)
        assert!(screen.using_alternate);
        assert_eq!(addressable_scrollback_len(&screen), 0);

        screen.feed(b"\x1b[?1049l"); // back to the primary screen (vim/less/... on exit)
        assert!(!screen.using_alternate);
        assert_eq!(addressable_scrollback_len(&screen), primary_len);
    }

    // Same bug, exercised through `ScreenBuffer` itself (the actual
    // `Buffer` normal-mode navigation reads from) rather than the raw
    // helper directly: with real pre-existing primary-grid scrollback,
    // `line_count`/`char_at` must still address purely within the
    // alternate screen's own rows while it's active, not the stale
    // scrollback ahead of it.
    #[test]
    fn screen_buffer_addresses_only_the_alternate_screens_own_rows() {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(5, 20)));
        {
            let mut s = screen.borrow_mut();
            for i in 0..20 {
                s.feed(format!("line {i}\r\n").as_bytes());
            }
            assert!(s.scrollback.len() > 0);
            s.feed(b"\x1b[?1049h");
            s.feed(b"vim content here");
        }
        let buf = ScreenBuffer::new(screen, 5);
        // Only the one alternate-screen row with real content -- none of
        // the primary grid's real scrollback should be counted.
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.char_at(0, 0), Some('v'));
        let line: String = buf.line_chars(0).into_iter().collect();
        assert_eq!(line, "vim content here");
    }
}
