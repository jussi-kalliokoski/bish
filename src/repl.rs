use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::rc::Rc;

use crate::bishedit::completion;
use crate::bishedit::highlight::{self, HighlightContext};
use crate::bishedit::lint;
use crate::bishedit::motion;
use crate::bishedit::registers::{RegisterShape, RegisterValue, Registers};
use crate::bishedit::suggestion;
use crate::bishedit::textbuffer::TextBuffer;
use crate::bishedit::unicode_width::col_of;
use crate::bishedit::vimkeys::{KeyOutcome, Op, VimKeys, WindowCmd};
use crate::bishedit::Buffer as BisheditBuffer;
use crate::browser;
use crate::debugger;
use crate::docs;
use crate::editor::{self, Key, ReadOutcome};
use crate::exec::{self, DebugHook, ExecResult, PaneDirection, Shell, WindowAction};
use crate::fileeditor;
use crate::history::{self, History};
use crate::lexer::Lexer;
use crate::parser::{AndOr, Command, Parser, Pipeline, Program};
use crate::prompt;
use crate::pty;
use crate::session;
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
// TextBuffer's own content is definitely not Copy either). Diagnostics
// is different from all three: it's never the *only* frame a pane ever
// shows (see split_diagnostics_pane) -- it names the `:diag`-triggered
// sibling pane that sits below a specific Edit frame's own pane,
// browsing that same `EditFrameId`'s buffer.diagnostics. It carries no
// persistent state of its own (unlike Job/Edit, there's no third
// `diag_frames` map) -- everything it shows is re-read from
// `edit_frames[&id]` fresh each time it's focused, since nothing about
// a diagnostics list is worth preserving across a collapse.
//
// DebugRun is the same shape as Diagnostics (a `:dbg`-triggered sibling
// under a specific Edit frame's own pane, named by that same
// `EditFrameId`, see split_debug_run_pane) but -- like Job/Edit, unlike
// Diagnostics -- it *does* carry real persistent state worth keeping
// across a focus change (the debugged script's own Shell, output,
// pause/run state): a `debug_frames: HashMap<EditFrameId, debugger::
// DebugSession>` map, alongside `job_frames`/`edit_frames`.
#[derive(Clone, Copy, PartialEq)]
enum Frame {
    Session(SessionId),
    Job(JobFrameId),
    Edit(EditFrameId),
    Diagnostics(EditFrameId),
    DebugRun(EditFrameId),
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
//
// `fixed`, when `Some`, overrides all of that for this one child: it
// always gets exactly that many rows (a horizontal split) or columns (a
// vertical one), taken off the top before whatever's left divides among
// the `fixed: None` siblings by weight exactly as before (see
// compute_regions/split_sizes). `weight` is simply ignored while `fixed`
// is set, rather than the two interacting -- there's no case yet where
// a pane needs both "resizable" and "pinned" at once. The diagnostics
// pane (see Frame::Diagnostics) uses this while *expanded*, to size
// itself to its own list content -- capped by its own caller at half of
// whatever space was available before it expanded (see
// diagnostics_pane_rows), so this alone can't be used to demand more
// than half the split.
//
// `minimized` is a separate, more drastic state, for the same pane
// while *collapsed*: it folds down to exactly one row/column, and --
// unlike an ordinary `fixed: Some(1)` -- compute_regions skips
// reserving a *separate* divider line on either side of it, since a
// minimized pane's own single row already reads as that divider (see
// its own doc comment for the exact rule and render_diagnostics_title
// for what actually gets drawn into it: a title "pill" set into an
// otherwise ordinary-looking divider line, not a full-width bar).
// `fixed`/`weight` are simply irrelevant while `minimized` is set. Every
// `SplitChild` anywhere else stays `fixed: None, minimized: false` and
// behaves identically to before these fields existed.
struct SplitChild {
    layout: PaneLayout,
    weight: f64,
    fixed: Option<usize>,
    minimized: bool,
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

// Reserves the terminal's real last two rows for global chrome: the tab
// bar (render_compositor_frame, pinned to term_rows itself) and, one row
// above it, the global mode-line/command row (render_global_status_row,
// command_mode_row) -- everything panes/compute_regions ever divide up
// lives above both.
fn content_rows(term_rows: usize) -> usize {
    term_rows.saturating_sub(2).max(1)
}

// The one real exec::take_winch() consumer -- called from
// service_background_jobs, which every blocking inner loop's own
// on_idle already calls unconditionally (run_normal_mode_navigation,
// fileeditor::run_insert_mode, run_diagnostics_frame, run_command_mode,
// drive_fg_job), so a resize is noticed within one idle-poll tick no
// matter which of those is currently blocking on input, not just once
// that loop happens to return control back to run's own outer loop
// (which calls this exact same function directly, for the same reason,
// at its own top). Requeries the real size and, if it actually changed,
// resizes every session's own screen to match (pane rects/hit-testing/
// rendering everywhere then see the fresh size immediately) and every
// still-*background*-running job's own pty to match its session's
// (already-resized) screen -- a real terminal multiplexer propagates a
// resize straight through to whatever's running, not just the pane
// you're currently looking at. The currently fg'd job (if any) isn't
// found here: run_fg_job_frame already took it out of job_frames before
// driving it, so drive_fg_job catches that one's pty up itself, reacting
// to this same screen resize (see its own doc comment) rather than
// polling WINCH a second time.
fn poll_and_apply_resize(
    sessions: &HashMap<SessionId, SessionState>,
    windows: &[WindowEntry],
    job_frames: &mut HashMap<JobFrameId, exec::FgJob>,
    term_rows: &mut usize,
    term_cols: &mut usize,
    sinks_are_grid: bool,
    current_window: usize,
) -> bool {
    use std::os::unix::io::AsRawFd;
    if !exec::take_winch() {
        return false;
    }
    let (rows, cols) = query_term_size();
    if (rows, cols) == (*term_rows, *term_cols) {
        return false;
    }
    *term_rows = rows;
    *term_cols = cols;
    for s in sessions.values() {
        s.screen.borrow_mut().resize(content_rows(*term_rows), *term_cols);
    }
    for window in windows {
        for pane in &window.panes {
            if let Some(Frame::Job(job_frame_id)) = pane.stack.last()
                && let Some(job) = job_frames.get_mut(job_frame_id)
            {
                let sid = pane.owning_session();
                let (rows, cols) = sessions[&sid].screen.borrow().size();
                let _ = pty::set_size(job.pty_master().as_raw_fd(), rows as u16, cols as u16);
            }
        }
    }
    if sinks_are_grid {
        compositor_redraw(sessions, windows, current_window, *term_rows, *term_cols);
    }
    true
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
                    Frame::Job(_) | Frame::Edit(_) | Frame::Diagnostics(_) | Frame::DebugRun(_) => false,
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

pub fn run(mut shell: Shell, start_promoted: bool) {
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
    // Same idea again, for `:dbg`'s own attached sessions (Frame::
    // DebugRun) -- see Frame's own doc comment.
    let mut debug_frames: HashMap<EditFrameId, debugger::DebugSession> = HashMap::new();
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

    // `bish --promoted`: same one-time transition ensure_promoted performs
    // for the first window-family command a session runs, just done here
    // instead so the very first prompt already has the compositor's tab
    // bar/alt-screen up -- there's only ever the one root session at this
    // point, so this is safe to call before the main loop even starts.
    // The explicit compositor_redraw right after is what every other
    // ensure_promoted call site also does (see apply_window_action) --
    // without it, the tab bar wouldn't actually be drawn until the user's
    // first keystroke or a resize.
    if start_promoted {
        ensure_promoted(&mut sessions, &mut sinks_are_grid);
        compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
    }

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
        poll_and_apply_resize(&sessions, &windows, &mut job_frames, &mut term_rows, &mut term_cols, sinks_are_grid, current_window);

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
                &mut debug_frames,
                &mut cmd_history,
                &mut sinks_are_grid,
                &mut registers,
                &mut term_rows,
                &mut term_cols,
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
                &mut debug_frames,
                &mut cmd_history,
                &mut sinks_are_grid,
                &mut registers,
                &mut term_rows,
                &mut term_cols,
            );
            let _ = io::stdout().flush();
            continue;
        }

        // Same idea again, for the `:diag`-created diagnostics sibling
        // pane (see split_diagnostics_pane) -- reached whenever the user
        // navigates focus onto it (<C-w>j, or clicking its own
        // collapsed title bar).
        if let Frame::Diagnostics(edit_frame_id) = *windows[current_window].stack().last().unwrap() {
            run_diagnostics_frame(
                windows[current_window].focused_pane,
                edit_frame_id,
                &mut sessions,
                &mut windows,
                &mut current_window,
                &mut next_session_id,
                &mut next_window_id,
                &mut job_frames,
                &mut edit_frames,
                &mut sinks_are_grid,
                &mut term_rows,
                &mut term_cols,
            );
            let _ = io::stdout().flush();
            continue;
        }

        // Same idea again, for the `:dbg`-created debug-run sibling pane
        // (see split_debug_run_pane) -- reached whenever the user
        // navigates focus onto it directly while nothing is actually
        // running (a real run takes over the terminal itself -- see
        // run_debug_run_frame's own doc comment).
        if let Frame::DebugRun(edit_frame_id) = *windows[current_window].stack().last().unwrap() {
            run_debug_run_frame(
                windows[current_window].focused_pane,
                edit_frame_id,
                &mut sessions,
                &mut windows,
                &mut current_window,
                &mut next_session_id,
                &mut next_window_id,
                &debug_frames,
                &mut sinks_are_grid,
                &mut term_rows,
                &mut term_cols,
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
        // Same owned-snapshot pattern as cwd_snapshot/known_functions --
        // read_line's own abbrs param needs this session's current table
        // by the time expand_abbr_at_cursor runs, not a live borrow held
        // for the whole call (which would conflict with on_idle's own
        // &mut sessions borrow below, same reasoning as session_history).
        let abbrs_snapshot = sessions[&session_id].shell.abbrs.clone();
        // Same owned-snapshot pattern again -- this redraw's live
        // syn_col_* colors, resolved once up front rather than
        // re-querying the shell per span. See syntax_color_overrides'
        // own doc comment.
        let color_overrides = syntax_color_overrides(&sessions[&session_id].shell);
        let highlight_ctx =
            HighlightContext { cwd: Some(cwd_snapshot.as_path()), known_functions: Some(&known_functions), color_overrides: Some(&color_overrides) };
        // Same owned-snapshot pattern as cwd_snapshot/known_functions above
        // -- registered `complete NAME` specs, the contextual shell data
        // (aliases, PATH commands, jobs, ...) evaluating one needs, and a
        // functions/vars preamble for any -F/-C spec (which runs via
        // subprocess, so this snapshot is all it needs -- see compgen.rs's
        // own doc comment). One PATH scan per prompt (inside
        // action_context), not per keystroke -- same cost class as
        // known_functions/abbrs_snapshot just above.
        let completions_snapshot = sessions[&session_id].shell.completions_snapshot();
        let default_completion_snapshot = sessions[&session_id].shell.default_completion_snapshot();
        let action_ctx_snapshot = sessions[&session_id].shell.action_context();
        let preamble_snapshot = sessions[&session_id].shell.functions_preamble();
        // Same owned-snapshot pattern as highlight_ctx just above -- built
        // from the exact same locals, not re-snapshotted.
        let shell_completion = completion::ShellCompletionProvider {
            cwd: Some(cwd_snapshot.as_path()),
            known_functions: Some(&known_functions),
            completions: Some(&completions_snapshot),
            default_completion: default_completion_snapshot.as_ref(),
            action_ctx: Some(&action_ctx_snapshot),
            functions_preamble: Some(&preamble_snapshot),
        };
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

        // Ctrl-E's own live search input draws at the shared global
        // status row using this exact snapshot -- a plain `(usize,
        // usize)` copy taken before `on_idle` below gets its own `&mut`
        // borrow of the same `term_rows`/`term_cols` (see read_line's
        // own doc comment on this param for why a snapshot, not a live
        // reference, is the right trade here).
        let global_row_size = Some((term_rows, term_cols));
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
            &abbrs_snapshot,
            global_row_size,
            || {
                service_background_jobs(&mut sessions, &mut windows, &mut job_frames, current_window, &mut term_rows, &mut term_cols, sinks_are_grid);
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
            // A qualifying left click while typing at the plain prompt --
            // see ReadOutcome::Mouse's own doc comment for why read_line
            // hands this straight back up rather than acting on it
            // itself. Nothing to click on before the first promotion
            // (see ensure_promoted's own doc comment: no tab bar, no
            // multi-pane layout, just this one pane's whole own row) --
            // and a miss, or a click on the already-focused tab/pane,
            // both just resume typing exactly where it left off via
            // `pending_initial`, the same resume mechanism NormalMode's
            // own NavExit::Resume handling already uses below. A genuine
            // target change freezes this session's own in-progress text
            // into its grid first (freeze_input_with_text, not the plain
            // freeze_focused_idle_prompt -- SessionState.buffer never
            // mirrors read_line's own in-progress keystrokes) before
            // switching, so nothing typed so far is lost; the *next*
            // loop iteration's own read_line call then starts fresh for
            // whatever session is now focused, not resuming this text
            // there (switching panes/tabs must never relocate what was
            // being typed into a different pane's own prompt).
            Ok(ReadOutcome::Mouse { event, text, cursor }) => {
                let target = if sinks_are_grid { hit_test_click(event, &sessions, &windows, current_window, term_rows, term_cols) } else { None };
                match target {
                    Some(ClickTarget::Window(idx)) if idx != current_window => {
                        freeze_input_with_text(sessions.get_mut(&session_id).unwrap(), &text);
                        current_window = idx;
                        compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                    }
                    Some(ClickTarget::Pane(pane_id)) if pane_id != windows[current_window].focused_pane => {
                        freeze_input_with_text(sessions.get_mut(&session_id).unwrap(), &text);
                        windows[current_window].focused_pane = pane_id;
                        compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                    }
                    _ => pending_initial = Some((text, cursor)),
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
                    None,
                    &mut debug_frames,
                    &mut cmd_history,
                    &mut registers,
                    NavStart::Prompt { text, cursor },
                    &mut term_rows,
                    &mut term_cols,
                    None,
                ) {
                    Ok((NavExit::Resume(t, c), _)) => {
                        pending_initial = Some((t, c));
                        // Resuming straight back into ordinary prompt
                        // typing -- read_line's own redraw only ever
                        // touches its own prompt line, never the global
                        // mode-line row this excursion was just using
                        // (render_normal_mode_frame/render_global_status_
                        // row), so it'd otherwise sit there stale
                        // forever. A full compositor_redraw, same as
                        // every sibling arm here (Interrupted/DirNav/
                        // CtrlL) -- a targeted single-row clear instead
                        // left the real cursor parked on that row, which
                        // read_line's own *relative* prompt redraw then
                        // trusted as already being on row 0, landing the
                        // live prompt on the wrong row entirely.
                        if sinks_are_grid {
                            compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
                        }
                    }
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
                                // Every session sharing this one real
                                // process (see new_virtual_child's own
                                // doc comment on why "window new"/pane
                                // splits do) needs its own variables/umask
                                // restored onto the real process before
                                // it runs anything, and captured back
                                // right after -- otherwise a sibling
                                // session's own last command silently
                                // leaks in (or gets clobbered). Cheap and
                                // idempotent when this session was
                                // already the one last synced in.
                                session.shell.sync_real_state_in();
                                let result = session.shell.run_program(&prog);
                                session.shell.sync_real_state_out();
                                if session.shell.cwd != cwd_before {
                                    push_dir_history(session, session.shell.cwd.clone());
                                }
                                session.buffer.clear();
                                match result {
                                    ExecResult::Window(action) => window_action = Some(action),
                                    ExecResult::Fg => fg_pending = true,
                                    ExecResult::Edit => edit_pending = true,
                                    // The exit trap already ran at whichever
                                    // site produced this (see ExecResult::
                                    // Exit's own doc comment) -- matches
                                    // this codebase's prior behavior, where
                                    // `exit`/`set -e`/`set -u` called
                                    // std::process::exit directly and
                                    // unconditionally killed the whole
                                    // process, regardless of which
                                    // session/pane triggered it.
                                    ExecResult::Exit(code) => std::process::exit(code),
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
                        &mut debug_frames,
                        &mut cmd_history,
                        &mut sinks_are_grid,
                        &mut registers,
                        &mut term_rows,
                        &mut term_cols,
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
                    let args = sessions.get_mut(&session_id).unwrap().shell.take_pending_edit();
                    let rect = pane_rect(&windows[current_window], windows[current_window].focused_pane, term_rows, term_cols);
                    let opened = match fileeditor::parse_edit_args(&args) {
                        Ok(targets) => open_edit_targets(&targets, &mut sessions, session_id, &mut edit_frames, &mut next_edit_frame_id, rect),
                        Err(e) => {
                            sessions.get_mut(&session_id).unwrap().shell.sink_err(&format!("bish: e: {}\n", e));
                            Vec::new()
                        }
                    };
                    // Pushed back-to-front so the *first* file named is
                    // the one actually on top -- `e a b` opens both, with
                    // `a` in front and `b` revealed once it's closed,
                    // matching `vim a b`'s own argument order.
                    for id in opened.iter().rev() {
                        windows[current_window].stack_mut().push(Frame::Edit(*id));
                    }
                    if let Some(&edit_frame_id) = opened.first() {
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
                            &mut debug_frames,
                            &mut cmd_history,
                            &mut sinks_are_grid,
                            &mut registers,
                            &mut term_rows,
                            &mut term_cols,
                        );
                    } else {
                        // Nothing opened at all (a bad flag, or every
                        // named file failed) -- the pane keeps showing
                        // its shell, with whatever was sunk above now
                        // part of it.
                        compositor_redraw(&sessions, &windows, current_window, term_rows, term_cols);
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

// Opens every target `e` was given, in order, returning the frame ids
// that actually came up -- shared by the `e` builtin's own handling in
// `run()` and by `bish tool edit`'s bootstrap below, so the two open
// files identically. Pushing those ids onto a pane's frame stack (and
// deciding which to drive) is the caller's job: the two differ there,
// and only there.
//
// A target that fails to open is reported and skipped rather than
// abandoning the whole command -- `e a.sh missing-dir/b.sh` opening what
// it can is both what vim does and the more useful half of the outcome.
// The message goes to the owning session's own output, so it's still
// there underneath once every editor frame this opened is closed again.
fn open_edit_targets(
    targets: &[fileeditor::EditTarget],
    sessions: &mut HashMap<SessionId, SessionState>,
    session_id: SessionId,
    edit_frames: &mut HashMap<EditFrameId, fileeditor::EditSession>,
    next_edit_frame_id: &mut EditFrameId,
    rect: Rect,
) -> Vec<EditFrameId> {
    let mut opened = Vec::new();
    for target in targets {
        match fileeditor::EditSession::open(target.path.as_deref(), normal_mode_content_rows(rect)) {
            Ok(session) => {
                let id = *next_edit_frame_id;
                *next_edit_frame_id += 1;
                edit_frames.insert(id, session);
                opened.push(id);
            }
            Err(e) => {
                let what = target.path.as_deref().unwrap_or("<unnamed>");
                sessions.get_mut(&session_id).unwrap().shell.sink_err(&format!("bish: e: {}: {}\n", what, e));
            }
        }
    }
    opened
}

// `bish tool edit FILE` -- a minimal, single-session, single-window
// bootstrap that goes straight into a Frame::Edit, skipping the tab
// bar/window-switching chrome `run()`'s own full multi-session
// compositor loop exists for entirely (there's exactly one pane here,
// never split, so nothing to multiplex). Reuses run_edit_frame
// completely unchanged -- everything below it (Insert mode, every
// operator, `:w`/`:wq`/`:git`/`:diag`, ...) is the exact same real
// editor `e` drives, not a re-derived subset the way debugger.rs's own
// standalone read-only view is (that one deliberately reimplements only
// a small, non-mutating slice of Normal mode -- there'd be nothing left
// to reuse by re-deriving the *entire* editor here instead of just
// bootstrapping the real one).
//
// `ensure_promoted` is called directly (not `compositor_redraw`
// alongside it, the way `run()`'s own startup and every real `e`
// invocation do) -- it's what actually switches the terminal to the
// alternate screen buffer (vim/less-style clean takeover, restored on
// exit) via `Shell::promote_if_needed`; `render_editor_frame` (what
// actually paints the file content) always writes straight to the real
// terminal itself regardless of promotion, so the only thing skipping
// `compositor_redraw` loses is the tab bar -- exactly the "without
// windows" point of this entry point. If the user still explicitly
// invokes a window-family command (`<C-w>`/`:split`/`window new`) from
// inside it, that isn't specially blocked (no flag threaded through
// run_edit_frame/run_normal_mode_navigation for it -- see debugger.rs's
// own doc comment on why avoiding that kind of change to shared code is
// deliberate); run_edit_frame simply returns once focus genuinely
// leaves this one pane, and this function treats that exactly like a
// real quit -- there's no second window/pane here for it to drive.
pub fn run_edit(args: &[String]) -> i32 {
    match fileeditor::parse_edit_args(args) {
        Ok(targets) => run_edit_impl(&targets, false),
        Err(e) => {
            eprintln!("bish tool edit: {e}");
            2
        }
    }
}

// `bish tool debug FILE`'s own thin wrapper -- reuses this same minimal
// single-session/single-window bootstrap to open FILE, then immediately
// attaches a debug session to it, exactly what a bare `:dbg` does inside
// the real windowed editor. See debugger.rs's own top-of-file doc
// comment for the rest of the shape (a real, read-only source pane plus
// a real DebugRun sibling).
pub fn run_edit_debug(path: &str) -> i32 {
    run_edit_impl(&[fileeditor::EditTarget { path: Some(path.to_string()) }], true)
}

// `attach_debug` only ever comes with exactly one target (see
// run_edit_debug just above) -- it attaches to the *first* one opened,
// which is that same file.
fn run_edit_impl(targets: &[fileeditor::EditTarget], attach_debug: bool) -> i32 {
    let mut shell = exec::Shell::new();
    shell.enable_monitor_mode();
    let root_cwd = shell.cwd.clone();

    let mut cmd_history = History::load(".bish_cmd_history");
    let mut registers = Registers::new();
    let (mut term_rows, mut term_cols) = query_term_size();

    let mut sessions: HashMap<SessionId, SessionState> = HashMap::new();
    let root_screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(term_rows), term_cols)));
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
    let mut job_frames: HashMap<JobFrameId, exec::FgJob> = HashMap::new();
    let mut edit_frames: HashMap<EditFrameId, fileeditor::EditSession> = HashMap::new();
    let mut debug_frames: HashMap<EditFrameId, debugger::DebugSession> = HashMap::new();
    let mut sinks_are_grid = false;

    ensure_promoted(&mut sessions, &mut sinks_are_grid);

    let rect = pane_rect(&windows[current_window], windows[current_window].focused_pane, term_rows, term_cols);
    // Unlike the `e` builtin (see open_edit_targets, which reports a bad
    // target and opens the rest), a failure here aborts the whole
    // invocation: this is a one-shot command line, so a path that can't
    // be opened is a usage error worth an exit status, not a note that
    // scrolls past in a session that keeps running.
    let mut sessions_to_open = Vec::new();
    for (i, target) in targets.iter().enumerate() {
        match fileeditor::EditSession::open(target.path.as_deref(), normal_mode_content_rows(rect)) {
            Ok(s) => sessions_to_open.push((i as EditFrameId + 1, s)),
            Err(e) => {
                // Leaving promotion behind here too: promote_if_needed already
                // switched to the alternate screen before this could fail.
                print!("\x1b[?1049l");
                let _ = io::stdout().flush();
                eprintln!("bish: {}: {}", target.path.as_deref().unwrap_or("<unnamed>"), e);
                return 1;
            }
        }
    }
    let edit_frame_id: EditFrameId = 1;
    if attach_debug {
        let path = targets[0].path.as_deref().unwrap_or_default();
        match debugger::DebugSession::attach(std::path::Path::new(path)) {
            Ok(debug_session) => {
                sessions_to_open[0].1.buffer.set_readonly(true);
                debug_frames.insert(edit_frame_id, debug_session);
                let (pane_id, _screen) = split_debug_run_pane(&mut sessions, &mut windows, current_window, &mut next_session_id, edit_frame_id, term_rows, term_cols);
                let sid = windows[current_window].pane(pane_id).owning_session();
                render_debug_run_title(&sessions[&sid].screen, term_cols, "attached -- :dbg run to start");
            }
            Err(e) => {
                print!("\x1b[?1049l");
                let _ = io::stdout().flush();
                eprintln!("bish: dbg: {}: {}", path, e);
                return 1;
            }
        }
    }
    let ids: Vec<EditFrameId> = sessions_to_open.iter().map(|(id, _)| *id).collect();
    for (id, session) in sessions_to_open {
        edit_frames.insert(id, session);
    }
    // Back-to-front, so the first file named is the one on top -- see
    // the `e` builtin's own identical push in run().
    for id in ids.iter().rev() {
        windows[current_window].stack_mut().push(Frame::Edit(*id));
    }

    // Loops rather than driving `edit_frame_id` once: with several files
    // open, closing the top one has to hand control to the next frame
    // down, which is exactly what `run()`'s own main loop does for the
    // builtin. Terminates because run_edit_frame only ever returns once
    // its own frame is no longer this pane's top one (it pops on `:q`,
    // and keeps driving through a no-op detach itself -- see its own
    // `Detached` arm).
    while let Some(&Frame::Edit(id)) = windows[current_window].stack().last() {
        run_edit_frame(
            id,
            0,
            &mut sessions,
            &mut windows,
            &mut current_window,
            &mut next_session_id,
            &mut next_window_id,
            &mut job_frames,
            &mut edit_frames,
            &mut debug_frames,
            &mut cmd_history,
            &mut sinks_are_grid,
            &mut registers,
            &mut term_rows,
            &mut term_cols,
        );
    }

    // Same restoration run()'s own final-exit path does -- see its own
    // doc comment on why this is a direct terminal write, not anything
    // routed through a session's own sink.
    print!("\x1b[?1049l");
    let _ = io::stdout().flush();
    0
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
    debug_frames: &mut HashMap<EditFrameId, debugger::DebugSession>,
    registers: &mut Registers,
    term_rows: &mut usize,
    term_cols: &mut usize,
    editing: Option<&mut TextBuffer>,
    seed: Option<String>,
) -> CommandModeOutcome {
    let outcome = run_command_mode(
        session_id, sessions, windows, *current_window, next_session_id, cmd_history, job_frames, debug_frames, registers, term_rows, term_cols, *sinks_are_grid, editing, seed,
    );
    match outcome {
        CommandModeOutcome::Action(action) => {
            apply_window_action(action, sessions, windows, current_window, next_session_id, next_window_id, sinks_are_grid, *term_rows, *term_cols);
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
                compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
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
    debug_frames: &mut HashMap<EditFrameId, debugger::DebugSession>,
    cmd_history: &mut History,
    sinks_are_grid: &mut bool,
    registers: &mut Registers,
    term_rows: &mut usize,
    term_cols: &mut usize,
) {
    // Taken out of job_frames (rather than borrowed via get_mut) so the
    // on_idle closure below can freely borrow job_frames itself to
    // service every *other* window's job -- see service_background_jobs.
    let mut job = job_frames.remove(&job_frame_id).expect("Frame::Job always has a live job_frames entry");
    let focused_screen = sessions[&session_id].screen.clone();
    let mut tab_bar = tab_bar_line(sessions, windows, *current_window);
    let mut layout = snapshot_window(&windows[*current_window], sessions, *term_rows, *term_cols);
    let cw = *current_window;
    // Persists across every drive_fg_job call in this loop (reset to None
    // -- forcing the next redraw back to a full repaint -- only on
    // FgOutcome::Resized below, alongside layout/tab_bar's own
    // re-snapshot) so a quiet job's later output keeps diffing against
    // what's actually already on screen instead of re-clearing every
    // time. See render_compositor_frame_diff's own doc comment.
    let mut frame_cache: Option<TerminalFrame> = None;
    // Loops rather than returning straight after one call: a qualifying
    // click that turns out to be for the job itself, not bish's own UI
    // chrome (FgOutcome::MouseClick's own doc comment), or a resize
    // (FgOutcome::Resized's own doc comment), gets handled in place and
    // this keeps driving the very same job -- every other outcome still
    // returns straight out of this function, same as before this loop
    // existed.
    loop {
        // A plain owned copy, not `term_rows` itself: the `redraw`
        // closure below and the `on_idle` closure both close over state
        // for this same drive_fg_job call, and on_idle needs `term_rows`
        // as a genuine `&mut usize` (to drive service_background_jobs's
        // own resize handling) -- capturing it a second time here, even
        // just to read it, would conflict with that. A resize that lands
        // mid-drive is caught by drive_fg_job itself instead (see its
        // own doc comment) and only takes effect for *this* closure once
        // the FgOutcome::Resized arm below re-snapshots and loops back
        // around, at most one drive_fg_job iteration later.
        let redraw_rows = *term_rows;
        let redraw_cols = *term_cols;
        let outcome = drive_fg_job(
            &mut job,
            &focused_screen,
            || render_compositor_frame_diff(&layout, &tab_bar, redraw_rows, redraw_cols, &mut frame_cache),
            || { service_background_jobs(sessions, windows, job_frames, cw, term_rows, term_cols, *sinks_are_grid); },
        );
        match outcome {
        FgOutcome::Exited(status) => {
            windows[*current_window].stack_mut().pop();
            sessions.get_mut(&session_id).unwrap().shell.last_status = status;
            if *sinks_are_grid {
                compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
            }
            return;
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
                None,
                debug_frames,
                cmd_history,
                registers,
                NavStart::JobDetach,
                term_rows,
                term_cols,
                None,
            );
            // Whatever that excursion did (window action, or plain EOF)
            // hands control straight back to re-driving this same job
            // live -- which only redraws once fresh output actually
            // arrives (drive_fg_job's own redraw callback). A quiet job
            // could sit for a while with the global mode-line row
            // (render_normal_mode_frame's own render_global_status_row)
            // still showing this excursion's now-stale text otherwise.
            if *sinks_are_grid {
                compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
            }
            return;
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
                compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
            }
            return;
        }
        FgOutcome::MouseClick(ev) => {
            // sinks_are_grid gate matches every other hit_test_click
            // call site: there's no tab bar/pane layout to click on
            // before the first promotion, so there's nothing to
            // hit-test against -- falls straight to the "forward it"
            // arm below, same as a genuine miss.
            let target = if *sinks_are_grid {
                hit_test_click(ev, sessions, windows, *current_window, *term_rows, *term_cols)
            } else {
                None
            };
            match target {
                Some(ClickTarget::Window(idx)) if idx != *current_window => {
                    job_frames.insert(job_frame_id, job);
                    freeze_focused_idle_prompt(sessions, windows, *current_window);
                    *current_window = idx;
                    if *sinks_are_grid {
                        compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                    }
                    return;
                }
                Some(ClickTarget::Pane(pane_id)) if pane_id != windows[*current_window].focused_pane => {
                    job_frames.insert(job_frame_id, job);
                    windows[*current_window].focused_pane = pane_id;
                    if *sinks_are_grid {
                        compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                    }
                    return;
                }
                // A miss, the already-focused tab/pane, or sinks_are_grid
                // was false -- this click was for the job itself (or
                // nothing meaningful bish owns either way). Reconstruct
                // the exact SGR press byte drive_fg_job held back and
                // forward it, then keep driving the same job (the
                // enclosing loop, not a recursive call).
                _ => {
                    let seq = format!("\x1b[<{};{};{}M", ev.button, ev.col, ev.row);
                    let _ = job.pty_master().write_all(seq.as_bytes());
                }
            }
        }
        FgOutcome::Resized => {
            // service_background_jobs's own on_idle-driven WINCH handling
            // (poll_and_apply_resize) already resized every session's
            // screen and every *background* job's pty; drive_fg_job
            // already caught this job's own pty up too. All that's left
            // is this function's own pane-rects/tab-bar snapshot, taken
            // before this loop started and now stale -- without
            // recomputing it, the compositor would keep drawing this
            // job's now-correctly-sized output into the old geometry
            // until this whole function eventually returned. No redraw
            // forced here: drive_fg_job's own redraw() fires again as
            // soon as the job's next output arrives, which for a
            // well-behaved full-screen program is basically immediate
            // (TIOCSWINSZ delivers SIGWINCH to its own process group).
            layout = snapshot_window(&windows[*current_window], sessions, *term_rows, *term_cols);
            tab_bar = tab_bar_line(sessions, windows, *current_window);
            // The geometry frame_cache's own cells describe is no longer
            // even the right shape -- render_compositor_frame_diff's own
            // stale check would catch a rows/cols mismatch anyway, but
            // resetting explicitly here is what forces the *next* redraw
            // back to a full, self-healing repaint (matching
            // poll_and_apply_resize's own direct compositor_redraw call,
            // which already painted a full frame with the new geometry
            // via a completely different path this cache knows nothing
            // about) rather than potentially diffing against content from
            // before the resize.
            frame_cache = None;
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
    debug_frames: &mut HashMap<EditFrameId, debugger::DebugSession>,
    cmd_history: &mut History,
    sinks_are_grid: &mut bool,
    registers: &mut Registers,
    term_rows: &mut usize,
    term_cols: &mut usize,
) {
    // Taken out of edit_frames (rather than borrowed via get_mut) so
    // on_idle below can freely borrow edit_frames -- moot today (nothing
    // in service_background_jobs touches it), but matches
    // run_fg_job_frame's own reasoning for job_frames exactly, and keeps
    // the two symmetric.
    let session = edit_frames.remove(&edit_frame_id).expect("Frame::Edit always has a live edit_frames entry");
    // "%": refreshed here, once, whenever this function starts driving a
    // session -- see fileeditor::set_last_filename's own doc comment for
    // the other place (a successful :w/:wq/:x) it needs the same
    // refresh.
    fileeditor::set_last_filename(&session.buffer, registers);
    let mut buffer = session.buffer;
    let mut vk = session.vk;
    // This whole function's own lifetime, not re-resolved per keystroke:
    // bishopt is a plain shell builtin, unreachable from inside the modal
    // file editor this function drives, so the owning session's colors
    // genuinely can't change while this runs (only detaching back to a
    // real prompt, changing them there, and re-focusing this editor --
    // a fresh call to this same function -- could).
    let color_overrides = syntax_color_overrides(&sessions[&session_id].shell);
    // Where this editor frame actually lives -- captured once, before
    // any window command can move focus elsewhere. The Detached arm
    // below needs this pane's own rect to freeze into, not whatever
    // pane/window happens to be focused *after* the command that
    // detached this one ran (that could be a sibling pane a split just
    // switched to, or even a different window entirely after `<C-w>w`/
    // `window next` -- `windows[*current_window].focused_pane` at that
    // point names neither).
    let own_window = *current_window;
    let own_pane_id = windows[*current_window].focused_pane;
    // Loops rather than returning straight after one call: a `Window`
    // keypress (KeyOutcome::Window, handled inside run_normal_mode_
    // navigation) always resolves to NavExit::Detached, even when
    // dispatch_window_cmd's own action turned out to be a no-op from
    // *this* pane's perspective (`<C-w>n` with only one window, `<C-w>h`
    // with nowhere to go, ...) -- that loop has no way to tell a real
    // focus change from a no-op one. See the `Detached` arm below for
    // what goes wrong if this pane's own frame is still on top after
    // one of those and this function returns anyway instead of noticing
    // and just continuing to drive it.
    loop {
        let outcome = run_normal_mode_navigation(
            session_id,
            sessions,
            windows,
            current_window,
            next_session_id,
            next_window_id,
            sinks_are_grid,
            job_frames,
            Some(edit_frame_id),
            debug_frames,
            cmd_history,
            registers,
            NavStart::Edit(Box::new(buffer), Box::new(vk)),
            term_rows,
            term_cols,
            Some(&color_overrides),
        );
        match outcome {
            Ok((NavExit::Quit, _)) => {
                windows[*current_window].stack_mut().pop();
                // Reverting to a plain shell prompt in this same session
                // -- clear whatever the editor last drew into its own
                // screen (freeze_editor_frame always writes pane-relative
                // from row 0, so any row at or past this editor's own
                // last rect otherwise just sits there forever: the
                // shell's own prompt draw, freeze_idle_prompt, only ever
                // touches its own single line, never the rest of the
                // grid). Matches how a real terminal full-screen program
                // (vim, less, ...) leaves the screen on exit.
                sessions.get_mut(&session_id).unwrap().screen.borrow_mut().feed(b"\x1b[2J\x1b[H");
                // A file with an open diagnostics pane (`:diag`) below
                // it -- close that too rather than leaving an orphaned
                // pane no `Frame::Edit` will ever point at again.
                if let Some(diag_pane) = diagnostics_sibling(&windows[*current_window], edit_frame_id) {
                    close_pane(&mut windows[*current_window], diag_pane);
                    close_orphaned_sessions(sessions, windows);
                }
                // Same idea for an attached `:dbg` session's own
                // DebugRun sibling -- the buffer this session was
                // attached to is going away regardless of whether
                // `:dbg quit` was ever run explicitly, so there's
                // nothing left for it to debug.
                if debug_frames.remove(&edit_frame_id).is_some()
                    && let Some(debug_pane) = debug_run_sibling(&windows[*current_window], edit_frame_id)
                {
                    close_pane(&mut windows[*current_window], debug_pane);
                    close_orphaned_sessions(sessions, windows);
                }
                if *sinks_are_grid {
                    compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                }
                // Command mode's own colon-line (`:q`/`:q!`/`:wq`/`:x`)
                // paints straight to the real terminal's global status row
                // (see run_command_mode's own doc comment on why -- it
                // bypasses the session's vt100 grid model entirely), which
                // means it's invisible to compositor_redraw's own grid-
                // diffed repaint just above: nothing else ever re-touches
                // that row on the way out, so the colon-line's own last
                // text (literally "q!") would otherwise sit there forever
                // once this editor pane is gone.
                print!("{}", erase_global_status_row(*term_rows));
                let _ = io::stdout().flush();
                return;
            }
            Ok((NavExit::Detached, Some((b, v)))) => {
                // If this exact edit frame is still the top of the
                // (possibly now-different) current window's own focused
                // pane, nothing actually took over -- freezing here
                // would bake this frame's *current* content into the
                // session's grid for no reason, where it would then sit
                // untouched (nothing else feeds that grid while an
                // editor pane is being driven -- see freeze_editor_
                // frame's own doc comment on why this is normally the
                // *only* place that happens) until some later
                // compositor_redraw painted it back onto the real
                // terminal on top of whatever a genuine exit draws
                // afterward -- exactly the "shell prompt drawn over the
                // file's own last content" bug this loop exists to
                // avoid. Keep driving it instead.
                if windows[*current_window].stack().last() == Some(&Frame::Edit(edit_frame_id)) {
                    buffer = b;
                    vk = v;
                    continue;
                }
                // A genuine focus change (or this pane now shows
                // something else entirely, e.g. `window fg` pushed a
                // different frame on top) -- freeze for real. Recomputed
                // fresh rather than reusing anything computed before
                // this loop's first iteration: a window action that
                // *does* leave this frame on top (the branch above) can
                // still have resized panes (`window balance`, `+`/`-`),
                // and one that moves focus away can too.
                let rect = if own_window < windows.len() && windows[own_window].panes.iter().any(|p| p.id == own_pane_id) {
                    pane_rect(&windows[own_window], own_pane_id, *term_rows, *term_cols)
                } else {
                    // This pane (or its whole window) is gone by now --
                    // nothing sensible to freeze into; fall back to
                    // wherever focus actually landed rather than
                    // indexing a pane that no longer exists.
                    pane_rect(&windows[*current_window], windows[*current_window].focused_pane, *term_rows, *term_cols)
                };
                fileeditor::freeze_editor_frame(&sessions[&session_id].screen, &b, &v, rect, Some(&color_overrides));
                edit_frames.insert(edit_frame_id, fileeditor::EditSession { buffer: b, vk: v });
                return;
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
                    compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                }
                return;
            }
        }
    }
}

// The Edit-frame pane whose own diagnostics `edit_frame_id` names, if
// it's currently live in this window -- diagnostics_sibling's own
// mirror image, walked the other direction (from the diagnostics pane
// back to the editor pane it belongs to, e.g. to refocus it once the
// diagnostics pane collapses).
fn editor_pane_for(window: &WindowEntry, edit_frame_id: EditFrameId) -> Option<PaneId> {
    window.panes.iter().find(|p| p.stack.last() == Some(&Frame::Edit(edit_frame_id))).map(|p| p.id)
}

// How many rows the diagnostics pane's own SplitChild should claim
// while expanded (focused): sized to fit its own list content, plus
// DIAG_DETAIL_ROWS more while the selected item's own detail block is
// showing (see Space's own handling) -- but capped at half of `budget`
// regardless of how long the list or the detail gets, per this
// feature's own spec ("only the space it needs, up to 50%"). `budget`
// is the split's own total (the editor sibling's rows *plus* the one
// row a minimized pane's own divider-pill costs -- see SplitChild's own
// doc comment), captured once when this pane is first focused and
// reused for every resize during that same focus session, not
// recomputed from the *already-shrunk* editor rect on every keystroke
// (which would make the cap itself shrink every time the pane grew).
const DIAG_DETAIL_ROWS: usize = 2;
fn diagnostics_pane_rows(diagnostics_len: usize, show_detail: bool, budget: usize) -> usize {
    let max_rows = (budget / 2).max(1);
    let wanted = if show_detail { diagnostics_len + DIAG_DETAIL_ROWS } else { diagnostics_len };
    wanted.max(1).min(max_rows)
}

// The diagnostics pane's own interactive list -- dispatched from repl::
// run's main loop whenever a pane's top frame is `Frame::Diagnostics
// (edit_frame_id)`, the same tier as run_fg_job_frame/run_edit_frame
// just above. Deliberately not built on bishedit::Buffer/VimKeys::
// feed's motion machinery the way run_normal_mode_navigation is (see
// Frame::Diagnostics's own doc comment) -- a flat list with nothing to
// edit doesn't need vim's word/line motions, registers, or Insert mode.
// Still constructs a bare VimKeys purely to reuse its already-correct
// `<C-w>` chord parsing (KeyOutcome::Window -> dispatch_window_cmd, the
// exact same call every other loop makes) and its plain Motion::Down/Up
// parsing (so `5j`/`3k` move the selection by count too, for free) --
// every other key is intercepted before `vk.feed`, same tier run_
// normal_mode_navigation's own Visual-mode `y`/`d`/`Z` interception
// already uses. Selection/expanded-detail state is purely local to this
// call (see Frame::Diagnostics's own doc comment on why it's never
// worth persisting) -- every (re-)entry starts fresh at row 0,
// collapsed detail.
#[allow(clippy::too_many_arguments)]
fn run_diagnostics_frame(
    pane_id: PaneId,
    edit_frame_id: EditFrameId,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: &mut usize,
    next_session_id: &mut SessionId,
    next_window_id: &mut u32,
    job_frames: &mut HashMap<JobFrameId, exec::FgJob>,
    edit_frames: &mut HashMap<EditFrameId, fileeditor::EditSession>,
    sinks_are_grid: &mut bool,
    term_rows: &mut usize,
    term_cols: &mut usize,
) {
    // Every other loop that reads keys directly off the terminal
    // (run_normal_mode_navigation, run_insert_mode, ...) holds one of
    // these for its own duration -- without it the terminal is left in
    // whatever mode the *previous* loop's own guard restored it to on
    // return (cooked/canonical, echoing input back and line-buffering
    // until Enter), which is not remotely usable for a `j`/`k`/`Enter`/
    // `Space`-driven list. Silently returns (pane stays exactly as it
    // last rendered) on the -- essentially unreachable in practice --
    // chance this fails, same tolerance a failed read gets elsewhere.
    let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else { return };

    let mut vk = VimKeys::new();
    let mut selected: usize = 0;
    let mut expanded: Option<usize> = None;

    // Captured once, before this pane grows at all -- see
    // diagnostics_pane_rows's own doc comment for why this can't just
    // be recomputed from the editor's own (by-then-shrunk) rect on
    // every later resize instead.
    let budget = editor_pane_for(&windows[*current_window], edit_frame_id)
        .map(|id| pane_rect(&windows[*current_window], id, *term_rows, *term_cols).rows + 1)
        .unwrap_or(*term_rows);

    let set_expanded = |windows: &mut Vec<WindowEntry>, current_window: usize, rows: usize| {
        if let Some((_, children, idx)) = find_parent_split_mut(&mut windows[current_window].layout, pane_id) {
            children[idx].minimized = false;
            children[idx].fixed = Some(rows);
        }
    };
    let set_minimized = |windows: &mut Vec<WindowEntry>, current_window: usize| {
        if let Some((_, children, idx)) = find_parent_split_mut(&mut windows[current_window].layout, pane_id) {
            children[idx].minimized = true;
            children[idx].fixed = None;
        }
    };
    // Whenever set_expanded/set_minimized change this pane's own size,
    // the editor sibling's size changes right along with it (the split
    // only has the two of them) -- but that sibling's own last frozen
    // render (freeze_editor_frame, from run_edit_frame's own Detached
    // arm, or an earlier call to this very closure) was addressed
    // against whatever rect was current *then*. compositor_redraw's own
    // resize of a pane's session screen keeps existing rows in place and
    // either truncates or leaves the rest blank -- it does not reflow
    // content -- so a stale frozen frame's status line (always its own
    // rect's *last* row) ends up either clipped off (shrinking) or
    // stranded above new blank rows (growing) once the split resizes
    // out from under it. Re-freezing against the sibling's current rect
    // keeps it honest across every resize this loop causes.
    // Takes term_rows/term_cols as plain parameters (fresh at each call
    // site below), not captured -- capturing them here (even just to
    // read) would hold a borrow alive across this closure's whole
    // lifetime (it's called repeatedly, throughout this function), which
    // would conflict with the on_idle closure's own need to reborrow
    // them mutably for service_background_jobs's resize handling.
    let refresh_editor_frame = |sessions: &HashMap<SessionId, SessionState>, windows: &Vec<WindowEntry>, edit_frames: &HashMap<EditFrameId, fileeditor::EditSession>, current_window: usize, term_rows: usize, term_cols: usize| {
        let Some(editor_pane) = editor_pane_for(&windows[current_window], edit_frame_id) else { return };
        let Some(session) = edit_frames.get(&edit_frame_id) else { return };
        let rect = pane_rect(&windows[current_window], editor_pane, term_rows, term_cols);
        let sid = windows[current_window].pane(editor_pane).owning_session();
        let color_overrides = syntax_color_overrides(&sessions[&sid].shell);
        let screen = &sessions[&sid].screen;
        screen.borrow_mut().resize(rect.rows, rect.cols);
        fileeditor::freeze_editor_frame(screen, &session.buffer, &session.vk, rect, Some(&color_overrides));
    };
    let diagnostics_len = edit_frames.get(&edit_frame_id).map(|s| s.buffer.diagnostics.len()).unwrap_or(0);
    set_expanded(windows, *current_window, diagnostics_pane_rows(diagnostics_len, false, budget));
    refresh_editor_frame(sessions, windows, edit_frames, *current_window, *term_rows, *term_cols);
    compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);

    loop {
        let rect = pane_rect(&windows[*current_window], pane_id, *term_rows, *term_cols);
        if let Some(session) = edit_frames.get(&edit_frame_id) {
            render_diagnostics_list_frame(&session.buffer, rect, selected, expanded);
        }

        let key = match editor::read_key_idle(&mut || {
            service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid);
        }) {
            Ok(Some(k)) => k,
            // EOF/error: nothing to resume -- leave the pane exactly as
            // it last rendered, same as every other loop's own EOF arm.
            Ok(None) | Err(_) => return,
        };

        if let Key::Mouse(ev) = key {
            if ev.is_left_click() {
                let row0 = (ev.row as usize).saturating_sub(1);
                let diagnostics_len = edit_frames.get(&edit_frame_id).map(|s| s.buffer.diagnostics.len()).unwrap_or(0);
                if row0 >= rect.row {
                    let idx = row0 - rect.row;
                    if idx < diagnostics_len {
                        selected = idx;
                        expanded = None;
                    }
                }
            }
            continue;
        }

        // A pane-focus-changing sibling reason to leave -- collapses
        // back to the title row and hands focus to the editor pane this
        // diagnostics pane belongs to, refreshing that pane's own
        // collapsed title first so it reflects whatever's still true
        // (only matters after `f` actually changed the count below, but
        // cheap enough to always do).
        let leave = |windows: &mut Vec<WindowEntry>, sessions: &HashMap<SessionId, SessionState>, edit_frames: &mut HashMap<EditFrameId, fileeditor::EditSession>, jump: bool| {
            if let Some(session) = edit_frames.get_mut(&edit_frame_id) {
                if jump && let Some(d) = session.buffer.diagnostics.get(selected).cloned() {
                    let (line, col) = fileeditor::diagnostic_position(&session.buffer, d.start);
                    session.buffer.set_cursor(line, col);
                }
                let sid = windows[*current_window].pane(pane_id).owning_session();
                render_diagnostics_title(&sessions[&sid].screen, rect.cols, &session.buffer.diagnostics);
            }
            set_minimized(windows, *current_window);
            if let Some(editor_pane) = editor_pane_for(&windows[*current_window], edit_frame_id) {
                windows[*current_window].focused_pane = editor_pane;
            }
        };

        match key {
            Key::Enter => {
                leave(windows, sessions, edit_frames, true);
                refresh_editor_frame(sessions, windows, edit_frames, *current_window, *term_rows, *term_cols);
                compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                return;
            }
            Key::Escape | Key::Char('q') => {
                leave(windows, sessions, edit_frames, false);
                refresh_editor_frame(sessions, windows, edit_frames, *current_window, *term_rows, *term_cols);
                compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                return;
            }
            Key::Char(' ') => {
                expanded = if expanded == Some(selected) { None } else { Some(selected) };
                let diagnostics_len = edit_frames.get(&edit_frame_id).map(|s| s.buffer.diagnostics.len()).unwrap_or(0);
                set_expanded(windows, *current_window, diagnostics_pane_rows(diagnostics_len, expanded.is_some(), budget));
                refresh_editor_frame(sessions, windows, edit_frames, *current_window, *term_rows, *term_cols);
                compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
            }
            // `f`: applies the selected item's own fix, only while its
            // detail (and so the `[f] autofix` hint) is actually showing
            // -- matches vim's own "an action only fires once its own
            // hint is visible" convention nothing else in this codebase
            // has yet, but is the obvious reading of "space shows...
            // extra actions" from this feature's own spec. Re-diagnoses
            // from scratch afterward (see apply_fix's own doc comment on
            // why) and keeps the pane expanded, so applying several
            // fixes in a row doesn't require re-entering each time.
            Key::Char('f') if expanded == Some(selected) => {
                let applied = edit_frames.get_mut(&edit_frame_id).map(|session| {
                    let Some(d) = session.buffer.diagnostics.get(selected).cloned() else { return false };
                    if !fileeditor::apply_fix(&mut session.buffer, &d) {
                        return false;
                    }
                    session.buffer.diagnostics = fileeditor::diagnose_buffer(&session.buffer);
                    true
                });
                if applied == Some(true) {
                    let diagnostics_len = edit_frames.get(&edit_frame_id).map(|s| s.buffer.diagnostics.len()).unwrap_or(0);
                    selected = selected.min(diagnostics_len.saturating_sub(1));
                    expanded = None;
                    set_expanded(windows, *current_window, diagnostics_pane_rows(diagnostics_len, false, budget));
                    refresh_editor_frame(sessions, windows, edit_frames, *current_window, *term_rows, *term_cols);
                    compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                }
            }
            _ => match vk.feed(key) {
                KeyOutcome::Motion(motion::Motion::Down, count) => {
                    let diagnostics_len = edit_frames.get(&edit_frame_id).map(|s| s.buffer.diagnostics.len()).unwrap_or(0);
                    selected = (selected + count.unwrap_or(1).max(1)).min(diagnostics_len.saturating_sub(1));
                    expanded = None;
                    compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                }
                KeyOutcome::Motion(motion::Motion::Up, count) => {
                    selected = selected.saturating_sub(count.unwrap_or(1).max(1));
                    expanded = None;
                    compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                }
                KeyOutcome::Window(cmd, count) => {
                    dispatch_window_cmd(cmd, count, sessions, windows, current_window, next_session_id, next_window_id, sinks_are_grid, *term_rows, *term_cols);
                    return;
                }
                _ => {}
            },
        }
    }
}

// `:browse [path]`'s own driving loop -- the terminal half of the split
// browser.rs's module doc comment describes: that module owns the
// listing model, the grid arithmetic and the rendering (all testable
// without a terminal); this owns raw mode, keystrokes, and the same
// `service_background_jobs` idle callback every other blocking loop in
// this file already threads through, so background jobs keep draining
// and a resize is still noticed within one poll tick while the browser
// is what's blocking on input.
//
// Draws into whichever pane is focused, recomputing that pane's rect
// every iteration (not once up front) for exactly the reason
// run_diagnostics_frame does the same: a resize can land between two
// keystrokes, and the very next frame has to be addressed against the
// new rect rather than the old one.
//
// Restoring the pane's original content on the way out is deliberately
// *not* this function's job -- every caller of command mode already
// repaints the pane it was driving once an outcome comes back (see
// run_normal_mode_navigation's own `CommandModeOutcome::Ran` arm, which
// can't rely on `compositor_redraw` alone for a `Frame::Edit` pane), so
// adding a redraw here would just paint the same rows twice.
#[allow(clippy::too_many_arguments)]
fn run_browse_frame(
    start: &std::path::Path,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: usize,
    job_frames: &mut HashMap<JobFrameId, exec::FgJob>,
    term_rows: &mut usize,
    term_cols: &mut usize,
    sinks_are_grid: bool,
) -> Result<Option<Vec<std::path::PathBuf>>, String> {
    // Opened before raw mode is touched, so an unreadable path is an
    // ordinary command-mode error at the colon line rather than a
    // flicker into a browser that immediately backs out again.
    let mut browser = browser::Browser::open(start)?;
    // Same reasoning run_diagnostics_frame's own guard has: whatever
    // loop ran last restored the terminal to cooked mode on its way
    // out, which is unusable for a key-at-a-time grid.
    let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else {
        return Err("not a terminal".to_string());
    };
    let pane_id = windows[current_window].focused_pane;

    let outcome = loop {
        let rect = pane_rect(&windows[current_window], pane_id, *term_rows, *term_cols);
        print!("{}", browser.render(rect, *term_rows, *term_cols));
        let _ = io::stdout().flush();

        let key = match editor::read_key_idle(&mut || {
            service_background_jobs(sessions, windows, job_frames, current_window, term_rows, term_cols, sinks_are_grid);
        }) {
            Ok(Some(k)) => k,
            // EOF/error: nothing chosen, same tolerance every other
            // loop's own EOF arm has.
            Ok(None) | Err(_) => break None,
        };

        match browser.handle_key(key, rect) {
            browser::Outcome::Continue => {}
            browser::Outcome::Cancelled => break None,
            browser::Outcome::Accepted(paths) => break Some(paths),
        }
    };

    // `render` hides the real cursor for every frame it draws (there's
    // no text insertion point in a grid), so put it back before handing
    // the pane to a caller that expects to place it itself.
    print!("\x1b[?25h");
    let _ = io::stdout().flush();
    Ok(outcome)
}

// Renders the diagnostics pane's own expanded list directly to the real
// terminal -- same "print straight to the terminal while this pane is
// the one actually focused/driven" model run_normal_mode_navigation's
// own render_normal_mode_frame uses (see Frame's doc comment on why
// only the focused pane ever draws live). Hides the real cursor
// (`\x1b[?25l`) rather than parking it somewhere arbitrary -- unlike
// every other interactive pane in this codebase, there's no text
// insertion point here for a blinking cursor to usefully mark.
fn render_diagnostics_list_frame(buf: &TextBuffer, rect: Rect, selected: usize, expanded: Option<usize>) {
    fn pad(text: &str, cols: usize) -> String {
        let mut s: String = text.chars().take(cols).collect();
        let len = s.chars().count();
        if len < cols {
            s.push_str(&" ".repeat(cols - len));
        }
        s
    }
    let mut out = String::new();
    let mut row = 0usize;
    if buf.diagnostics.is_empty() {
        out.push_str(&format!("\x1b[{};{}H", rect.row + 1, rect.col + 1));
        out.push_str(&pad("No problems found", rect.cols));
        row = 1;
    } else {
        for (i, d) in buf.diagnostics.iter().enumerate() {
            if row >= rect.rows {
                break;
            }
            let (line, col) = fileeditor::diagnostic_position(buf, d.start);
            let text = pad(&format!("{}:{}  {}", line + 1, col + 1, d.message), rect.cols);
            out.push_str(&format!("\x1b[{};{}H", rect.row + row + 1, rect.col + 1));
            if i == selected {
                out.push_str("\x1b[7m");
                out.push_str(&text);
                out.push_str("\x1b[0m");
            } else {
                out.push_str(&text);
            }
            row += 1;
            if expanded == Some(i) {
                if row < rect.rows {
                    out.push_str(&format!("\x1b[{};{}H", rect.row + row + 1, rect.col + 1));
                    out.push_str(&pad(&format!("  [{}] {}", d.code, d.message), rect.cols));
                    row += 1;
                }
                if row < rect.rows {
                    let hint = if d.fix.is_some() { "  [space] collapse   [f] autofix" } else { "  [space] collapse" };
                    out.push_str(&format!("\x1b[{};{}H", rect.row + row + 1, rect.col + 1));
                    out.push_str(&pad(hint, rect.cols));
                    row += 1;
                }
            }
        }
    }
    while row < rect.rows {
        out.push_str(&format!("\x1b[{};{}H", rect.row + row + 1, rect.col + 1));
        out.push_str(&" ".repeat(rect.cols));
        row += 1;
    }
    out.push_str("\x1b[?25l");
    print!("{}", out);
    let _ = io::stdout().flush();
}

// The `Frame::DebugRun` sibling's own idle view -- reached whenever the
// user navigates focus onto it directly (`<C-w>j`, or clicking its own
// collapsed title bar) while nothing is actually running (a real run
// drives the terminal itself, from inside `PauseState::on_statement`/
// repl.rs's own "dbg run"/"continue"/"next"/"step" handling -- see
// debugger.rs's own top-of-file doc comment for why that can't go
// through this function instead). There's nothing to select/expand here
// the way `run_diagnostics_frame` has -- just a status line and
// `<C-w>`/Escape/`q` to leave, so this is much smaller than that
// function despite the shared shape.
#[allow(clippy::too_many_arguments)]
fn run_debug_run_frame(
    pane_id: PaneId,
    edit_frame_id: EditFrameId,
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut Vec<WindowEntry>,
    current_window: &mut usize,
    next_session_id: &mut SessionId,
    next_window_id: &mut u32,
    debug_frames: &HashMap<EditFrameId, debugger::DebugSession>,
    sinks_are_grid: &mut bool,
    term_rows: &mut usize,
    term_cols: &mut usize,
) {
    let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else { return };
    let mut vk = VimKeys::new();

    let leave = |windows: &mut Vec<WindowEntry>| {
        if let Some(editor_pane) = editor_pane_for(&windows[*current_window], edit_frame_id) {
            windows[*current_window].focused_pane = editor_pane;
        }
    };

    // Same reasoning run_diagnostics_frame's own identical call has:
    // whichever pane this one's *sibling* is (the editor pane) has never
    // had this pane's own rect painted around it before now -- without
    // this, only the rows this loop's own writes below touch would ever
    // update, leaving the editor pane's last content sitting there at
    // its *previous* (unshrunk) size until something else happens to
    // trigger a full repaint.
    compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);

    loop {
        let rect = pane_rect(&windows[*current_window], pane_id, *term_rows, *term_cols);
        let status = if debug_frames.contains_key(&edit_frame_id) {
            "dbg: attached -- :dbg run to start  <C-w> to leave"
        } else {
            "dbg: not attached -- :dbg to attach"
        };
        let mut out = format!("\x1b[{};{}H\x1b[K", rect.row + 1, rect.col + 1);
        out.push_str(&status.chars().take(rect.cols).collect::<String>());
        for row in 1..rect.rows {
            out.push_str(&format!("\x1b[{};{}H\x1b[K", rect.row + row + 1, rect.col + 1));
        }
        out.push_str("\x1b[?25l");
        print!("{out}");
        let _ = io::stdout().flush();

        let key = match editor::read_key_idle(&mut || {}) {
            Ok(Some(k)) => k,
            Ok(None) | Err(_) => return,
        };
        match key {
            Key::Escape | Key::Char('q') => {
                leave(windows);
                compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                return;
            }
            _ => {
                if let KeyOutcome::Window(cmd, count) = vk.feed(key) {
                    dispatch_window_cmd(cmd, count, sessions, windows, current_window, next_session_id, next_window_id, sinks_are_grid, *term_rows, *term_cols);
                    return;
                }
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
                // Same reasoning again -- a diagnostics pane is scoped
                // to the one Edit frame's own pane it sits below, not a
                // session that could sensibly show up somewhere else.
                Some(Frame::Diagnostics(_)) => {
                    sessions[&cur_sid].shell.sink_err("bish: window: fg: that window is showing diagnostics, not a session\n");
                }
                // Same reasoning again -- the debug-run pane is scoped to
                // the one Edit frame's own pane it sits below too.
                Some(Frame::DebugRun(_)) => {
                    sessions[&cur_sid].shell.sink_err("bish: window: fg: that window is running a debugged script, not a session\n");
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
    window.layout = insert_sibling(old_layout, focused_id, new_pane_id, horizontal, None, false);

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

// `:diag`'s own sibling to split_focused_pane, just above -- same
// "fork a session purely to give the new pane a screen to composite
// from" pattern (see Frame's own doc comment on why every pane needs
// one), but the new pane is never a usable shell: `Frame::Diagnostics
// (edit_frame_id)` goes straight on top of its `Frame::Session`, it
// starts `minimized` (see SplitChild's own doc comment), and -- unlike
// an ordinary `<C-w>s`/`<C-w>v` split -- focus stays right where it was
// (running `:diag` shouldn't yank you out of the file you're editing).
// Always a horizontal split (the pane goes at the bottom, per the
// feature's own name); `focused_id` is the *editor's* own pane, i.e.
// whichever pane is currently focused when `:diag` runs (it's the one
// driving this command in the first place).
fn split_diagnostics_pane(
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut [WindowEntry],
    current_window: usize,
    next_session_id: &mut SessionId,
    edit_frame_id: EditFrameId,
    term_rows: usize,
    term_cols: usize,
) -> PaneId {
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
    window.panes.push(Pane { id: new_pane_id, stack: vec![Frame::Session(sid), Frame::Diagnostics(edit_frame_id)] });

    let focused_id = window.focused_pane;
    let old_layout = std::mem::replace(&mut window.layout, PaneLayout::Leaf(0));
    window.layout = insert_sibling(old_layout, focused_id, new_pane_id, true, None, true);

    new_pane_id
}

// The diagnostics sibling pane already sitting below `edit_frame_id`'s
// own editor pane, if `:diag` has created one -- `None` before the
// first `:diag` run, or after `:diag clear`/the file closing removed
// it.
fn diagnostics_sibling(window: &WindowEntry, edit_frame_id: EditFrameId) -> Option<PaneId> {
    window.panes.iter().find(|p| p.stack.last() == Some(&Frame::Diagnostics(edit_frame_id))).map(|p| p.id)
}

// `:dbg`'s own sibling to split_diagnostics_pane, just above -- same
// "fork a session purely to give the new pane a screen to composite
// from" pattern, same "always horizontal, starts minimized, doesn't
// move focus" shape (attaching a debug session shouldn't yank you out
// of the file either). The new pane's own `SessionState.shell` is,
// like diagnostics' own, never actually used to run anything -- it
// exists only so this pane has a `Frame::Session` underneath
// `DebugRun` to sit on, and a screen to composite from. The *debugged
// script's own* Shell lives separately, in the `debugger::DebugSession`
// this frame's own `EditFrameId` indexes into (`debug_frames`), set to
// render into this exact same screen (`Shell::set_sink_grid`) so the
// script's real output actually shows up in this pane the same way any
// ordinary session's own live output does -- no bespoke ANSI-building.
fn split_debug_run_pane(
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut [WindowEntry],
    current_window: usize,
    next_session_id: &mut SessionId,
    edit_frame_id: EditFrameId,
    term_rows: usize,
    term_cols: usize,
) -> (PaneId, Rc<RefCell<vt100::Screen>>) {
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
            history: child_history,
            screen: screen.clone(),
            warned_stopped_jobs: false,
            dir_history: vec![child_cwd],
            dir_history_index: 0,
            command_transcript: Vec::new(),
        },
    );

    let window = &mut windows[current_window];
    let new_pane_id = window.next_pane_id;
    window.next_pane_id += 1;
    window.panes.push(Pane { id: new_pane_id, stack: vec![Frame::Session(sid), Frame::DebugRun(edit_frame_id)] });

    let focused_id = window.focused_pane;
    let old_layout = std::mem::replace(&mut window.layout, PaneLayout::Leaf(0));
    window.layout = insert_sibling(old_layout, focused_id, new_pane_id, true, None, true);

    (new_pane_id, screen)
}

// The debug-run sibling pane already sitting below `edit_frame_id`'s
// own editor pane, if `:dbg` has attached a session -- `None` before
// the first `:dbg` run, or after `:dbg quit`/the file closing removed
// it.
fn debug_run_sibling(window: &WindowEntry, edit_frame_id: EditFrameId) -> Option<PaneId> {
    window.panes.iter().find(|p| p.stack.last() == Some(&Frame::DebugRun(edit_frame_id))).map(|p| p.id)
}

// The debug-run pane's own 1-row collapsed title, same "dashes + reverse-
// video pill + dashes" convention render_diagnostics_title uses -- shown
// while idle (not actually running/paused, which draws directly instead,
// see PauseState::render's own doc comment).
fn render_debug_run_title(screen: &Rc<RefCell<vt100::Screen>>, cols: usize, status: &str) {
    let pill: String = format!(" dbg: {status} ").chars().take(cols).collect();
    let pill_len = pill.chars().count();
    let left = 2.min(cols.saturating_sub(pill_len));
    let right = cols.saturating_sub(pill_len + left);
    let mut framed = String::from("\r\x1b[K");
    framed.push_str(&"─".repeat(left));
    framed.push_str("\x1b[7m");
    framed.push_str(&pill);
    framed.push_str("\x1b[0m");
    framed.push_str(&"─".repeat(right));
    screen.borrow_mut().feed(framed.as_bytes());
}

// The diagnostics pane's own 1-row title, in reverse video (matching
// the tab bar's own convention, tab_bar_line) -- fed directly into its
// session's grid via feed(), the same mechanism freeze_editor_frame/
// freeze_idle_prompt already use to bake static content into a pane's
// grid outside of anything actually being "typed". Left-aligned; the
// rest of the row is left blank (reverse video still fills it visually
// once composited, same as how render_row already pads a styled run).
// The whole point of `minimized` (see SplitChild's own doc comment):
// this pane's one row *is* the divider between it and the editor pane
// above it, styled to read as one -- a short reverse-video "pill"
// holding the title, set into an otherwise ordinary dashed divider
// line, not a full-width bar filling the entire row (that read as its
// own separate content line sitting *below* a real, plain divider,
// exactly the "title shown as a separate line" this replaces).
fn render_diagnostics_title(screen: &Rc<RefCell<vt100::Screen>>, cols: usize, diagnostics: &[lint::Diagnostic]) {
    let text = match diagnostics.len() {
        0 => "No problems found".to_string(),
        1 => "1 problem found".to_string(),
        n => format!("{n} problems found"),
    };
    let pill: String = format!(" {text} ").chars().take(cols).collect();
    let pill_len = pill.chars().count();
    let left = 2.min(cols.saturating_sub(pill_len));
    let right = cols.saturating_sub(pill_len + left);
    let mut framed = String::from("\r\x1b[K");
    framed.push_str(&"─".repeat(left));
    framed.push_str("\x1b[7m");
    framed.push_str(&pill);
    framed.push_str("\x1b[0m");
    framed.push_str(&"─".repeat(right));
    screen.borrow_mut().feed(framed.as_bytes());
}

// Creates the diagnostics sibling if `:diag` hasn't already (via
// split_diagnostics_pane), then (re)writes its collapsed title and
// repaints -- called by `:diag`'s own handler right after it recomputes
// `tb.diagnostics`, while that buffer is still this function's own
// caller's local (see this module's own doc comment on why that's the
// only time this pane's content can be synced at all).
#[allow(clippy::too_many_arguments)]
fn sync_diagnostics_pane(
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut [WindowEntry],
    current_window: usize,
    next_session_id: &mut SessionId,
    edit_frame_id: EditFrameId,
    diagnostics: &[lint::Diagnostic],
    term_rows: usize,
    term_cols: usize,
) {
    let pane_id = diagnostics_sibling(&windows[current_window], edit_frame_id)
        .unwrap_or_else(|| split_diagnostics_pane(sessions, windows, current_window, next_session_id, edit_frame_id, term_rows, term_cols));
    let rect = pane_rect(&windows[current_window], pane_id, term_rows, term_cols);
    let sid = windows[current_window].pane(pane_id).owning_session();
    render_diagnostics_title(&sessions[&sid].screen, rect.cols, diagnostics);
    compositor_redraw(sessions, windows, current_window, term_rows, term_cols);
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
fn insert_sibling(layout: PaneLayout, target: PaneId, new_id: PaneId, horizontal: bool, new_fixed: Option<usize>, new_minimized: bool) -> PaneLayout {
    match layout {
        PaneLayout::Leaf(id) if id == target => PaneLayout::Split {
            horizontal,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(id), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(new_id), weight: 1.0, fixed: new_fixed, minimized: new_minimized },
            ],
        },
        PaneLayout::Leaf(id) => PaneLayout::Leaf(id),
        PaneLayout::Split { horizontal: h, children } => {
            let direct_child_idx = children.iter().position(|c| matches!(&c.layout, PaneLayout::Leaf(id) if *id == target));
            if let Some(idx) = direct_child_idx {
                if h == horizontal {
                    let mut children = children;
                    children.insert(idx + 1, SplitChild { layout: PaneLayout::Leaf(new_id), weight: 1.0, fixed: new_fixed, minimized: new_minimized });
                    return PaneLayout::Split { horizontal: h, children };
                }
            }
            let children = children
                .into_iter()
                .map(|c| SplitChild { layout: insert_sibling(c.layout, target, new_id, horizontal, new_fixed, new_minimized), weight: c.weight, fixed: c.fixed, minimized: c.minimized })
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
                .filter_map(|c| remove_from_layout(c.layout, target).map(|layout| SplitChild { layout, weight: c.weight, fixed: c.fixed, minimized: c.minimized }))
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
    close_pane(window, window.focused_pane);
}

// Generalizes close_focused_pane to any pane in the window, not just the
// focused one -- used to close a diagnostics sibling (never focused at
// the moment its own Edit frame quits, see run_edit_frame's NavExit::
// Quit arm) alongside close_focused_pane's own original call site. Only
// reassigns `focused_pane` when `pane_id` actually was the focused one;
// closing some *other* pane leaves focus exactly where it was.
fn close_pane(window: &mut WindowEntry, pane_id: PaneId) {
    let old_layout = std::mem::replace(&mut window.layout, PaneLayout::Leaf(0));
    window.layout = remove_from_layout(old_layout, pane_id).expect("closing one of >1 panes always leaves at least one behind");
    window.panes.retain(|p| p.id != pane_id);
    if window.focused_pane == pane_id {
        window.focused_pane = first_leaf(&window.layout);
    }
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
                Frame::Job(_) | Frame::Edit(_) | Frame::Diagnostics(_) | Frame::DebugRun(_) => None,
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
    // A qualifying left click (MouseEvent::is_left_click) arrived at
    // bish's own stdin instead of being forwarded to the job -- see
    // decode_fg_click's own doc comment for why this one gesture gets
    // intercepted while every other byte (including every other kind of
    // mouse event) still just goes straight through. The caller
    // (run_fg_job_frame) hit-tests it: a miss, or a hit within the
    // job's own pane, means it was actually meant for the job after
    // all, and gets forwarded there before resuming; a hit elsewhere
    // (the tab bar, a different pane) switches focus there instead,
    // leaving the job running in the background -- the same outcome
    // Ctrl+Space's own Detached already produces, just reached by a
    // click instead of a keystroke.
    MouseClick(editor::MouseEvent),
    // This job's own screen (the same Rc<RefCell<vt100::Screen>> as
    // `screen`, drive_fg_job's own param) changed size mid-drive --
    // service_background_jobs's own on_idle-driven WINCH handling
    // (poll_and_apply_resize) already applied the resize everywhere else
    // (every session's screen, every *background* job's pty) by the time
    // this is returned; drive_fg_job already caught this one job's own
    // pty up to match too (see its own doc comment), so all that's left
    // for the caller is to stop driving with whatever pane-rects/tab-bar
    // snapshot it took before this loop started (now stale) and take a
    // fresh one before resuming -- see run_fg_job_frame's own handling.
    Resized,
}

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
    print!("{}", if wants { term::MOUSE_REPORTING_ENABLE } else { term::MOUSE_REPORTING_DISABLE });
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

// Same idea again, for this job's own pty size -- keeps it matching
// `screen`'s own size (the source of truth: repl.rs always keeps a
// session's screen resized to exactly its own on-screen area) instead
// of polling SIGWINCH itself. `screen` only ever changes size from
// *outside* drive_fg_job (service_background_jobs's own on_idle-driven
// poll_and_apply_resize, reached via drive_fg_job's own on_idle
// argument) -- this just needs to notice that already happened and
// catch this one job's own pty up to match, the same way a real
// terminal multiplexer propagates a resize straight through to whatever
// program is running in the pane you're looking at. Returns whether it
// changed, so the caller can stop driving with now-stale chrome
// geometry built around the old size (see FgOutcome::Resized's own doc
// comment) instead of silently continuing.
fn sync_pty_size(last: &mut (u16, u16), job: &mut exec::FgJob, screen: &Rc<RefCell<vt100::Screen>>) -> bool {
    use std::os::unix::io::AsRawFd;
    let (rows, cols) = screen.borrow().size();
    let (rows, cols) = (rows as u16, cols as u16);
    if (rows, cols) == *last {
        return false;
    }
    let _ = pty::set_size(job.pty_master().as_raw_fd(), rows, cols);
    *last = (rows, cols);
    true
}

// A complete SGR mouse report ("\x1b[<Cb;Cx;CyM/m") decoded from the
// *first* such report at the start of `seq`, but only when it's a
// qualifying left click (see MouseEvent::is_left_click) -- anything
// else (incomplete, malformed, a release/drag/wheel/other button) is
// `None`, meaning "forward this unmodified, exactly like every other
// byte sequence" (drive_fg_job's own call site). Deliberately scans for
// the first 'M'/'m' rather than trusting `seq`'s own last byte: a press
// and its paired release (or several clicks in quick succession) can
// easily arrive in the very same read as one another over a local pty,
// and treating the *whole* buffer as one report would garble the params
// and silently fail to recognize a genuine click. Pure and separate
// from drive_fg_job's own loop so the decoding itself has unit test
// coverage without a real pty, mirroring editor.rs's own
// decode_sgr_mouse_final split from read_sgr_mouse.
fn decode_fg_click(seq: &[u8]) -> Option<editor::MouseEvent> {
    if !seq.starts_with(b"\x1b[<") {
        return None;
    }
    let end = seq[3..].iter().position(|b| *b == b'M' || *b == b'm')? + 3;
    let final_byte = seq[end];
    let params = std::str::from_utf8(&seq[3..end]).ok()?;
    match editor::decode_sgr_mouse_final(params, final_byte) {
        Key::Mouse(ev) if ev.is_left_click() => Some(ev),
        _ => None,
    }
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
    let mut pty_size = { let (r, c) = screen.borrow().size(); (r as u16, c as u16) };

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
        // Checked every iteration (cheap: an Rc<RefCell> borrow plus a
        // comparison, no syscall unless it actually changed) rather than
        // only in the idle branch below -- a firehose job can keep this
        // loop busy draining output for a while without ever reaching
        // that branch, and this job's own pty should still catch up
        // promptly regardless. See sync_pty_size's own doc comment for
        // why this doesn't need its own SIGWINCH poll.
        if sync_pty_size(&mut pty_size, job, screen) {
            break FgOutcome::Resized;
        }

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
                // An SGR mouse report ("\x1b[<Cb;Cx;CyM/m") -- wait for
                // the rest if it hasn't all arrived in this same read
                // yet (same reasoning as the lone-ESC wait just above),
                // then decode it before deciding whether to forward: a
                // qualifying left click (decode_fg_click's own doc
                // comment) might be aimed at bish's own UI chrome (the
                // tab bar, a different pane) rather than this job, and
                // gets handed back to the caller instead of forwarded.
                // Everything else here (a release, drag, wheel, right/
                // middle click, or the sequence just never completing)
                // falls straight through to the ordinary forward below,
                // unmodified.
                if seq.starts_with(b"\x1b[<") {
                    // Whether the *first* report has terminated yet --
                    // not just seq's own last byte, which a
                    // fast-arriving paired release (or several clicks in
                    // a row) could easily make M/m without the first
                    // report actually being complete (decode_fg_click's
                    // own doc comment).
                    while !seq[3..].iter().any(|b| *b == b'M' || *b == b'm') && term::stdin_ready(50) {
                        let more = unsafe { raw_read(0, buf.as_mut_ptr(), buf.len()) };
                        if more <= 0 {
                            break;
                        }
                        seq.extend_from_slice(&buf[..more as usize]);
                    }
                    if let Some(ev) = decode_fg_click(&seq) {
                        break FgOutcome::MouseClick(ev);
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
        print!("{}", term::MOUSE_REPORTING_DISABLE);
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
// Returns true iff a session-daemon client just (re)attached this call
// -- see the `just_attached` handling below for what that triggers here
// (a full compositor_redraw), and its own doc comment for why that
// alone isn't enough for a caller currently driving a grid-bypassing
// view (a `Frame::Edit`'s own real content, see run_normal_mode_
// navigation's own on_idle closure for the caller that needs to react
// to this return value; every other caller correctly ignores it).
fn service_background_jobs(
    sessions: &mut HashMap<SessionId, SessionState>,
    windows: &mut [WindowEntry],
    job_frames: &mut HashMap<JobFrameId, exec::FgJob>,
    skip_window: usize,
    term_rows: &mut usize,
    term_cols: &mut usize,
    sinks_are_grid: bool,
) -> bool {
    use std::io::Read;

    // A plain no-op unless this process is running as a `bish session`
    // daemon (session::install_bridge was called) -- see session.rs's
    // own module doc comment for why this one call, here, is the whole
    // wiring needed rather than a new parameter threaded through every
    // caller between here and repl::run.
    session::service_current_bridge();
    // A client just (re)connected -- force one full repaint (not the
    // ordinary incremental diff) so its own blank real terminal starts
    // correctly caught up, rather than staying empty until unrelated
    // activity happens to redraw something. See take_bridge_just_
    // attached's own doc comment. `compositor_redraw` alone is *not*
    // sufficient when the currently-focused pane is a `Frame::Edit`
    // being actively driven -- its real content never feeds the
    // session's own vt100 grid while that's happening (the exact same
    // gap this codebase already hit once before, for command mode's own
    // post-return redraw -- see this file's own history), so
    // `compositor_redraw` alone would just paint that pane blank. The
    // `bool` returned here is what lets such a caller notice and follow
    // up with its own direct redraw of the real content it alone has
    // access to.
    let just_attached = session::take_bridge_just_attached();
    if just_attached && sinks_are_grid {
        compositor_redraw(sessions, windows, skip_window, *term_rows, *term_cols);
    }
    // The new client's own TERM/COLORTERM -- applied to *every*
    // session's own remembered environment (Shell::
    // set_terminal_capability_env), not just the currently-focused one:
    // there's only one real attached client for the whole daemon, so a
    // window that isn't focused right now still needs to see the
    // update whenever it's focused later. Applying this directly via
    // `std::env::set_var` here wouldn't stick -- see that method's own
    // doc comment for why it has to go through each session's env_
    // snapshot instead.
    if let Some((term, colorterm)) = session::take_pending_capability() {
        for session in sessions.values_mut() {
            session.shell.set_terminal_capability_env(&term, &colorterm);
        }
    }

    poll_and_apply_resize(&*sessions, &*windows, job_frames, term_rows, term_cols, sinks_are_grid, skip_window);

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
    just_attached
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

// Whether a divider line is drawn between `children[i]` and
// `children[i + 1]` -- every adjacent pair, except where either side is
// `minimized` (see SplitChild's own doc comment): a minimized pane's
// own single row already reads as the boundary between it and its
// neighbor, so a further, separate divider line right next to it would
// just be a redundant blank line. Shared by split_sizes (to know how
// much of the axis dividers actually consume) and compute_regions (to
// know where to actually draw one).
fn dividers_after(children: &[SplitChild]) -> Vec<bool> {
    (0..children.len().saturating_sub(1)).map(|i| !(children[i].minimized || children[i + 1].minimized)).collect()
}

// One Split's own children resolved down to how many cells each gets
// along the split's own axis (`usable` -- the area already minus every
// *actually drawn* divider strip, see dividers_after). A `minimized`
// child always claims exactly one cell, first, regardless of `fixed`/
// `weight` (both irrelevant while minimized). Of what's left, every
// `fixed`-child (see SplitChild's own doc comment) claims its own
// requested count next, clamped so it can never squeeze a plain
// `fixed: None` sibling below 1 cell; whatever's left after *that* then
// divides among those weighted siblings exactly the way this used to
// work before `fixed`/`minimized` existed -- proportional to weight/
// (sum of weighted siblings' weights), with the *last* weighted child
// (not necessarily the last child overall) absorbing the rounding
// remainder so the weighted children's own total always adds up
// exactly. With no `fixed`/`minimized` children at all (every existing
// call site, before this pane type), `weighted_count == children.len()`
// and this is byte-for-byte the original formula.
fn split_sizes(children: &[SplitChild], usable: usize) -> Vec<usize> {
    let mut sizes = vec![0usize; children.len()];
    let mut budget = usable;
    for (i, child) in children.iter().enumerate() {
        if child.minimized {
            let size = 1.min(budget);
            sizes[i] = size;
            budget = budget.saturating_sub(size);
        }
    }
    let weighted_count = children.iter().filter(|c| !c.minimized && c.fixed.is_none()).count();
    for (i, child) in children.iter().enumerate() {
        if let (false, Some(f)) = (child.minimized, child.fixed) {
            let reserve_for_weighted = weighted_count.min(budget);
            let size = f.min(budget.saturating_sub(reserve_for_weighted)).max(1.min(budget));
            sizes[i] = size;
            budget = budget.saturating_sub(size);
        }
    }
    let total_weight: f64 = children.iter().filter(|c| !c.minimized && c.fixed.is_none()).map(|c| c.weight.max(MIN_PANE_WEIGHT)).sum();
    let mut allocated = 0usize;
    let mut seen = 0usize;
    for (i, child) in children.iter().enumerate() {
        if child.minimized || child.fixed.is_some() {
            continue;
        }
        seen += 1;
        let h = if seen == weighted_count {
            budget.saturating_sub(allocated).max(1)
        } else {
            (((budget as f64) * child.weight.max(MIN_PANE_WEIGHT) / total_weight).round() as usize).max(1)
        };
        sizes[i] = h;
        allocated += h;
    }
    sizes
}

// Walks `layout`, splitting `area` among each Split's children (see
// split_sizes, above, for how much each one gets) down to each Leaf's
// own rectangle. `dividers` collects the reserved divider strips
// separately (row=true for a horizontal divider line, running
// left-right; false for a vertical one, running top-bottom) so the
// caller can draw them after every pane's own content, rather than each
// Split trying to draw into space a child might otherwise want --
// skipped entirely between a minimized child and its neighbor (see
// dividers_after).
fn compute_regions(layout: &PaneLayout, area: Rect, out: &mut Vec<(PaneId, Rect)>, dividers: &mut Vec<(Rect, bool)>) {
    match layout {
        PaneLayout::Leaf(id) => out.push((*id, area)),
        PaneLayout::Split { horizontal, children } => {
            let n = children.len();
            if n == 0 {
                return;
            }
            let draws_divider = dividers_after(children);
            let divider_count = draws_divider.iter().filter(|d| **d).count();
            if *horizontal {
                // Panes stacked top/bottom; the divider is the horizontal
                // line between them.
                let usable = area.rows.saturating_sub(divider_count);
                let sizes = split_sizes(children, usable);
                let mut row = area.row;
                for (i, child) in children.iter().enumerate() {
                    let h = sizes[i];
                    compute_regions(&child.layout, Rect { row, col: area.col, rows: h, cols: area.cols }, out, dividers);
                    row += h;
                    if i + 1 < n && draws_divider[i] {
                        dividers.push((Rect { row, col: area.col, rows: 1, cols: area.cols }, true));
                        row += 1;
                    }
                }
            } else {
                // Panes side by side; the divider is the vertical line
                // between them.
                let usable = area.cols.saturating_sub(divider_count);
                let sizes = split_sizes(children, usable);
                let mut col = area.col;
                for (i, child) in children.iter().enumerate() {
                    let w = sizes[i];
                    compute_regions(&child.layout, Rect { row: area.row, col, rows: area.rows, cols: w }, out, dividers);
                    col += w;
                    if i + 1 < n && draws_divider[i] {
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
//
// Deliberately no leading `\x1b[2J` (erase-whole-display): compute_regions
// already tiles every pane plus every divider across the entire content
// area with no gaps, render_row always emits exactly `pane.rect.cols`
// characters per row regardless of what those cells actually hold (a
// blank cell still writes a space), and the tab bar's own row gets an
// explicit `\x1b[K` first -- between those three, every single cell of
// the real terminal is already unconditionally overwritten every call,
// making a separate whole-screen erase pure redundant work. It used to
// be here, and was the actual cause of a reported flash on ordinary
// discrete redraws (e.g. Ctrl-C at the live prompt): an explicit erase
// followed immediately by repainting the exact same content one real
// terminal can still render as a visible blank frame in between, even
// though nothing (or almost nothing) about the actual content changed.
fn render_compositor_frame(layout: &CompositorLayout, tab_bar: &str, term_rows: usize) {
    print!("{}", build_compositor_frame_output(layout, tab_bar, term_rows));
    let _ = io::stdout().flush();
}

// render_compositor_frame's own string-building half, split out purely
// for testability -- same "pure builder plus a thin print wrapper"
// shape diff_frames/render_compositor_frame_diff already established.
fn build_compositor_frame_output(layout: &CompositorLayout, tab_bar: &str, term_rows: usize) -> String {
    let mut out = String::new();
    out.push_str("\x1b[H");

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

    out
}

// A cell-by-cell snapshot of exactly what's currently painted on the real
// terminal's compositor content area -- every pane's own screen content,
// flattened into one term_cols-wide grid, plus the tab bar text and
// cursor position/visibility. What render_compositor_frame_diff diffs a
// freshly-resolved CompositorLayout against, so only genuinely-changed
// cells get repainted -- unlike render_compositor_frame's own
// unconditional `\x1b[2J` clear+repaint. Deliberately does *not* capture
// divider glyphs (see render_compositor_frame_diff's own doc comment on
// why that's fine): only ever compared against another TerminalFrame
// built the same way, so a divider cell always reads as
// vt100::Cell::default() on both sides and never shows up as "changed."
struct TerminalFrame {
    rows: usize,
    cols: usize,
    cells: Vec<vt100::Cell>,
    tab_bar: String,
    cursor: (usize, usize),
    cursor_visible: bool,
}

impl TerminalFrame {
    // Flattens every pane's own screen content into one rows*cols grid --
    // the same data render_compositor_frame's own per-pane render_row
    // loop paints, just captured as data instead of escape codes.
    //
    // `pane.rect` can be stale relative to `pane.screen`'s own *current*
    // size by the time this runs: render_compositor_frame_diff's caller
    // (run_fg_job_frame) only re-snapshots its whole `layout` -- rects
    // included -- when `sync_pty_size` notices the *focused* pane's own
    // screen size changed, but a background WINCH tick's own
    // poll_and_apply_resize/compositor_redraw pass can resize a
    // *sibling* pane's screen independently (e.g. a fixed-size split
    // child, or simply a different rounding remainder) without the
    // focused one's size moving at all -- leaving this function's own
    // `rect` for that sibling describing a larger area than its screen
    // now actually has. Clamping each pane's own read range to its
    // screen's own live size (not just `pane.rect`) is what avoids
    // reading past it -- this was a real, reproducible crash (`Screen::
    // cell`'s own index-out-of-bounds panic) before this clamp existed.
    fn capture(layout: &CompositorLayout, tab_bar: &str, rows: usize, cols: usize) -> TerminalFrame {
        let mut cells = vec![vt100::Cell::default(); rows * cols];
        for pane in &layout.panes {
            let screen = pane.screen.borrow();
            let (screen_rows, screen_cols) = screen.size();
            for r in 0..pane.rect.rows.min(screen_rows) {
                for c in 0..pane.rect.cols.min(screen_cols) {
                    let (row, col) = (pane.rect.row + r, pane.rect.col + c);
                    if row < rows && col < cols {
                        cells[row * cols + col] = screen.cell(r, c);
                    }
                }
            }
        }
        let focused = layout.panes.iter().find(|p| p.focused).expect("exactly one pane is always focused");
        let screen = focused.screen.borrow();
        let (cur_row, cur_col) = screen.cursor();
        TerminalFrame {
            rows,
            cols,
            cells,
            tab_bar: tab_bar.to_string(),
            cursor: (focused.rect.row + cur_row, focused.rect.col + cur_col),
            cursor_visible: screen.cursor_visible,
        }
    }
}

// drive_fg_job's own hot redraw path (run_fg_job_frame's own redraw
// closure) -- unlike compositor_redraw/render_compositor_frame (every
// *discrete*-event redraw: window/pane switches, resizes, command-mode
// overlays, ...), this fires on every single batch of a foreground job's
// own pty output, often many times a second for a full-screen program (a
// status line, spinner, or cursor-move tick with no actual visible
// change). Always clearing and repainting the whole screen for that --
// what render_compositor_frame does -- is exactly what produced the
// reported flash (two Claude Code sessions, each in its own tab, each
// streaming near-continuous low-level output while merely idling):
// this instead diffs against `cache` (the previous call's own painted
// content, see TerminalFrame) and only touches cells that actually
// changed.
//
// `*cache == None` means "start fresh": either the very first redraw for
// this job, or the caller has explicitly invalidated it (run_fg_job_frame
// does this on FgOutcome::Resized, since the geometry a stale cache
// describes is no longer even the right shape) -- falls back to
// render_compositor_frame's own full clear+repaint for the same
// self-healing property that function's own doc comment describes
// (anything that wrote to the real terminal directly in between, like
// editor.rs's Ctrl-L, self-heals on the next full repaint), then seeds
// `cache` from the result so the *next* call can diff.
//
// Deliberately scoped to exactly this one redraw path: every other
// redraw in this codebase (compositor_redraw's own callers -- window/
// pane switches, resizes, command-mode overlays, the diagnostics pane,
// ...) stays a full repaint, unchanged. Those are infrequent discrete
// events, not a continuous loop, so there's nothing to gain and no
// reason to widen this cache's invalidation surface to cover every other
// function that ever writes to the real terminal directly
// (render_diagnostics_list_frame, render_normal_mode_frame, ...) -- this
// cache lives and dies entirely within one run_fg_job_frame call, and is
// never read by (or invalidated on behalf of) anything else.
fn render_compositor_frame_diff(layout: &CompositorLayout, tab_bar: &str, term_rows: usize, term_cols: usize, cache: &mut Option<TerminalFrame>) {
    let rows = content_rows(term_rows);
    let stale = match cache {
        Some(prev) => prev.rows != rows || prev.cols != term_cols,
        None => true,
    };
    if stale {
        render_compositor_frame(layout, tab_bar, term_rows);
        *cache = Some(TerminalFrame::capture(layout, tab_bar, rows, term_cols));
        return;
    }
    let prev = cache.as_ref().unwrap();
    let new_frame = TerminalFrame::capture(layout, tab_bar, rows, term_cols);
    let out = diff_frames(prev, &new_frame, term_rows, term_cols);
    if !out.is_empty() {
        print!("{}", out);
        let _ = io::stdout().flush();
    }
    *cache = Some(new_frame);
}

// The pure half of render_compositor_frame_diff: exactly the escape-code
// text to write to bring the real terminal from `prev`'s own painted
// state to `new`'s, assuming both describe the *same* `rows`/`term_cols`
// shape (render_compositor_frame_diff's own stale check is what
// guarantees that before this is ever called). Split out from the I/O
// wrapper purely so this -- the actual diffing decision -- is unit
// testable without a real terminal, matching this file's own existing
// split between logic (compute_regions/split_sizes/render_row, all pure)
// and thin print!-wrapping callers.
fn diff_frames(prev: &TerminalFrame, new: &TerminalFrame, term_rows: usize, term_cols: usize) -> String {
    let mut out = String::new();

    for row in 0..new.rows {
        let mut col = 0;
        while col < term_cols {
            let idx = row * term_cols + col;
            if new.cells[idx] == prev.cells[idx] {
                col += 1;
                continue;
            }
            // Extend the dirty run while cells keep differing, then paint
            // the whole run with one cursor move -- same style-coalescing
            // render_row itself uses, just scoped to this sub-span rather
            // than the whole row (a span can cross pane boundaries in a
            // side-by-side split; the flattened grid already makes that
            // transparent here).
            let run_start = col;
            while col < term_cols && new.cells[row * term_cols + col] != prev.cells[row * term_cols + col] {
                col += 1;
            }
            out.push_str(&format!("\x1b[{};{}H", row + 1, run_start + 1));
            let mut last_style: Option<(vt100::Color, vt100::Color, vt100::CellAttrs)> = None;
            for c in run_start..col {
                let cell = new.cells[row * term_cols + c];
                let key = (cell.fg, cell.bg, cell.attrs);
                if last_style != Some(key) {
                    out.push_str(&vt100::sgr_codes(cell.fg, cell.bg, cell.attrs));
                    last_style = Some(key);
                }
                out.push(cell.ch);
            }
        }
    }

    if new.tab_bar != prev.tab_bar {
        out.push_str(&format!("\x1b[{};1H\x1b[K", term_rows));
        out.push_str(&new.tab_bar);
    }

    // The cursor always needs re-asserting after painting anything above
    // (each write already moved it), and whenever its own logical
    // position/visibility changed even with zero cell writes (e.g. a job
    // just moved its cursor without changing any visible glyph).
    if !out.is_empty() || new.cursor != prev.cursor || new.cursor_visible != prev.cursor_visible {
        out.push_str(&format!("\x1b[{};{}H", new.cursor.0 + 1, new.cursor.1 + 1));
        out.push_str(if new.cursor_visible { "\x1b[?25h" } else { "\x1b[?25l" });
    }

    out
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

    // A real terminal grid, unlike a text file, can autowrap a line that
    // just ran out of columns -- see vt100::Grid's own `wrapped` field
    // doc comment. `raw_len`/`raw_char_at` already translate this
    // combined line index into either a scrollback row or a live grid
    // row the same way; this just reads the matching wrapped flag
    // instead of a cell.
    fn line_wraps(&self, line: usize) -> bool {
        let s = self.screen.borrow();
        let sb_len = addressable_scrollback_len(&s);
        if line < sb_len {
            s.scrollback_wrapped.get(line).copied().unwrap_or(false)
        } else {
            s.row_wraps(line - sb_len)
        }
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

    // Deliberately NOT gated on `TextBuffer::is_readonly` -- this is what
    // feeds `run_command_mode`'s own `editing` parameter, and `:dbg`
    // itself (a command-mode command) needs `&mut TextBuffer` access
    // regardless of whether content mutation is currently allowed, to
    // toggle `set_readonly`/`breakpoints` in the first place. Vim-motion
    // content mutation is gated separately, at each individual
    // KeyOutcome/raw-key arm below, via `as_writable_mut`.
    fn as_editable_mut(&mut self) -> Option<&mut TextBuffer> {
        match self {
            NavBuffer::ReadOnly(_) => None,
            NavBuffer::Editable(b) => Some(b),
        }
    }

    // True only for a genuinely mutable `Editable` buffer -- false both
    // for `ReadOnly` (matching what a bare `matches!(_, NavBuffer::
    // Editable(_))` already meant) and for an `Editable` buffer currently
    // under `:dbg` control (`TextBuffer::is_readonly` -- see that
    // field's own doc comment). Every mutating `KeyOutcome`/raw-key arm
    // in `run_normal_mode_navigation` uses this (or `as_writable_mut`)
    // instead of matching `NavBuffer::Editable` directly, so "read-only
    // while a debug session is attached" is enforced by omission the
    // exact same way plain `ReadOnly` scrollback navigation already is.
    fn is_writable(&self) -> bool {
        match self {
            NavBuffer::ReadOnly(_) => false,
            NavBuffer::Editable(tb) => !tb.is_readonly(),
        }
    }

    fn as_writable_mut(&mut self) -> Option<&mut TextBuffer> {
        match self {
            NavBuffer::ReadOnly(_) => None,
            NavBuffer::Editable(tb) if !tb.is_readonly() => Some(tb),
            NavBuffer::Editable(_) => None,
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

    // ReadOnly's own ScreenBuffer never overrides Buffer::viewport_left's
    // default (always 0, see that method's own doc comment) -- delegated
    // through anyway rather than just relying on NavBuffer's own default,
    // so Editable's real TextBuffer value is actually used.
    fn viewport_left(&self) -> usize {
        match self {
            NavBuffer::ReadOnly(b) => b.viewport_left(),
            NavBuffer::Editable(b) => b.viewport_left(),
        }
    }

    fn set_viewport_left(&mut self, col: usize) {
        match self {
            NavBuffer::ReadOnly(b) => b.set_viewport_left(col),
            NavBuffer::Editable(b) => b.set_viewport_left(col),
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

    fn line_wraps(&self, line: usize) -> bool {
        match self {
            NavBuffer::ReadOnly(b) => b.line_wraps(line),
            NavBuffer::Editable(b) => b.line_wraps(line),
        }
    }
}

// Adjusts `buf`'s viewport so its navigation cursor's line is visible,
// scrolling as little as possible -- matching vim's own scrolling, which
// only jumps when the cursor would otherwise move off-screen, not
// recentering on every motion. `content_cols`: this pane's own current
// content width (see `nav_content_cols`) -- a no-op horizontally for a
// `ReadOnly` `ScreenBuffer` (`Buffer::viewport_left`'s own doc comment
// on why that one never actually scrolls), the real thing for `Editable`
// (mirrors `fileeditor::scroll_to_show_cursor` exactly).
pub(crate) fn scroll_to_show_cursor(buf: &mut impl BisheditBuffer, content_cols: usize) {
    let (line, col) = buf.cursor();
    let height = buf.viewport_height();
    if line < buf.viewport_top() {
        buf.set_viewport_top(line);
    } else if line >= buf.viewport_top() + height {
        buf.set_viewport_top(line + 1 - height);
    }
    // `viewport_left` is a display column, not a char index (see
    // bishedit::unicode_width's own doc comment) -- mirrors
    // fileeditor::scroll_to_show_cursor exactly, see its own identical
    // comment on why.
    let cursor_col = col_of(&buf.line_chars(line), col);
    let width = content_cols.max(1);
    if cursor_col < buf.viewport_left() {
        buf.set_viewport_left(cursor_col);
    } else if cursor_col >= buf.viewport_left() + width {
        buf.set_viewport_left(cursor_col + 1 - width);
    }
}

// `buf`'s own current content width for scroll_to_show_cursor's purposes
// -- `ReadOnly`'s `ScreenBuffer` has no gutter at all (the whole pane
// width is content), `Editable`'s `TextBuffer` does (see
// fileeditor::editor_content_cols).
fn nav_content_cols(buf: &NavBuffer, rect: Rect) -> usize {
    match buf {
        NavBuffer::ReadOnly(_) => rect.cols,
        NavBuffer::Editable(tb) => fileeditor::editor_content_cols(tb, rect),
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

// How many rows of this pane's own rect are available for the
// scrollback view -- all of them: the mode-line lives in the terminal's
// own global status row now (render_global_status_row), not carved out
// of this pane's own rect. `.max(1)`: a degenerate zero-height rect
// still gets *some* content rather than a panicking view.
fn normal_mode_content_rows(rect: Rect) -> usize {
    rect.rows.max(1)
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
    // "recording @a" while `q{reg}` is active, ahead of the mode label --
    // vim's own recording indicator.
    let recording = vk.is_recording().map(|r| format!("recording @{r}  ")).unwrap_or_default();
    let label = mode_label(vk);
    if !pending.is_empty() {
        return format!("{}{} {}", recording, label, pending);
    }
    let last = vk.last_motion_display();
    if !last.is_empty() {
        return format!("{}{} [{}]", recording, label, last);
    }
    format!("{}{}", recording, label)
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

// Draws `buf`'s current viewport into `rect` (the focused pane's own
// rectangle -- see pane_rect), reusing sgr_codes the same way render_row
// does for a live pane, plus the global mode-line row (render_global_
// status_row) -- not part of `rect` at all, see that function's own doc
// comment. Lines past the end of the buffer's content are left blank --
// vim's own "~" convention for that is one more piece of scope this
// first pass leaves out. Positions the real terminal cursor at the
// navigation cursor's own screen location afterward -- not the mode-
// line, so the blinking cursor stays where it's actually useful (showing
// position in the content) even while the mode-line shows a pending
// command/search line taking input from the very same keystream.
fn render_normal_mode_frame(buf: &ScreenBuffer, rect: Rect, vk: &VimKeys, command_line: Option<&str>, term_rows: usize, term_cols: usize) {
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

    out.push_str(&render_global_status_row(&normal_mode_status_text(buf, vk, command_line, term_cols), term_rows));

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
    // Both fields are boxed only to keep this enum's own size close to
    // its other two (unit-ish) variants -- `Prompt`'s inline `String` is
    // by far the common case, and clippy flags an enum whose largest
    // variant dwarfs the rest, since every `NavStart` value pays that
    // largest variant's stack size regardless of which one it is. The
    // return type (`Option<(TextBuffer, VimKeys)>`) stays unboxed --
    // that's a plain function return, not a value every other variant of
    // some enum has to be sized against.
    Edit(Box<TextBuffer>, Box<VimKeys>),
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

// A session's live syn_col_* colors (see bishedit::highlight::
// SYN_COL_OPTIONS/ColorOverrides, exec::Shell::bishopt_color), resolved
// fresh from `shell` -- every caller of this owns its own snapshot rather
// than sharing one, since `--set`/`--unset` can only ever run from an
// actual shell prompt (bishopt is a builtin, not reachable from inside
// the modal file editor or its own diagnostics pane), so whichever
// moment a caller resolves this at is always current for however long it
// then uses it.
fn syntax_color_overrides(shell: &exec::Shell) -> highlight::ColorOverrides {
    highlight::SYN_COL_OPTIONS.iter().filter_map(|(kind, name)| shell.bishopt_color(name).map(|c| (*kind, c))).collect()
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
// `&mut NavBuffer` (not `&NavBuffer`) specifically so the `Editable` arm
// can call `checkpoint_undo` first -- this is the one call site every
// top-level `KeyOutcome` arm reaches (including after `EnterInsert`/
// `OpenLine`/`Change`'s own `fileeditor::run_insert_mode` call fully
// returns), which is what makes it the right place to commit an undo-tree
// node: an entire Insert-mode session collapses into a single checkpoint
// here, not one per keystroke, with no changes needed to any of the
// individual `Operator`/`OperatorLines`/`Put`/... arms that call this.
fn render_nav_frame(buf: &mut NavBuffer, vk: &VimKeys, rect: Rect, term_rows: usize, term_cols: usize, color_overrides: Option<&highlight::ColorOverrides>) {
    match buf {
        // ReadOnly (NavStart::Prompt/JobDetach) never carries file syntax
        // highlighting to color-override at all -- it's replaying/
        // searching an already-rendered grid, not re-highlighting bash
        // source -- so every caller of those two starts just passes None
        // here; only NavStart::Edit's own caller (run_edit_frame) has a
        // real value to offer.
        NavBuffer::ReadOnly(sb) => render_normal_mode_frame(sb, rect, vk, None, term_rows, term_cols),
        NavBuffer::Editable(tb) => {
            tb.checkpoint_undo();
            fileeditor::render_editor_frame(tb, vk, fileeditor::EditorMode::Normal, rect, term_rows, term_cols, color_overrides);
        }
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
    // `Some` iff `start` is `NavStart::Edit` -- the id `debug_frames`
    // itself is keyed on (see Frame::DebugRun's own doc comment), needed
    // both for `K`'s own live-value lookup below and threaded on into
    // `handle_command_mode` so its `:dbg` handling can attach/detach a
    // session. `None` for the other two starts (a plain prompt or a
    // detached job's own frozen screen) -- neither has a file buffer
    // `:dbg` could mean anything against, so `debug_frames` is simply
    // never touched from those calls.
    edit_frame_id: Option<EditFrameId>,
    debug_frames: &mut HashMap<EditFrameId, debugger::DebugSession>,
    cmd_history: &mut History,
    registers: &mut Registers,
    start: NavStart,
    term_rows: &mut usize,
    term_cols: &mut usize,
    color_overrides: Option<&highlight::ColorOverrides>,
) -> io::Result<(NavExit, Option<(TextBuffer, VimKeys)>)> {
    let mut rect = pane_rect(&windows[*current_window], windows[*current_window].focused_pane, *term_rows, *term_cols);

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
        NavStart::Edit(tb, vk0) => (NavBuffer::Editable(*tb), *vk0),
    };

    let _guard = term::RawGuard::enable_with_mouse(0)?;
    // Repaints the whole screen first -- necessary the very first time
    // normal mode ever triggers promotion (the alternate screen buffer
    // starts out blank), harmless otherwise -- then this pane's own
    // rectangle on top of that with the current view.
    compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
    render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
    let mut pending_view = PendingView::None;

    let result: (NavExit, Option<(TextBuffer, VimKeys)>) = 'nav: loop {
        // Recomputed every iteration, not just once up front: this pane's
        // own rect can change mid-loop without this function ever
        // exiting -- `:diag` (run from right inside this same loop's own
        // `:` handling) can graft a sibling diagnostics pane below this
        // one, shrinking it. Every other window/pane action that resizes
        // anything (`<C-w>` chords, a click) already exits via
        // `KeyOutcome::Window`/click handling, so this was never
        // observable before `:diag` -- cheap enough to just always do.
        rect = pane_rect(&windows[*current_window], windows[*current_window].focused_pane, *term_rows, *term_cols);
        // Waits for a byte to actually be ready *before* calling
        // vk.next_key below, rather than passing this same on_idle work
        // as that call's own closure (editor::read_key_idle's usual
        // shape, used everywhere else) -- vk.next_key(&mut self, ...)
        // holds `vk` exclusively borrowed for its whole call, including
        // while its own on_idle closure runs, which would make it
        // impossible for that closure to also touch `buf`/`vk` to
        // redraw them. Doing the wait out here instead, where neither
        // is borrowed by anything else, is what actually lets a
        // just-attached session client's blank real terminal get this
        // pane's *real* content redrawn immediately -- not just once
        // the next keystroke happens to arrive (which, for a client
        // that reattaches and doesn't immediately type anything, could
        // be never). Same IDLE_POLL_MS interval read_key_idle's own
        // loop uses, so behavior is identical once a byte does arrive.
        while !term::stdin_ready(editor::IDLE_POLL_MS) {
            if service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid) {
                // compositor_redraw already ran (inside
                // service_background_jobs), but that alone paints this
                // pane blank if it's a `Frame::Edit` (see that
                // function's own doc comment) -- follow up with the
                // real thing, the exact same call this function's own
                // entry above already makes once, up front.
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
        }
        // A byte is already known ready -- read_key_idle's own internal
        // poll will see that immediately, so this on_idle closure is
        // never actually called; kept only to match its existing
        // signature rather than adding a second, idle-free entry point.
        let mut key = match vk.next_key(|| editor::read_key_idle(&mut || {}))? {
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
                    render_command_transcript(&sessions[&session_id].command_transcript, *term_rows, *term_cols);
                    key = match vk.next_key(|| editor::read_key_idle(&mut || {
                        service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid);
                    }))? {
                        Some(k) => k,
                        None => {
                            let exit = if matches!(buf, NavBuffer::Editable(_)) { NavExit::Quit } else { NavExit::Detached };
                            break 'nav (exit, nav_buffer_into_edit_state(buf, vk));
                        }
                    };
                }
                PendingView::Output | PendingView::Transcript => {
                    pending_view = PendingView::None;
                    compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                    render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                    break;
                }
            }
        }

        // A qualifying left click (see MouseEvent::is_left_click),
        // intercepted before vk.feed ever sees it -- mouse clicks are an
        // orthogonal input channel to vim's own key-state-machine, same
        // spirit as PendingView's own resolution just above. A genuine
        // focus change exits normal mode (NavExit::Detached) exactly like
        // the existing KeyOutcome::Window arm below already does for
        // Ctrl-W chords -- see its own doc comment: "running one always
        // exits normal mode too... jumps to the next window and drops
        // straight into its live prompt, not back into this pane's own
        // normal mode." A same-window pane-focus change via Ctrl-W h/j/
        // k/l already detaches today, so a same-window pane click doing
        // the same is consistent, not a new limitation. A wheel notch
        // (MouseEvent::is_scroll_up/is_scroll_down) scrolls this pane's
        // own view -- same Motion::ScrollLineDown/Up and the same
        // apply_motion_or_reselect/scroll_to_show_cursor/render_nav_frame
        // sequence as an ordinary KeyOutcome::Motion below, just reached
        // directly here since a wheel event never goes through vk.feed at
        // all (same "orthogonal input channel" reasoning as a click). A
        // miss, a click on the already-focused tab/pane, or a non-
        // qualifying mouse event (drag/release/some other button) is just
        // a no-op -- re-render (in case a PendingView overlay was just
        // cleared above) and keep navigating.
        if let Key::Mouse(ev) = key {
            if ev.is_left_click() {
                match hit_test_click(ev, sessions, windows, *current_window, *term_rows, *term_cols) {
                    Some(ClickTarget::Window(idx)) if idx != *current_window => {
                        freeze_focused_idle_prompt(sessions, windows, *current_window);
                        *current_window = idx;
                        compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                        return Ok((NavExit::Detached, nav_buffer_into_edit_state(buf, vk)));
                    }
                    Some(ClickTarget::Pane(pane_id)) if pane_id != windows[*current_window].focused_pane => {
                        if matches!(windows[*current_window].stack().last(), Some(Frame::Session(_))) {
                            let sid = windows[*current_window].owning_session();
                            freeze_idle_prompt(sessions.get_mut(&sid).unwrap());
                        }
                        windows[*current_window].focused_pane = pane_id;
                        compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
                        return Ok((NavExit::Detached, nav_buffer_into_edit_state(buf, vk)));
                    }
                    _ => {}
                }
            } else if ev.is_scroll_down() || ev.is_scroll_up() {
                let motion = if ev.is_scroll_down() { motion::Motion::ScrollLineDown } else { motion::Motion::ScrollLineUp };
                editor::apply_motion_or_reselect(&mut vk, &mut buf, motion, Some(fileeditor::MOUSE_WHEEL_LINES));
                let content_cols = nav_content_cols(&buf, rect);
                scroll_to_show_cursor(&mut buf, content_cols);
            }
            render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            continue 'nav;
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
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
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
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                continue;
            }
            Key::Char('d') if vk.is_idle() && buf.is_writable() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let register = vk.take_pending_register();
                let end_cursor = buf.cursor();
                if let Some(tb) = buf.as_writable_mut() {
                    tb.delete_selections(registers, register);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                continue;
            }
            // `c`: every deleted selection's own gap (delete_selections'
            // own return, not just its bool-ish "did anything happen")
            // gets typed into at once -- run_insert_mode's own
            // `extra_cursors` param replicates every keystroke across
            // all of them, not just the one `buf.cursor()` happens to
            // land on. A single selection is just the len-1 case of the
            // exact same call, unchanged from before this existed.
            Key::Char('c') if vk.is_idle() && buf.is_writable() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let register = vk.take_pending_register();
                let end_cursor = buf.cursor();
                let mut gaps: Vec<(usize, usize)> = Vec::new();
                if let Some(tb) = buf.as_writable_mut() {
                    gaps = tb.delete_selections(registers, register);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                if !gaps.is_empty() && let Some(tb) = buf.as_writable_mut() {
                    let (insert_term_rows, insert_term_cols) = (*term_rows, *term_cols);
                    fileeditor::run_insert_mode(
                        tb,
                        &mut vk,
                        rect,
                        registers,
                        &mut || service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid),
                        false,
                        insert_term_rows,
                        insert_term_cols,
                        color_overrides,
                        &gaps[1..],
                    )?;
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                continue;
            }
            Key::Char('p') | Key::Char('P') if vk.is_idle() && buf.is_writable() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let register = vk.take_pending_register();
                let end_cursor = buf.cursor();
                if let Some(tb) = buf.as_writable_mut() {
                    tb.put_over_selections(registers, register);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                continue;
            }
            // Visual `>`/`<`: same shape as `p`/`P` just above -- shifts
            // every committed selection (plus the active one) whole-line
            // via fileeditor::indent_selections/outdent_selections, then
            // drops back to Normal mode at the first shifted line, same
            // as vim's own Visual-mode `>`/`<`.
            Key::Char('>') if vk.is_idle() && buf.is_writable() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let end_cursor = buf.cursor();
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::indent_selections(tb);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                continue;
            }
            Key::Char('<') if vk.is_idle() && buf.is_writable() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let end_cursor = buf.cursor();
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::outdent_selections(tb);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                continue;
            }
            Key::Char('S') if vk.is_idle() && buf.is_writable() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let end_cursor = buf.cursor();
                if let Some(tb) = buf.as_writable_mut()
                    && let Some(Key::Char(ch)) = vk.next_key(|| editor::read_key_idle(&mut || {
                        service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid);
                    }))?
                {
                    fileeditor::surround_selections(tb, ch);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                continue;
            }
            Key::Escape | Key::CtrlC if vk.is_idle() && (key == Key::Escape || matches!(buf, NavBuffer::Editable(_))) && (vk.is_visual() || !buf.selections().is_empty()) => {
                let end_cursor = buf.cursor();
                vk.end_visual(end_cursor);
                buf.selections_mut().clear();
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                continue;
            }
            // `q`/`@` -- macro record/replay. Host-level, same tier as
            // every other outer-tier arm here, for the same reason: `@`
            // needs to re-enter this exact match (Visual `y`/`d`/`Z`/`:`,
            // and `q`/`@` themselves) for each replayed key, which only
            // this loop -- not vimkeys.rs's own `feed()` -- can drive
            // (see `VimKeys::next_key`'s own doc comment). No `matches!
            // (buf, NavBuffer::Editable(_))` gate the way `d`/`c`/`p`/`S`
            // have: recording/replaying a pure navigation-and-yank macro
            // over read-only scrollback is just as meaningful as one that
            // edits a file. One real gap: `i` from `ReadOnly` exits this
            // loop entirely (see `KeyOutcome::EnterInsert`'s own arm
            // below), silently dropping an in-progress recording along
            // with it -- same as every other piece of `vk` state already
            // does on that path (`nav_buffer_into_edit_state` returns
            // `None` for `ReadOnly`).
            //
            // Bare `q` while already recording always stops it, full
            // stop -- checked first so it takes priority over the "start
            // a new recording" arm below.
            Key::Char('q') if vk.is_idle() && vk.is_recording().is_some() => {
                vk.stop_recording();
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                continue;
            }
            // `q{reg}`: starts recording. The register-name lookahead
            // goes through `vk.next_key` too (not a bare `read_key_idle`
            // like `S`'s delimiter), so a macro whose own recorded body
            // contains `q{reg}` (recording-within-a-macro) still replays
            // correctly.
            Key::Char('q') if vk.is_idle() => {
                if let Some(Key::Char(ch)) = vk.next_key(|| editor::read_key_idle(&mut || {
                    service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid);
                }))? && ch.is_ascii_alphabetic()
                {
                    vk.start_recording(ch);
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                continue;
            }
            // `@{reg}`/`@@`: replays. `is_idle_except_count` (not
            // `is_idle`) plus `take_count`: unlike every other arm here,
            // a count typed in front of `@` is a real repeat count
            // (`3@a`), not one silently discarded.
            Key::Char('@') if vk.is_idle_except_count() => {
                let count = vk.take_count().unwrap_or(1).max(1);
                if let Some(Key::Char(ch)) = vk.next_key(|| editor::read_key_idle(&mut || {
                    service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid);
                }))? && (ch == '@' || ch.is_ascii_lowercase())
                {
                    vk.queue_macro_replay(ch, count);
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                continue;
            }
            Key::Char('Z') => {
                let k2 = vk.next_key(|| editor::read_key_idle(&mut || {
                    service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid);
                }))?;
                if k2 != Some(Key::Char('Z')) {
                    continue;
                }
                if matches!(buf, NavBuffer::Editable(_)) {
                    // `ZZ`: vim's own alias for `:x` -- save and quit.
                    let mut saved = true;
                    if let Some(tb) = buf.as_writable_mut() {
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
                    render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                    continue;
                }
                break 'nav (NavExit::Resume(initial_text.clone(), initial_cursor), nav_buffer_into_edit_state(buf, vk));
            }
            // `K`: hover whatever's under the cursor -- same shared
            // lookup (docs::hover_lines_at) debugger.rs's own pause loop
            // used to build itself. Its live-value tier now comes from
            // `debug_frames`: `Some` iff a `:dbg` session is actually
            // attached to *this* edit frame, in which case the hovered
            // identifier's current value (Shell::debug_peek_var) is
            // shown same as it always was -- `|_| None` otherwise (no
            // session attached, or this isn't even an Edit frame at
            // all). Scoped to `Editable` only (matching `:help`/`:git`/
            // `:format`'s own "only means something while editing a real
            // file" convention) -- for `ReadOnly` scrollback navigation
            // it just falls through to vimkeys' own handling (a no-op,
            // since `K` isn't bound there either in real vim). The doc
            // index is rebuilt fresh from this buffer's own *live* text
            // every time (TextBuffer::text, not a re-read off disk) so
            // an unsaved edit is reflected immediately -- moot while a
            // debug session is attached (readonly, so there's never an
            // unsaved edit to reflect), but this arm is shared with the
            // ordinary, non-debugged case too.
            Key::Char('K') if vk.is_idle() && matches!(buf, NavBuffer::Editable(_)) => {
                let NavBuffer::Editable(tb) = &buf else { unreachable!("guarded by this arm's own match above") };
                let (row, col) = tb.cursor();
                let chars = tb.line_chars(row);
                let line_text: String = chars.iter().collect();
                let base_path = tb.path().map(|p| p.to_path_buf()).unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join("untitled"));
                let index = docs::DocIndex::build_from_source(&tb.text(), &base_path);
                let debug_session = edit_frame_id.and_then(|id| debug_frames.get(&id));
                let hover_lines = docs::hover_lines_at(&chars, col, &line_text, &index, |name| debug_session.and_then(|s| s.peek_var(name)));
                let gutter_width = rect.cols.saturating_sub(fileeditor::editor_content_cols(tb, rect));
                let cursor_row = rect.row + row.saturating_sub(tb.viewport_top());
                let cursor_display_col = col_of(&chars, col);
                let cursor_col = rect.col + gutter_width + cursor_display_col.saturating_sub(tb.viewport_left());
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                print!("{}", fileeditor::render_hover_popup(&hover_lines, cursor_row, cursor_col, rect));
                let _ = io::stdout().flush();
                continue;
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
                let outcome = handle_command_mode(
                    session_id,
                    sessions,
                    windows,
                    current_window,
                    next_session_id,
                    next_window_id,
                    cmd_history,
                    sinks_are_grid,
                    job_frames,
                    debug_frames,
                    registers,
                    term_rows,
                    term_cols,
                    buf.as_editable_mut(),
                    None,
                );
                // `:diag` (only command that can, today) may have grafted
                // a sibling pane in below this one -- recomputed here,
                // not just at the top of the next loop iteration, since
                // `Ran`'s own `render_nav_frame` call just below still
                // needs the *current* rect, not next keystroke's.
                rect = pane_rect(&windows[*current_window], windows[*current_window].focused_pane, *term_rows, *term_cols);
                match outcome {
                    // Matches vim: an aborted/cancelled ':' command drops
                    // back into Normal mode, not out of it entirely.
                    CommandModeOutcome::Cancelled => {
                        render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
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
                    // on the very next key). render_nav_frame runs first
                    // regardless of which branch this takes -- NOT just
                    // handle_command_mode's own baseline compositor_
                    // redraw (which reads this pane's session grid, and
                    // for a `NavBuffer::Editable` pane that grid is never
                    // actually fed while the real editor content is being
                    // driven -- see fileeditor::freeze_editor_frame's own
                    // doc comment -- so relying on it alone painted a
                    // blank pane under any error/output overlay). Once
                    // this repaints the real content, the overlay (when
                    // shown) only ever overwrites its own reserved bottom
                    // rows on top of it (see build_command_output_
                    // overlay's own doc comment on why it no longer
                    // blanks anything above them).
                    CommandModeOutcome::Ran { output, status } => {
                        render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
                        if !output.is_empty() || status != 0 {
                            render_command_output_overlay(&output, status, *term_rows, *term_cols);
                            pending_view = PendingView::Output;
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
                let content_cols = nav_content_cols(&buf, rect);
                scroll_to_show_cursor(&mut buf, content_cols);
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
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
                    if let Some(tb) = buf.as_writable_mut() {
                        fileeditor::resolve_insert_start(tb, cmd);
                        let (insert_term_rows, insert_term_cols) = (*term_rows, *term_cols);
                        fileeditor::run_insert_mode(tb, &mut vk, rect, registers, &mut || service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid), false, insert_term_rows, insert_term_cols, color_overrides, &[])?;
                    }
                    render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
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
                    if let Some(tb) = buf.as_writable_mut() {
                        let (insert_term_rows, insert_term_cols) = (*term_rows, *term_cols);
                        fileeditor::run_insert_mode(tb, &mut vk, rect, registers, &mut || service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid), true, insert_term_rows, insert_term_cols, color_overrides, &[])?;
                    }
                    render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
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
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::ReselectVisual => {
                if let Some((shape, anchor, cursor)) = vk.last_visual() {
                    buf.set_cursor(cursor.0, cursor.1);
                    vk.begin_visual(shape, anchor);
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::Jump { forward } => {
                let current = buf.cursor();
                let target = if forward { vk.jump_forward(current) } else { vk.jump_back(current) };
                if let Some((row, col)) = target {
                    let row = row.min(buf.line_count() - 1);
                    let col = col.min(buf.line_len(row));
                    buf.set_cursor(row, col);
                    let content_cols = nav_content_cols(&buf, rect);
                    scroll_to_show_cursor(&mut buf, content_cols);
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            // `u`/`Ctrl-R`: no-op for `ReadOnly`, same as every other
            // mutating outcome. Guarded on `!vk.is_visual() &&
            // buf.selections().is_empty()` -- real vim's Visual mode binds
            // bare `u`/`U` to lowercase/uppercase the selection, not
            // implemented in this codebase today (see KeyOutcome::Undo's
            // own doc comment in vimkeys.rs), so `u` simply does nothing
            // while a selection is active rather than misfiring as undo.
            KeyOutcome::Undo(count) => {
                if !vk.is_visual() && buf.selections().is_empty() && let Some(tb) = buf.as_writable_mut() {
                    for _ in 0..count.unwrap_or(1).max(1) {
                        if !tb.undo() {
                            break;
                        }
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::Redo(count) => {
                if !vk.is_visual() && buf.selections().is_empty() && let Some(tb) = buf.as_writable_mut() {
                    for _ in 0..count.unwrap_or(1).max(1) {
                        if !tb.redo() {
                            break;
                        }
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            // `g-`/`g+`: same guard as `u`/`Ctrl-R` just above, for the
            // same reason.
            KeyOutcome::UndoSeq { forward, count } => {
                if !vk.is_visual() && buf.selections().is_empty() && let Some(tb) = buf.as_writable_mut() {
                    for _ in 0..count.unwrap_or(1).max(1) {
                        if !tb.time_travel(forward) {
                            break;
                        }
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
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
                } else if let Some(tb) = buf.as_writable_mut() {
                    match op {
                        Op::Delete => {
                            fileeditor::delete_motion(tb, registers, motion, count, register);
                        }
                        Op::Change => {
                            let m = fileeditor::redirect_cw_to_ce(tb, &motion);
                            if fileeditor::delete_motion(tb, registers, m, count, register) {
                                let (insert_term_rows, insert_term_cols) = (*term_rows, *term_cols);
                                fileeditor::run_insert_mode(tb, &mut vk, rect, registers, &mut || service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid), false, insert_term_rows, insert_term_cols, color_overrides, &[])?;
                            }
                        }
                        Op::Lowercase | Op::Uppercase | Op::CaseToggle => {
                            fileeditor::case_operator_motion(tb, motion, count, fileeditor::case_kind_for_op(op));
                        }
                        Op::Indent => fileeditor::indent_operator_motion(tb, motion, count),
                        Op::Outdent => fileeditor::outdent_operator_motion(tb, motion, count),
                        Op::Yank => unreachable!("handled above"),
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::OperatorLines(op, count, register) => {
                if op == Op::Yank {
                    editor::yank_lines(&buf, registers, count, register);
                } else if let Some(tb) = buf.as_writable_mut() {
                    match op {
                        Op::Delete => fileeditor::delete_lines(tb, registers, count, register),
                        Op::Change => {
                            fileeditor::delete_lines(tb, registers, count, register);
                            let (insert_term_rows, insert_term_cols) = (*term_rows, *term_cols);
                            fileeditor::run_insert_mode(tb, &mut vk, rect, registers, &mut || service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid), false, insert_term_rows, insert_term_cols, color_overrides, &[])?;
                        }
                        Op::Lowercase | Op::Uppercase | Op::CaseToggle => fileeditor::case_operator_lines(tb, count, fileeditor::case_kind_for_op(op)),
                        Op::Indent => fileeditor::indent_lines(tb, count),
                        Op::Outdent => fileeditor::outdent_lines(tb, count),
                        Op::Yank => unreachable!("handled above"),
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            // `p`/`P`/`x`/`J`/`gJ`/`ys`/`ds`/`cs`/`r`/`~`/`o`/`O`: all
            // mutate, so all a no-op for `ReadOnly` (same as before this
            // unification -- a view over already-rendered scrollback,
            // not an editable buffer), each calling the matching
            // `fileeditor::` helper for `Editable`.
            KeyOutcome::Put { before, count, register } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::put(tb, registers, before, count, register);
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::DeleteCharForward { count, register } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::delete_char_forward(tb, registers, count, register);
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::Join { count, with_space } => {
                if let Some(tb) = buf.as_writable_mut() {
                    tb.join_lines(count.unwrap_or(1).max(1), with_space);
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::AddSurround { target, ch } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::add_surround(tb, target, ch);
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::DeleteSurround { ch } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::delete_surround(tb, ch);
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::ChangeSurround { ch, replacement } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::change_surround(tb, ch, replacement);
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::ReplaceChar { ch, count } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::replace_char(tb, ch, count.unwrap_or(1).max(1));
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::ToggleCase { count } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::toggle_case(tb, count.unwrap_or(1).max(1));
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::AdjustNumber { delta } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::adjust_number(tb, delta);
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            KeyOutcome::OpenLine { above } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::open_line(tb, above);
                    let (insert_term_rows, insert_term_cols) = (*term_rows, *term_cols);
                    fileeditor::run_insert_mode(tb, &mut vk, rect, registers, &mut || service_background_jobs(sessions, windows, job_frames, *current_window, term_rows, term_cols, *sinks_are_grid), false, insert_term_rows, insert_term_cols, color_overrides, &[])?;
                }
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
            // dispatch_window_cmd does the actual work (shared with
            // run_edit_frame's own identical need -- see its own doc
            // comment); this loop just exits afterward, same as any
            // other Window outcome -- a focus change, nothing to resume,
            // handing the buffer/vk back so an `Editable` caller can
            // re-stash them.
            KeyOutcome::Window(cmd, count) => {
                dispatch_window_cmd(cmd, count, sessions, windows, current_window, next_session_id, next_window_id, sinks_are_grid, *term_rows, *term_cols);
                return Ok((NavExit::Detached, nav_buffer_into_edit_state(buf, vk)));
            }
            // Rendered on every keystroke, not just a resolved Motion --
            // the status bar needs to show a pending count/prefix (e.g.
            // "20g" mid-`20gg`) and a search's in-progress text live, not
            // just the end result once a motion actually applies.
            KeyOutcome::Pending | KeyOutcome::None => {
                render_nav_frame(&mut buf, &vk, rect, *term_rows, *term_cols, color_overrides);
            }
        }
    };

    compositor_redraw(sessions, windows, *current_window, *term_rows, *term_cols);
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

// The visible text of one tab's own segment -- shared by render_tab_bar
// and tab_bar_regions so the rendered tab bar and its hit-test column
// ranges (see hit_test_click) can never drift apart from each other.
fn tab_segment_text(id: u32, cwd: &str) -> String {
    format!(" [{}] {} ", id, cwd)
}

fn render_tab_bar(snapshot: &[(u32, bool, String)]) -> String {
    let mut line = String::new();
    for (id, current, cwd) in snapshot {
        let seg = tab_segment_text(*id, cwd);
        if *current {
            line.push_str("\x1b[7m");
            line.push_str(&seg);
            line.push_str("\x1b[0m");
        } else {
            line.push_str(&seg);
        }
    }
    line
}

// (window_id, start_col, end_col), 0-indexed and half-open, in
// tab_bar_snapshot's own order -- for hit-testing a tab-bar click (see
// hit_test_click) against exactly the same segment widths render_tab_bar
// just drew, via the shared tab_segment_text helper.
fn tab_bar_regions(snapshot: &[(u32, bool, String)]) -> Vec<(u32, usize, usize)> {
    let mut col = 0;
    snapshot
        .iter()
        .map(|(id, _, cwd)| {
            let start = col;
            col += tab_segment_text(*id, cwd).chars().count();
            (*id, start, col)
        })
        .collect()
}

// Where a qualifying left click (see MouseEvent::is_left_click) landed:
// the tab bar (an absolute index into `windows`, mirroring
// dispatch_window_cmd's own GotoFirstWindow/GotoLastWindow -- there's no
// WindowAction variant for an arbitrary-index jump either) or a pane
// within the current window. `None` (not a variant here) covers a miss
// -- landing on a divider strip, or off both areas entirely.
enum ClickTarget {
    Window(usize),
    Pane(PaneId),
}

// Hit-tests a click's screen coordinates against the tab bar (if any),
// then the current window's own pane layout -- shared by both places a
// click can be acted on (repl::run's plain-prompt loop, and
// run_normal_mode_navigation's own loop), so the two can't disagree on
// what a given click means. `ev.row`/`ev.col` are 1-indexed (the
// terminal's own convention -- see MouseEvent's own doc comment); `Rect`/
// the tab bar's own row are 0-indexed, hence the -1 conversions below.
fn hit_test_click(
    ev: editor::MouseEvent,
    sessions: &HashMap<SessionId, SessionState>,
    windows: &[WindowEntry],
    current_window: usize,
    term_rows: usize,
    term_cols: usize,
) -> Option<ClickTarget> {
    let row0 = (ev.row as usize).saturating_sub(1);
    let col0 = (ev.col as usize).saturating_sub(1);
    if row0 == term_rows.saturating_sub(1) {
        let regions = tab_bar_regions(&tab_bar_snapshot(sessions, windows, current_window));
        let (id, _, _) = regions.into_iter().find(|(_, start, end)| col0 >= *start && col0 < *end)?;
        return Some(ClickTarget::Window(windows.iter().position(|w| w.id == id)?));
    }
    let area = Rect { row: 0, col: 0, rows: content_rows(term_rows), cols: term_cols };
    let mut regions = Vec::new();
    let mut dividers = Vec::new();
    compute_regions(&windows[current_window].layout, area, &mut regions, &mut dividers);
    let (pane_id, _) = regions.into_iter().find(|(_, r)| row0 >= r.row && row0 < r.row + r.rows && col0 >= r.col && col0 < r.col + r.cols)?;
    Some(ClickTarget::Pane(pane_id))
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

// `:help`/`:h`/`:?`'s own static content -- see that match arm's own doc
// comment for the scope this deliberately stays within. Kept short
// enough to fit one ordinary terminal's worth of command-output overlay
// without truncating (see command_mode_content_rows) -- a genuinely
// exhaustive reference would need real pagination/topics this feature
// doesn't attempt.
const EDITOR_HELP_TEXT: &str = "\
bish editor -- quick reference (:help, :h, :?)

Motion:  h j k l | w b e ge | 0 ^ $ | gg G | f F t T ; , | % | { }
Search:  /pat  ?pat  n  N          Marks:  m{a-z}  `{mark}  '{mark}
Jumps:   <C-o> back  <C-i> forward
Visual:  v (char)  V (line)  <C-v> (block)  o swaps ends
Insert:  i I a A o O  --  <Esc> exits back to Normal mode
Operators (take a motion, or double the key for the whole line):
         d delete  c change  y yank  > indent  < outdent  gu/gU/g~ case
Surround: ys{motion}{char} add   cs{old}{new} change   ds{char} delete
Registers: \"{reg} before an operator/put, e.g. \"ayy then \"ap
Undo/redo: u / <C-r>     Put: p P     Repeat last change: .
Hover: K on an identifier shows its live value, a doc comment, or a
       man-page snippet for an external command

Colon commands:
  :w [FILE]        write (:wq/:x write+quit, :q quit, :q! discard+quit)
  :s/PAT/REPL/[g]  substitute on this line (prefix a range, e.g. :%s/../../)
  :git blame       toggle a per-line blame gutter (:git diff for +/~/-)
  :diff            toggle +/~/- markers vs. what's on disk (no git needed)
  :format          run this file's own formatter
  :diag [clear]    toggle the diagnostics pane
  :dbg             attach a read-only debug session (:dbg help for more)
  :browse [PATH]   file browser in this pane (hjkl/arrows, tab selects,
                   / filters, enter opens, esc back)
  :help, :h, :?    this screen";

// `:dbg help`/`:dbg h`/`:dbg ?`'s own reference text -- shown via the
// same command-output overlay every other command's own output already
// uses (CommandModeOutcome::Ran), not a hover popup (that was the
// original standalone debugger's own convention, replaced along with
// the rest of that view -- see debugger.rs's own top-of-file doc
// comment).
const DBG_HELP_TEXT: &str = "\
bish dbg -- quick reference (:dbg help, :dbg h, :dbg ?)

:dbg                  attach a read-only debug session to this file
:dbg run              start executing the script
:dbg break [line]     toggle a breakpoint (cursor's own line if omitted)
:dbg break add|remove N   explicit add/remove
:dbg print NAME       show a variable's current value
:dbg quit             detach (writable again)
While paused at a breakpoint (in the debug-run pane): c)ontinue n)ext
s)tep p)rint q)uit h)elp -- bare key, or `:` then the long/short name";

// Command mode's own row, immediately above the tab bar (see render_
// compositor_frame's own "pinned to the terminal's real last row"
// comment) -- 0-indexed. Global, not tied to any particular pane's rect
// -- the same row render_global_status_row draws the ordinary mode-line
// into the rest of the time (see its own doc comment for why the two
// never conflict).
fn command_mode_row(term_rows: usize) -> usize {
    term_rows.saturating_sub(2)
}

// How many rows are free above command mode's own prompt row for the
// output overlay/transcript view to grow into.
fn command_mode_content_rows(term_rows: usize) -> usize {
    command_mode_row(term_rows)
}

// The terminal's one global mode-line/status row -- `command_mode_row`,
// styled in reverse video, exactly like real vim's own single command/
// status line: `run_command_mode`'s own `:` prompt draws directly to
// this same row while it's active, and this function is simply never
// called while that's happening (both live-render loops -- build_
// editor_frame's caller and render_normal_mode_frame -- only ever run
// between command-mode excursions, never during one). `text` is already
// padded/truncated to `term_cols` by the caller's own status-text
// function (fileeditor::status_text / normal_mode_status_text) -- this
// only adds positioning and styling.
pub(crate) fn render_global_status_row(text: &str, term_rows: usize) -> String {
    format!("\x1b[{};1H\x1b[7m{}\x1b[0m", command_mode_row(term_rows) + 1, text)
}

// A true erase of the same row `render_global_status_row` draws into --
// plain `\x1b[K`, not a reverse-video row of spaces (which would just be
// a highlighted *empty* bar sitting there, not actually gone). For a
// bar that's shown only sometimes within a single excursion (Ctrl-E's
// own line-local search input, editor.rs's `run_line_normal_mode` --
// unlike this row's other consumers, which are either always-on for the
// whole excursion or cleaned up by a coarser `compositor_redraw` at a
// handful of fixed transition points), calling this directly on every
// redraw where there's nothing to show is simpler and more precise than
// waiting for one of those coarser resets to happen to fire.
pub(crate) fn erase_global_status_row(term_rows: usize) -> String {
    format!("\x1b[{};1H\x1b[K", command_mode_row(term_rows) + 1)
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
    print!("{}", build_command_output_overlay(output, status, term_rows, term_cols));
    let _ = io::stdout().flush();
}

// render_command_output_overlay's own string-building half, split out
// purely for testability -- same "pure builder plus a thin print
// wrapper" shape build_compositor_frame_output/diff_frames already
// established.
fn build_command_output_overlay(output: &str, status: i32, term_rows: usize, term_cols: usize) -> String {
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
    // Deliberately does NOT blank rows 0..start_row: this overlay is
    // reused for both the plain scrollback view (nothing meaningful
    // above it) AND Frame::Edit's own real file content (very much
    // meaningful) -- the caller in the latter case (run_normal_mode_
    // navigation's own CommandModeOutcome::Ran arm) never repaints the
    // real buffer before showing an error/short-output overlay, so a
    // blanket wipe here used to erase the visible file to a blank
    // screen for however long the overlay stayed up. Any leftover text
    // from a *taller* previous overlay is already handled: the very
    // next keystroke resolves PendingView::Output with a real
    // render_nav_frame redraw before doing anything else (see
    // PendingView's own doc comment), so nothing stale can persist past
    // one keypress.
    for (i, line) in shown.iter().enumerate() {
        out.push_str(&format!("\x1b[{};1H", start_row + i + 1));
        out.push_str(&styled_full_width_line(line, bg, fg, term_cols));
    }
    out
}

// Shows one error message the exact same way a failed command's own
// output would (render_command_output_overlay, status != 0 -- its own
// error colors) -- what every "recognized but malformed input" arm
// inside run_command_mode's own loop uses now, instead of sink_err.
// Those arms stay in that same colon-line loop afterward (`buffer.
// clear(); continue;`) rather than returning to the caller, so they
// can't rely on the caller's own CommandModeOutcome::Ran-driven overlay
// call the way a *successful* command's output does -- and sink_err
// itself would silently go nowhere here: while a Frame::Edit pane's own
// file-editor content is focused, it's rendered by writing straight to
// the real terminal (render_editor_frame), bypassing the session's own
// vt100 Screen grid model entirely until it later loses focus, exactly
// where sink_err's Grid-sink path would otherwise land these bytes --
// invisibly, since nothing repaints from that model again before the
// next real keystroke arrives. A confirmed, previously-reported bug:
// every error this function used to report this way was completely
// silent in practice.
fn show_command_mode_error(msg: &str, term_rows: usize, term_cols: usize) {
    render_command_output_overlay(msg, 1, term_rows, term_cols);
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
    next_session_id: &mut SessionId,
    history: &mut History,
    job_frames: &mut HashMap<JobFrameId, exec::FgJob>,
    debug_frames: &mut HashMap<EditFrameId, debugger::DebugSession>,
    registers: &mut Registers,
    term_rows: &mut usize,
    term_cols: &mut usize,
    sinks_are_grid: bool,
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
    // Set from `seed` on the very first iteration, or by Ctrl+Space
    // below (see that arm's own comment) on any later one -- consumed by
    // the very next read_line call, then left None again either way.
    let mut pending_initial: Option<(String, usize)> = seed.map(|s| {
        let len = s.chars().count();
        (s, len)
    });
    loop {
        // Recomputed every iteration (not just once up front) so a
        // resize that arrives while this loop is blocked on a keystroke
        // (see service_background_jobs's own on_idle-driven WINCH
        // handling below) repositions this row correctly on the very
        // next redraw, instead of staying pinned to wherever the old
        // term_rows put it.
        let prompt_row = command_mode_row(*term_rows) + 1;
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
            *term_cols,
            HighlightContext::default(),
            None,
            None,
            false,
            None,
            registers,
            &[],
            // No global row of its own to draw into here -- this colon-
            // line already occupies that exact row with its own prompt
            // (see read_line's own doc comment on this param) -- so a
            // Ctrl-E search started while composing a `:` command falls
            // back to the original in-place prompt substitution.
            None,
            &mut || {
                service_background_jobs(sessions, windows, job_frames, current_window, term_rows, term_cols, sinks_are_grid);
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
            // Same reasoning as NormalMode just above: click-to-focus
            // isn't a thing while editing a command-mode colon-line
            // (you're not looking at any particular pane's content right
            // now to click on), so just preserve whatever was typed and
            // keep this same prompt going rather than acting on the
            // click's own target.
            Ok(ReadOutcome::Mouse { text, cursor, .. }) => {
                pending_initial = Some((text, cursor));
            }
            Ok(ReadOutcome::CtrlL) => {
                transcript_visible = !transcript_visible;
                if transcript_visible {
                    render_command_transcript(&sessions[&session_id].command_transcript, *term_rows, *term_cols);
                } else {
                    compositor_redraw(sessions, windows, current_window, *term_rows, *term_cols);
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
                            show_command_mode_error(&msg, *term_rows, *term_cols);
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
                    // `[range]s/pattern/replacement/[flags]`: checked
                    // before the ordinary space-split (cmd, arg) parsing
                    // just below, since this command's own syntax (an
                    // optional leading range, then `s` glued directly to
                    // its own delimiter -- no space at all) doesn't fit
                    // that shape. `parse_substitute_command` returns
                    // `None` -- not even a recognized attempt -- for
                    // anything else (`w`, `diag`, ...), letting this
                    // fall through to the ordinary dispatch unchanged.
                    if let Some(parsed) = parse_substitute_command(&trimmed) {
                        match parsed.and_then(|cmd| run_substitute(tb, &cmd)) {
                            Ok((subs, lines)) => {
                                let output =
                                    format!("{subs} substitution{} on {lines} line{}", if subs == 1 { "" } else { "s" }, if lines == 1 { "" } else { "s" });
                                sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                    command: trimmed,
                                    output: output.clone(),
                                    status: 0,
                                });
                                return CommandModeOutcome::Ran { output, status: 0 };
                            }
                            Err(e) => {
                                show_command_mode_error(&format!("bish: {e}"), *term_rows, *term_cols);
                                buffer.clear();
                                continue;
                            }
                        }
                    }

                    let (cmd, arg) = match trimmed.split_once(' ') {
                        Some((c, a)) => (c, Some(a.trim()).filter(|a| !a.is_empty())),
                        None => (trimmed.as_str(), None),
                    };
                    match cmd {
                        "w" | "write" => {
                            fileeditor::run_pre_save_hooks(tb);
                            match tb.save(arg.map(std::path::Path::new)) {
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
                                    show_command_mode_error(&format!("bish: E212: Can't open file for writing: {e}"), *term_rows, *term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            }
                        }
                        "wq" | "x" => {
                            fileeditor::run_pre_save_hooks(tb);
                            match tb.save(arg.map(std::path::Path::new)) {
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
                                    show_command_mode_error(&format!("bish: E212: Can't open file for writing: {e}"), *term_rows, *term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            }
                        }
                        "q" if tb.is_dirty() => {
                            show_command_mode_error("bish: E37: No write since last change (add ! to override)", *term_rows, *term_cols);
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
                            // Creates the collapsed diagnostics split
                            // (see split_diagnostics_pane's own doc
                            // comment) the first time this runs for this
                            // file, or just refreshes its title's count
                            // on every later run. `edit_frame_id`: this
                            // arm is only reachable while `editing` is
                            // `Some` (see this function's own `editing`
                            // param doc comment), which only happens for
                            // a `Frame::Edit` pane -- and that frame is
                            // still on top of this window's focused
                            // pane's own stack for the whole time this
                            // command-mode loop is driving it.
                            if let Some(Frame::Edit(edit_frame_id)) = windows[current_window].stack().last().copied() {
                                sync_diagnostics_pane(sessions, windows, current_window, next_session_id, edit_frame_id, &tb.diagnostics, *term_rows, *term_cols);
                            }
                            sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output, status: 0 });
                            // Empty `Ran` output (not the count message
                            // itself, still recorded above for `Ctrl-L`'s
                            // own transcript) -- unlike every other
                            // command here, the result of this one is
                            // already visible as the diagnostics pane's
                            // own persistent title bar. Returning the
                            // count as `Ran`'s own output would draw a
                            // second, redundant copy of it via
                            // `render_command_output_overlay`, which
                            // isn't pane-aware (it paints across the
                            // *whole* terminal's own bottom rows, not
                            // this pane's own shrunk rect) and would
                            // paint straight over the diagnostics pane's
                            // own divider/title the instant it ran.
                            return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                        }
                        // `diag clear`/`diagnose clear`: drops whatever
                        // `:diag` last found, same as it self-clears the
                        // instant a real edit would make its positions
                        // stale (see TextBuffer::diagnostics's own doc
                        // comment) -- this is just the explicit, no-edit
                        // version of that. Also closes the diagnostics
                        // split if `:diag` had created one, back to
                        // exactly the pre-`:diag` state rather than
                        // leaving a stale "0 problems" bar behind.
                        "diag" | "diagnose" if arg == Some("clear") => {
                            tb.diagnostics.clear();
                            if let Some(Frame::Edit(edit_frame_id)) = windows[current_window].stack().last().copied()
                                && let Some(pane_id) = diagnostics_sibling(&windows[current_window], edit_frame_id)
                            {
                                close_pane(&mut windows[current_window], pane_id);
                                close_orphaned_sessions(sessions, windows);
                            }
                            sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: String::new(), status: 0 });
                            return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                        }
                        "diag" | "diagnose" => {
                            show_command_mode_error(&format!("bish: diag: unknown subcommand '{}' (expected: clear)", arg.unwrap_or_default()), *term_rows, *term_cols);
                            buffer.clear();
                            continue;
                        }
                        // `diff` (bare, no `git` prefix): the same +/~/-
                        // gutter marker toggle `:git diff` uses, but
                        // answering a different question -- "what have I
                        // typed since I last saved" instead of "what's
                        // changed since the last commit" -- via a hand-
                        // rolled Myers diff (fileeditor::toggle_buffer_
                        // diff) against this buffer's own on-disk content,
                        // no git repository (or git at all) required.
                        // Shares the same buf.diff field/gutter rendering
                        // `:git diff` already populates, so the two are
                        // mutually exclusive toggle states.
                        "diff" if arg.is_none() => match fileeditor::toggle_buffer_diff(tb) {
                            Ok(on) => {
                                let output = if on { "Diff markers on.".to_string() } else { "Diff markers off.".to_string() };
                                sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                    command: trimmed,
                                    output: output.clone(),
                                    status: 0,
                                });
                                return CommandModeOutcome::Ran { output, status: 0 };
                            }
                            Err(e) => {
                                show_command_mode_error(&format!("bish: diff: {e}"), *term_rows, *term_cols);
                                buffer.clear();
                                continue;
                            }
                        },
                        "diff" => {
                            show_command_mode_error(&format!("bish: diff: unexpected argument '{}'", arg.unwrap_or_default()), *term_rows, *term_cols);
                            buffer.clear();
                            continue;
                        }
                        // `format`/`fmt`: the same per-filetype pre-save
                        // hook `w`/`wq`/`x` already run silently right
                        // before every save (fileeditor::run_pre_save_
                        // hooks), just triggerable by hand and with real
                        // feedback via the command-output overlay instead
                        // of running invisibly. No `clear` subcommand the
                        // way `diag` has one: there's no persistent
                        // state this leaves behind to clear, just an
                        // ordinary buffer edit `u` already undoes.
                        "format" | "fmt" if arg.is_none() => {
                            let (output, status) = match fileeditor::format_buffer(tb) {
                                fileeditor::FormatOutcome::Formatted => ("Reformatted.".to_string(), 0),
                                fileeditor::FormatOutcome::AlreadyFormatted => ("Already formatted.".to_string(), 0),
                                fileeditor::FormatOutcome::NotSupported => {
                                    show_command_mode_error("bish: format: no formatter for this filetype", *term_rows, *term_cols);
                                    buffer.clear();
                                    continue;
                                }
                                fileeditor::FormatOutcome::Error(e) => {
                                    show_command_mode_error(&format!("bish: format: {e}"), *term_rows, *term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            };
                            sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: output.clone(), status });
                            return CommandModeOutcome::Ran { output, status };
                        }
                        "format" | "fmt" => {
                            show_command_mode_error(&format!("bish: format: unexpected argument '{}'", arg.unwrap_or_default()), *term_rows, *term_cols);
                            buffer.clear();
                            continue;
                        }
                        // `dbg`/`debug <subcommand>`: everything the
                        // script debugger can do, nested under one
                        // command the same way `git`'s own subcommands
                        // are just below -- see debugger.rs's own top-of-
                        // file doc comment for the full shape (a real,
                        // read-only `Frame::Edit` pane for the source,
                        // plus a real `Frame::DebugRun` sibling that
                        // becomes focused while the script is actually
                        // running). Every arm below needs to know which
                        // `Frame::Edit` pane it's attaching to/detaching
                        // from -- always this same call's own pane (this
                        // whole `if let Some(tb) = ...` block only ever
                        // runs while that's true, same as `diag`'s own
                        // identical lookup just above).
                        "dbg" | "debug" => {
                            let Some(Frame::Edit(edit_frame_id)) = windows[current_window].stack().last().copied() else {
                                unreachable!("this whole match arm only runs while editing a real Frame::Edit pane")
                            };
                            let (subcmd, subarg) = match arg {
                                Some(a) => match a.split_once(' ') {
                                    Some((c, r)) => (c, Some(r.trim()).filter(|r| !r.is_empty())),
                                    None => (a, None),
                                },
                                None => ("", None),
                            };
                            match subcmd {
                                // Bare `:dbg`: attach. Refuses on a dirty
                                // buffer for the same reason the original
                                // standalone debugger did -- it reads the
                                // file fresh off disk (DebugSession::
                                // attach), not this buffer's own in-
                                // memory content, so an unsaved edit
                                // would silently debug something other
                                // than what's on screen.
                                "" if subarg.is_none() => {
                                    if debug_frames.contains_key(&edit_frame_id) {
                                        let output = "already attached -- :dbg run to start, :dbg quit to detach".to_string();
                                        sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: output.clone(), status: 0 });
                                        return CommandModeOutcome::Ran { output, status: 0 };
                                    }
                                    if tb.is_dirty() {
                                        show_command_mode_error("bish: dbg: E37: no write since last change -- save first with :w", *term_rows, *term_cols);
                                        buffer.clear();
                                        continue;
                                    }
                                    let Some(path) = tb.path().map(|p| p.to_path_buf()) else {
                                        show_command_mode_error("bish: dbg: no file name", *term_rows, *term_cols);
                                        buffer.clear();
                                        continue;
                                    };
                                    let session = match debugger::DebugSession::attach(&path) {
                                        Ok(s) => s,
                                        Err(e) => {
                                            show_command_mode_error(&format!("bish: dbg: {}: {e}", path.display()), *term_rows, *term_cols);
                                            buffer.clear();
                                            continue;
                                        }
                                    };
                                    debug_frames.insert(edit_frame_id, session);
                                    tb.set_readonly(true);
                                    let pane_id = debug_run_sibling(&windows[current_window], edit_frame_id)
                                        .unwrap_or_else(|| split_debug_run_pane(sessions, windows, current_window, next_session_id, edit_frame_id, *term_rows, *term_cols).0);
                                    let sid = windows[current_window].pane(pane_id).owning_session();
                                    render_debug_run_title(&sessions[&sid].screen, *term_cols, "attached -- :dbg run to start");
                                    compositor_redraw(sessions, windows, current_window, *term_rows, *term_cols);
                                    let output = "attached -- read-only until :dbg quit; :dbg run to start, :dbg break [line] to set a breakpoint".to_string();
                                    sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: output.clone(), status: 0 });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                // `:dbg run`: the only thing that ever
                                // starts a fresh execution (Shell::
                                // run_source_here, blocking -- a paused
                                // breakpoint blocks in place inside it,
                                // see debugger.rs's own PauseState). A
                                // *nested* RawGuard, same reasoning the
                                // original standalone debugger had for
                                // its own guard (term::RawGuard::
                                // suspend_raw/resume_raw were specifically
                                // fixed this session to derive fresh from
                                // the live termios state rather than a
                                // stored snapshot, exactly so nesting
                                // like this is correct) -- its Drop turns
                                // real mouse reporting back off, restored
                                // explicitly below for the same reason
                                // the original code did.
                                "run" if subarg.is_none() => {
                                    let Some(session) = debug_frames.get_mut(&edit_frame_id) else {
                                        show_command_mode_error("bish: dbg: not attached -- use :dbg to attach", *term_rows, *term_cols);
                                        buffer.clear();
                                        continue;
                                    };
                                    let Some(path) = tb.path().map(|p| p.to_path_buf()) else {
                                        show_command_mode_error("bish: dbg: no file name", *term_rows, *term_cols);
                                        buffer.clear();
                                        continue;
                                    };
                                    let pane_id = debug_run_sibling(&windows[current_window], edit_frame_id)
                                        .unwrap_or_else(|| split_debug_run_pane(sessions, windows, current_window, next_session_id, edit_frame_id, *term_rows, *term_cols).0);
                                    let budget = editor_pane_for(&windows[current_window], edit_frame_id)
                                        .map(|id| pane_rect(&windows[current_window], id, *term_rows, *term_cols).rows + 1)
                                        .unwrap_or(*term_rows);
                                    let rows = (budget / 2).max(6).min(budget.saturating_sub(3).max(1));
                                    if let Some((_, children, idx)) = find_parent_split_mut(&mut windows[current_window].layout, pane_id) {
                                        children[idx].minimized = false;
                                        children[idx].fixed = Some(rows);
                                    }
                                    windows[current_window].focused_pane = pane_id;
                                    compositor_redraw(sessions, windows, current_window, *term_rows, *term_cols);
                                    let run_rect = pane_rect(&windows[current_window], pane_id, *term_rows, *term_cols);
                                    // The real editor pane's own rect --
                                    // a pause paints straight into this
                                    // (see debugger.rs's own top-of-file
                                    // doc comment), not `run_rect`, which
                                    // stays reserved for the script's own
                                    // live/handed-off output.
                                    let Some(editor_pane) = editor_pane_for(&windows[current_window], edit_frame_id) else {
                                        unreachable!("this whole match arm only runs while editing a real Frame::Edit pane")
                                    };
                                    let editor_rect = pane_rect(&windows[current_window], editor_pane, *term_rows, *term_cols);

                                    let (quit_requested, nav_cursor) = match term::RawGuard::enable_with_mouse(0) {
                                        Ok(guard) => {
                                            let hook = match debugger::PauseState::new(&path, tb.breakpoints.clone(), run_rect, editor_rect, *term_rows, *term_cols, Rc::new(guard)) {
                                                Ok(h) => Rc::new(RefCell::new(h)),
                                                Err(e) => {
                                                    show_command_mode_error(&format!("bish: dbg: {}: {e}", path.display()), *term_rows, *term_cols);
                                                    buffer.clear();
                                                    continue;
                                                }
                                            };
                                            let src = session.source().to_string();
                                            session.shell.set_debug_hook(Some(hook.clone() as Rc<RefCell<dyn DebugHook>>));
                                            session.shell.run_source_here(&src, &trimmed);
                                            session.shell.set_debug_hook(None);
                                            print!("{}", term::MOUSE_REPORTING_ENABLE);
                                            let _ = io::stdout().flush();
                                            (hook.borrow().quit_requested(), hook.borrow().nav_cursor())
                                        }
                                        Err(_) => {
                                            show_command_mode_error("bish: dbg: not a terminal", *term_rows, *term_cols);
                                            (false, None)
                                        }
                                    };

                                    if let Some((_, children, idx)) = find_parent_split_mut(&mut windows[current_window].layout, pane_id) {
                                        children[idx].minimized = true;
                                        children[idx].fixed = None;
                                    }
                                    windows[current_window].focused_pane = editor_pane;
                                    // A pause navigated `tb`'s own stand-
                                    // in copy around (debugger.rs's own
                                    // `nav_buf`) -- carry that cursor
                                    // over to the real buffer so leaving
                                    // a run this way feels like leaving
                                    // any other pane and coming back.
                                    if let Some((row, col)) = nav_cursor {
                                        tb.set_cursor(row, col);
                                    }
                                    let status = if quit_requested {
                                        tb.set_readonly(false);
                                        debug_frames.remove(&edit_frame_id);
                                        close_pane(&mut windows[current_window], pane_id);
                                        close_orphaned_sessions(sessions, windows);
                                        "quit"
                                    } else {
                                        let sid = windows[current_window].pane(pane_id).owning_session();
                                        render_debug_run_title(&sessions[&sid].screen, *term_cols, "run finished -- :dbg run to run again");
                                        "run finished"
                                    };
                                    compositor_redraw(sessions, windows, current_window, *term_rows, *term_cols);
                                    let output = status.to_string();
                                    sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: output.clone(), status: 0 });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                // `:dbg continue`/`next`/`step`: only ever
                                // meaningful *while* genuinely paused,
                                // which can only happen deep inside the
                                // blocking `:dbg run` call just above --
                                // there's no way to reach this arm from
                                // there (the whole process is blocked in
                                // PauseState::on_statement's own loop
                                // instead, which recognizes this same
                                // vocabulary directly -- see debugger.rs's
                                // own top-of-file doc comment). Kept here,
                                // sharing the name, purely for an honest
                                // error instead of "unknown subcommand".
                                "continue" | "next" | "step" if subarg.is_none() => {
                                    show_command_mode_error("bish: dbg: not paused -- these only work while stopped at a breakpoint", *term_rows, *term_cols);
                                    buffer.clear();
                                    continue;
                                }
                                // `:dbg break [line]` / `:dbg break add|remove N`.
                                "break" => {
                                    if !debug_frames.contains_key(&edit_frame_id) {
                                        show_command_mode_error("bish: dbg: not attached -- use :dbg to attach", *term_rows, *term_cols);
                                        buffer.clear();
                                        continue;
                                    }
                                    let output = match subarg {
                                        None => {
                                            let line = tb.cursor().0 + 1;
                                            if !tb.breakpoints.insert(line) {
                                                tb.breakpoints.remove(&line);
                                            }
                                            format!("breakpoint toggled at line {line}")
                                        }
                                        Some(rest) => {
                                            let (op, num) = match rest.split_once(' ') {
                                                Some((o, n)) => (o, Some(n.trim())),
                                                None => (rest, None),
                                            };
                                            match (op, num) {
                                                ("add", Some(n)) => match n.parse::<usize>() {
                                                    Ok(n) => {
                                                        tb.breakpoints.insert(n);
                                                        format!("breakpoint added at line {n}")
                                                    }
                                                    Err(_) => {
                                                        show_command_mode_error(&format!("bish: dbg: {n}: invalid line number"), *term_rows, *term_cols);
                                                        buffer.clear();
                                                        continue;
                                                    }
                                                },
                                                ("remove", Some(n)) => match n.parse::<usize>() {
                                                    Ok(n) => {
                                                        tb.breakpoints.remove(&n);
                                                        format!("breakpoint removed at line {n}")
                                                    }
                                                    Err(_) => {
                                                        show_command_mode_error(&format!("bish: dbg: {n}: invalid line number"), *term_rows, *term_cols);
                                                        buffer.clear();
                                                        continue;
                                                    }
                                                },
                                                _ => match rest.trim().parse::<usize>() {
                                                    Ok(n) => {
                                                        if !tb.breakpoints.insert(n) {
                                                            tb.breakpoints.remove(&n);
                                                        }
                                                        format!("breakpoint toggled at line {n}")
                                                    }
                                                    Err(_) => {
                                                        show_command_mode_error(&format!("bish: dbg: {rest}: invalid line number (expected: [line], add N, remove N)"), *term_rows, *term_cols);
                                                        buffer.clear();
                                                        continue;
                                                    }
                                                },
                                            }
                                        }
                                    };
                                    sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: output.clone(), status: 0 });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                // `:dbg print NAME`.
                                "print" | "p" => {
                                    let Some(session) = debug_frames.get(&edit_frame_id) else {
                                        show_command_mode_error("bish: dbg: not attached -- use :dbg to attach", *term_rows, *term_cols);
                                        buffer.clear();
                                        continue;
                                    };
                                    let Some(name) = subarg else {
                                        show_command_mode_error("bish: dbg: usage: dbg print NAME", *term_rows, *term_cols);
                                        buffer.clear();
                                        continue;
                                    };
                                    let output = match session.peek_var(name) {
                                        Some(v) => format!("{name} = {v}"),
                                        None => format!("{name}: unset or not inspectable"),
                                    };
                                    sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: output.clone(), status: 0 });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                // `:dbg quit`: detach -- writable again,
                                // drops the debug_frames entry, closes
                                // the DebugRun sibling if one exists.
                                "quit" | "q" if subarg.is_none() => {
                                    if debug_frames.remove(&edit_frame_id).is_some() {
                                        tb.set_readonly(false);
                                        if let Some(pane_id) = debug_run_sibling(&windows[current_window], edit_frame_id) {
                                            close_pane(&mut windows[current_window], pane_id);
                                            close_orphaned_sessions(sessions, windows);
                                        }
                                        compositor_redraw(sessions, windows, current_window, *term_rows, *term_cols);
                                    }
                                    sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: String::new(), status: 0 });
                                    return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                                }
                                "help" | "h" | "?" if subarg.is_none() => {
                                    sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: DBG_HELP_TEXT.to_string(), status: 0 });
                                    return CommandModeOutcome::Ran { output: DBG_HELP_TEXT.to_string(), status: 0 };
                                }
                                _ => {
                                    show_command_mode_error(
                                        &format!("bish: dbg: unknown subcommand '{subcmd}' (expected: run, break, continue, next, step, print, quit, help)"),
                                        *term_rows,
                                        *term_cols,
                                    );
                                    buffer.clear();
                                    continue;
                                }
                            }
                        }
                        // `help`/`h`/`?`: a single-screen quick reference
                        // for this editor's own motions/operators/colon
                        // commands -- real vim's own `:help` opens a full
                        // multi-page manual in a split buffer; this is
                        // deliberately much smaller in scope (one static
                        // screen, no topics/tags/search of its own),
                        // matching this codebase's usual "practical
                        // subset" shape rather than building a whole help
                        // system for a one-off ask. `?` is a real,
                        // separate alias (not a typo-tolerant fuzzy
                        // match) -- unlike real vim, where `?` in command
                        // mode means "search backward" (a leftover from
                        // ed-style line editors), bish's own colon-line
                        // has no such meaning for a bare `?` at all (real
                        // `/`/`?` search here is a Normal-mode motion via
                        // vimkeys, entirely separate from this colon-line
                        // -- see vimkeys.rs), so reusing it as a `-h`/
                        // `--help`-style shorthand doesn't collide with
                        // anything. Shown via the same command-output
                        // overlay every other command's own output
                        // already uses (`CommandModeOutcome::Ran`) --
                        // free truncation-with-a-count-notice for a
                        // shorter terminal, and the real content behind
                        // it stays visible per the screen-wipe fix above.
                        "help" | "h" | "?" if arg.is_none() => {
                            sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                command: trimmed,
                                output: EDITOR_HELP_TEXT.to_string(),
                                status: 0,
                            });
                            return CommandModeOutcome::Ran { output: EDITOR_HELP_TEXT.to_string(), status: 0 };
                        }
                        "help" | "h" | "?" => {
                            show_command_mode_error(&format!("bish: help: unexpected argument '{}' (no help topics yet -- just `:help`)", arg.unwrap_or_default()), *term_rows, *term_cols);
                            buffer.clear();
                            continue;
                        }
                        // `git SUBCOMMAND...`: the editor's own git
                        // integration, entirely optional (see crate::git's
                        // own module doc comment) -- shells out to the
                        // real `git` executable rather than bish
                        // implementing any of it itself, so a missing
                        // `git` on $PATH just means these features don't
                        // work, checked once up front regardless of which
                        // subcommand was actually typed. `blame`/`diff`
                        // (neither takes a flag/subcommand of its own
                        // yet) each toggle their own gutter column via
                        // fileeditor::toggle_git_blame/toggle_git_diff,
                        // whose own Ok(bool)/Err(String) already say
                        // exactly what happened and why not.
                        "git" => {
                            if !crate::git::available() {
                                show_command_mode_error("bish: git: git executable not found", *term_rows, *term_cols);
                                buffer.clear();
                                continue;
                            }
                            let (subcmd, subarg) = match arg {
                                Some(a) => match a.split_once(' ') {
                                    Some((c, r)) => (c, Some(r.trim()).filter(|r| !r.is_empty())),
                                    None => (a, None),
                                },
                                None => {
                                    show_command_mode_error("bish: git: missing subcommand (expected: blame, diff)", *term_rows, *term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            };
                            match subcmd {
                                "blame" if subarg.is_none() => match fileeditor::toggle_git_blame(tb) {
                                    Ok(on) => {
                                        let output = if on { "Blame on.".to_string() } else { "Blame off.".to_string() };
                                        sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                            command: trimmed,
                                            output: output.clone(),
                                            status: 0,
                                        });
                                        return CommandModeOutcome::Ran { output, status: 0 };
                                    }
                                    Err(e) => {
                                        show_command_mode_error(&format!("bish: git: blame: {e}"), *term_rows, *term_cols);
                                        buffer.clear();
                                        continue;
                                    }
                                },
                                "blame" => {
                                    show_command_mode_error(
                                        &format!("bish: git: blame: unsupported argument '{}' (only a bare `git blame` toggle is supported for now)", subarg.unwrap_or_default()),
                                        *term_rows,
                                        *term_cols,
                                    );
                                    buffer.clear();
                                    continue;
                                }
                                // `diff` (no further flags/subcommand of
                                // its own yet): same toggle shape as
                                // `blame` above, just against
                                // fileeditor::toggle_git_diff -- gutter
                                // +/~/- markers for lines added/changed/
                                // removed relative to this file's tracked
                                // state instead of per-line authorship.
                                "diff" if subarg.is_none() => match fileeditor::toggle_git_diff(tb) {
                                    Ok(on) => {
                                        let output = if on { "Diff markers on.".to_string() } else { "Diff markers off.".to_string() };
                                        sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                            command: trimmed,
                                            output: output.clone(),
                                            status: 0,
                                        });
                                        return CommandModeOutcome::Ran { output, status: 0 };
                                    }
                                    Err(e) => {
                                        show_command_mode_error(&format!("bish: git: diff: {e}"), *term_rows, *term_cols);
                                        buffer.clear();
                                        continue;
                                    }
                                },
                                "diff" => {
                                    show_command_mode_error(
                                        &format!("bish: git: diff: unsupported argument '{}' (only a bare `git diff` toggle is supported for now)", subarg.unwrap_or_default()),
                                        *term_rows,
                                        *term_cols,
                                    );
                                    buffer.clear();
                                    continue;
                                }
                                other => {
                                    show_command_mode_error(&format!("bish: git: unknown subcommand '{}' (expected: blame, diff)", other), *term_rows, *term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            }
                        }
                        _ => {}
                    }
                }

                // `browse [path]`: the file browser, drawn into whichever
                // pane was focused when this colon line was opened.
                // Deliberately *outside* the `editing.is_some()` block
                // above -- unlike `w`/`diag`/`git blame`, which only mean
                // anything against a real file buffer, browsing is about
                // the pane and its session's own cwd, so it works from a
                // shell pane's Ctrl+Space excursion exactly as it does
                // from an editor pane. Handled here rather than as a
                // `Shell` builtin for the same reason `e`/`fg` can't be
                // ones either (see this function's own `ExecResult::Edit`
                // arm below): a builtin has no raw-mode, keystroke or
                // pane-rect access at all.
                let browse_arg = {
                    let (cmd, arg) = match trimmed.split_once(' ') {
                        Some((c, a)) => (c, Some(a.trim()).filter(|a| !a.is_empty())),
                        None => (trimmed.as_str(), None),
                    };
                    if cmd == "browse" || cmd == "br" { Some(arg.map(|a| a.to_string())) } else { None }
                };
                if let Some(arg) = browse_arg {
                    let start = browser::resolve_start(&sessions[&session_id].shell.cwd, arg.as_deref());
                    match run_browse_frame(&start, sessions, windows, current_window, job_frames, term_rows, term_cols, sinks_are_grid) {
                        // What to actually *do* with the chosen paths is
                        // a later pass (see browser.rs's own scope note)
                        // -- for now they come back as the command's own
                        // output, which the caller already shows via
                        // render_command_output_overlay. Cancelling
                        // (Esc/`q`) produces no output at all, so the
                        // pane just goes back to what it was showing.
                        Ok(chosen) => {
                            let output = match chosen {
                                None => String::new(),
                                Some(paths) if paths.is_empty() => String::new(),
                                Some(paths) => paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n"),
                            };
                            sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: output.clone(), status: 0 });
                            return CommandModeOutcome::Ran { output, status: 0 };
                        }
                        Err(e) => {
                            show_command_mode_error(&format!("bish: browse: {e}"), *term_rows, *term_cols);
                            buffer.clear();
                            continue;
                        }
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
                                show_command_mode_error(&format!("bish: {}", msg), *term_rows, *term_cols);
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
                                    // See the other run_program call site's
                                    // own identical comment (repl.rs's main
                                    // loop) for why this pair is needed.
                                    session.shell.sync_real_state_in();
                                    let result = session.shell.run_program(&prog);
                                    session.shell.sync_real_state_out();
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
                                    // stashed and never driven. Returns
                                    // `Ran{status: 1}` (not `Cancelled`,
                                    // which stays silent) so the message
                                    // actually reaches
                                    // render_command_output_overlay via
                                    // the caller's own `Ran` handling --
                                    // `Cancelled`'s own arm there never
                                    // shows anything, and a direct paint
                                    // from in here would just get clobbered
                                    // by handle_command_mode's own
                                    // unconditional post-return
                                    // compositor_redraw before the caller
                                    // even sees this outcome.
                                    ExecResult::Fg => {
                                        sessions.get_mut(&session_id).unwrap().shell.discard_pending_fg();
                                        let output = "bish: fg: not supported in command mode -- use it from the normal shell prompt".to_string();
                                        sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: output.clone(), status: 1 });
                                        return CommandModeOutcome::Ran { output, status: 1 };
                                    }
                                    // Same reasoning as Fg just above --
                                    // this restricted read-eval loop has
                                    // no way to drive an interactive
                                    // editor session either.
                                    ExecResult::Edit => {
                                        sessions.get_mut(&session_id).unwrap().shell.take_pending_edit();
                                        let output = "bish: e: not supported in command mode -- use it from the normal shell prompt".to_string();
                                        sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry { command: trimmed, output: output.clone(), status: 1 });
                                        return CommandModeOutcome::Ran { output, status: 1 };
                                    }
                                    // Matches this codebase's prior behavior
                                    // for `exit`/`set -e`/`set -u` (see the
                                    // other run_program call site's own
                                    // ExecResult::Exit arm above) -- the
                                    // exit trap already ran wherever this
                                    // was produced.
                                    ExecResult::Exit(code) => std::process::exit(code),
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
                                show_command_mode_error(&format!("bish: syntax error: {}", e), *term_rows, *term_cols);
                                buffer.clear();
                            }
                        }
                    },
                    Err(e) => {
                        if !is_incomplete(&e) {
                            show_command_mode_error(&format!("bish: syntax error: {}", e), *term_rows, *term_cols);
                            buffer.clear();
                        }
                    }
                }
            }
            // Same "Ran{status: 1}, not Cancelled" reasoning as the
            // Fg/Edit arms above -- a genuine stdin read failure while
            // composing the colon-line itself, before there was ever a
            // real command to record to the transcript.
            Err(e) => {
                return CommandModeOutcome::Ran { output: format!("bish: error reading input: {}", e), status: 1 };
            }
        }
    }
}

// `[range]s/pattern/replacement/[flags]` -- vim's own Ex substitute
// command. One line ref, either endpoint of a `LineRef` pair below.
#[derive(Clone, Copy, Debug, PartialEq)]
enum LineRef {
    Current,
    Last,
    Number(usize),
}

impl LineRef {
    // 0-indexed row, clamped to this buffer's own current bounds --
    // `Number` is vim's own 1-indexed convention (`:1` is the first
    // line), so it needs the -1; `Current`/`Last` are already resolved
    // against this exact buffer.
    fn resolve(self, current: usize, last: usize) -> usize {
        match self {
            LineRef::Current => current,
            LineRef::Last => last,
            LineRef::Number(n) => n.saturating_sub(1).min(last),
        }
    }
}

#[derive(Debug)]
struct SubstituteCmd {
    from: LineRef,
    to: LineRef,
    pattern: String,
    replacement: String,
    global: bool,
}

fn parse_one_line_ref(chars: &[char], i: &mut usize) -> Option<LineRef> {
    match chars.get(*i) {
        Some('.') => {
            *i += 1;
            Some(LineRef::Current)
        }
        Some('$') => {
            *i += 1;
            Some(LineRef::Last)
        }
        Some(c) if c.is_ascii_digit() => {
            let start = *i;
            while chars.get(*i).is_some_and(|c| c.is_ascii_digit()) {
                *i += 1;
            }
            let n: usize = chars[start..*i].iter().collect::<String>().parse().ok()?;
            Some(LineRef::Number(n))
        }
        _ => None,
    }
}

// `%` (whole buffer, shorthand for `1,$`), a single ref (`.`/`$`/`N`,
// meaning that same line for both ends), or `ref,ref` -- `None` (not an
// error) whenever nothing recognizable starts right at `*i`, so the
// caller knows no explicit range was given at all (defaults to the
// current line only, same as bare vim `:s` with no range). Deliberately
// doesn't support `'<,'>` (visual-mode marks), `;` (range separator that
// also moves the cursor), or relative `+N`/`-N` offsets -- see plan.md's
// own note on this feature's scope.
fn parse_range_prefix(chars: &[char], i: &mut usize) -> Option<(LineRef, LineRef)> {
    if chars.get(*i) == Some(&'%') {
        *i += 1;
        return Some((LineRef::Number(1), LineRef::Last));
    }
    let from = parse_one_line_ref(chars, i)?;
    if chars.get(*i) == Some(&',') {
        *i += 1;
        let to = parse_one_line_ref(chars, i).unwrap_or(from);
        Some((from, to))
    } else {
        Some((from, from))
    }
}

// Scans from `*i` up to the next unescaped `delim`, unescaping `\<delim>`
// back to a literal `delim` along the way (vim's own rule: the delimiter
// can appear literally in the pattern/replacement if backslash-escaped).
// Reaching the end of input without ever finding a bare `delim` isn't an
// error -- the *final* delimiter of a `:s` command is optional in vim
// (`:s/foo/bar` with no trailing `/` at all is valid, same as with one)
// -- so this just returns everything scanned either way.
fn scan_until_delim(chars: &[char], i: &mut usize, delim: char) -> String {
    let mut out = String::new();
    while let Some(&c) = chars.get(*i) {
        if c == delim {
            *i += 1;
            return out;
        }
        if c == '\\' && chars.get(*i + 1) == Some(&delim) {
            out.push(delim);
            *i += 2;
            continue;
        }
        out.push(c);
        *i += 1;
    }
    out
}

// `None`: `trimmed` doesn't even look like `[range]s...` at all (no `s`
// right after wherever the optional range parsed to), so the caller
// should fall through to the ordinary space-split command dispatch
// unchanged -- this never claims a command name it doesn't fully own.
// `Some(Err(_))`: recognizable as an attempted substitute but malformed
// (no pattern given at all, e.g. bare `:s` or `:s/`). `Some(Ok(_))`: a
// complete, ready-to-run command.
fn parse_substitute_command(trimmed: &str) -> Option<Result<SubstituteCmd, String>> {
    let chars: Vec<char> = trimmed.chars().collect();
    let mut i = 0;
    let (from, to) = parse_range_prefix(&chars, &mut i).unwrap_or((LineRef::Current, LineRef::Current));

    if chars.get(i) != Some(&'s') {
        return None;
    }
    i += 1;
    // The delimiter itself: any punctuation works in real vim (`s#..#..#`,
    // `s,..,..,`, ...), not just `/` -- excluding letters/digits/`\\`
    // keeps this from misfiring on an ordinary word starting with `s`
    // (`set`, `sort`, ...) that isn't this command at all.
    let delim = match chars.get(i) {
        Some(&c) if !c.is_alphanumeric() && c != '\\' => c,
        _ => return None,
    };
    i += 1;

    let pattern = scan_until_delim(&chars, &mut i, delim);
    if pattern.is_empty() {
        return Some(Err("E35: no previous regular expression".to_string()));
    }
    let replacement = scan_until_delim(&chars, &mut i, delim);
    let flags: String = chars[i..].iter().collect();
    Some(Ok(SubstituteCmd { from, to, pattern, replacement, global: flags.contains('g') }))
}

// Applies `once` (the very next appended char only, `\u`/`\l`) or,
// failing that, `sticky` (every appended char, `\U`/`\L`, until `\E`/
// `\e` or the end of the replacement) to each char of `s` as it's
// pushed -- shared by every expand_replacement call site that appends
// more than a single literal character (a backreference or `&`) so a
// case modifier immediately preceding one still applies to the whole
// substituted span, not just its first char.
fn push_transformed(out: &mut String, s: &str, once: &mut Option<bool>, sticky: &mut Option<bool>) {
    for c in s.chars() {
        let transformed = if let Some(upper) = once.take() {
            if upper { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() }
        } else if let Some(upper) = *sticky {
            if upper { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() }
        } else {
            c
        };
        out.push(transformed);
    }
}

// Expands a `:s` replacement string against one match's own captures
// (`caps[0]` is the whole match, `caps[1..]` are groups 1.. -- same
// shape crate::regex::Regex::find_at_with_captures returns) -- vim's own
// replacement escapes: `&`/`\0` (whole match), `\1`-`\9` (group N,
// silently empty if that group didn't participate or doesn't exist),
// `\\` (literal backslash), `\&` (literal &), `\r` (a real newline,
// splitting the line -- TextBuffer::insert_text already understands an
// embedded '\n'), `\u`/`\l`/`\U`/`\L`/`\e`/`\E` (case modifiers, see
// push_transformed). `\=` (the expression register) is deliberately not
// supported -- see plan.md's own note on this feature's scope.
fn expand_replacement(replacement: &str, caps: &[String]) -> String {
    let chars: Vec<char> = replacement.chars().collect();
    let mut out = String::new();
    let mut once: Option<bool> = None;
    let mut sticky: Option<bool> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let n = chars[i + 1];
            i += 2;
            match n {
                '0'..='9' => {
                    let idx = n.to_digit(10).unwrap() as usize;
                    let s = caps.get(idx).map(String::as_str).unwrap_or("");
                    push_transformed(&mut out, s, &mut once, &mut sticky);
                }
                '\\' => push_transformed(&mut out, "\\", &mut once, &mut sticky),
                '&' => push_transformed(&mut out, "&", &mut once, &mut sticky),
                'r' => out.push('\n'),
                'u' => once = Some(true),
                'l' => once = Some(false),
                'U' => sticky = Some(true),
                'L' => sticky = Some(false),
                'e' | 'E' => sticky = None,
                other => push_transformed(&mut out, &other.to_string(), &mut once, &mut sticky),
            }
        } else if c == '&' {
            let s = caps.first().map(String::as_str).unwrap_or("");
            push_transformed(&mut out, s, &mut once, &mut sticky);
            i += 1;
        } else {
            push_transformed(&mut out, &c.to_string(), &mut once, &mut sticky);
            i += 1;
        }
    }
    out
}

// One line's own worth of substitution -- every match if `global`,
// otherwise just the first. Returns the line's new text (which may
// itself now contain embedded newlines, from a `\r` in the replacement)
// and how many matches were actually replaced. A zero-width match (e.g.
// `s/x*/Y/g` against a line with no `x` at all) still advances by one
// character afterward -- copying that character through unchanged --
// rather than looping forever re-matching the same empty span.
//
// Real vim's own `g` loop never tries a match starting at exactly the
// line's own length (one past the last char) -- confirmed against real
// vim (`vim -es -c '%s/x*/-/g'`): "ab" -> "-a-b" (not "-a-b-"), "abb"
// against `b*` -> "-a-" (the real, non-zero-width "bb" match already
// consumes to the end, and nothing further is tried there either) -- the
// *one* exception is a genuinely empty line, where position 0 (which
// also happens to equal the line's own length) is the only position
// there is, and vim does substitute once there (`""` -> `"-"`). Handled
// as its own upfront special case below rather than folding it into the
// general loop's own bound, since "try position 0" and "never try
// position == len" would otherwise directly contradict each other for
// that one case.
fn substitute_line(text: &str, re: &crate::regex::Regex, replacement: &str, global: bool) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return match re.find_at_with_captures(&chars, 0) {
            Some((_, _, caps)) => (expand_replacement(replacement, &caps), 1),
            None => (String::new(), 0),
        };
    }
    let mut out = String::new();
    let mut pos = 0;
    let mut count = 0;
    while pos < chars.len() {
        match re.find_at_with_captures(&chars, pos) {
            None => {
                out.extend(&chars[pos..]);
                break;
            }
            Some((start, end, caps)) => {
                out.extend(&chars[pos..start]);
                out.push_str(&expand_replacement(replacement, &caps));
                count += 1;
                if !global {
                    out.extend(&chars[end..]);
                    break;
                }
                if end == start {
                    if end < chars.len() {
                        out.push(chars[end]);
                    }
                    pos = end + 1;
                } else {
                    pos = end;
                }
            }
        }
    }
    (out, count)
}

// `:s`'s own worker: runs `cmd` against `tb`, returning (total
// substitutions, lines actually changed) on success. `Err` is vim's own
// "E486: Pattern not found" message when the range had at least one
// line to search but nothing in it ever matched -- real vim's own exact
// wording, since this is meant to read like a familiar Ex error, not a
// bish-specific one. Processes a changed line by replacing its *entire*
// own content in place (delete_range then insert_text, the same
// splice-in-place TextBuffer::put_over_selections already uses) rather
// than trying to patch around just the matched span(s) -- simplest
// correct way to let a `\r` in the replacement split a line into several
// without this loop needing to special-case that itself; every
// downstream line in the range is then addressed by its own shifted row
// (`+= 1 + however many extra lines this one's own substitution added`)
// so a growing range is still walked correctly to its (also shifting)
// end.
fn run_substitute(tb: &mut TextBuffer, cmd: &SubstituteCmd) -> Result<(usize, usize), String> {
    let last = tb.line_count().saturating_sub(1);
    let current = tb.cursor().0;
    let a = cmd.from.resolve(current, last);
    let b = cmd.to.resolve(current, last);
    // A "backwards" range (e.g. `:5,2s/.../...`) is silently normalized
    // rather than rejected -- real vim asks for confirmation
    // (E493/"Backwards range given, OK to swap") since there's no
    // interactive prompt plumbed through command mode for that; just
    // swapping is the more useful default and matches what confirming
    // that prompt would do anyway.
    let mut row = a.min(b);
    let mut end = a.max(b);
    let re = crate::regex::Regex::compile(&cmd.pattern);
    let mut total = 0usize;
    let mut lines_changed = 0usize;
    while row <= end && row < tb.line_count() {
        let original: String = tb.line_chars(row).into_iter().collect();
        let (new_text, count) = substitute_line(&original, &re, &cmd.replacement, cmd.global);
        if count > 0 {
            total += count;
            lines_changed += 1;
            let line_len = original.chars().count();
            let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, 0), to: (row, line_len) };
            tb.delete_range(&range);
            tb.insert_text((row, 0), &new_text);
            let added = new_text.matches('\n').count();
            end += added;
            row += 1 + added;
        } else {
            row += 1;
        }
    }
    if total == 0 {
        return Err(format!("E486: Pattern not found: {}", cmd.pattern));
    }
    Ok((total, lines_changed))
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

#[cfg(test)]
mod pane_layout_tests {
    use super::*;

    fn regions(layout: &PaneLayout, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        let mut dividers = Vec::new();
        compute_regions(layout, area, &mut out, &mut dividers);
        out
    }

    #[test]
    fn a_fixed_child_gets_exactly_its_own_size_and_the_weighted_sibling_absorbs_the_rest() {
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: Some(1), minimized: false },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 10, cols: 20 };
        let out = regions(&layout, area);
        let r0 = out.iter().find(|(id, _)| *id == 0).unwrap().1;
        let r1 = out.iter().find(|(id, _)| *id == 1).unwrap().1;
        // 10 rows - 1 divider = 9 usable; the fixed child takes exactly
        // 1, the lone weighted child absorbs the remaining 8.
        assert_eq!(r1.rows, 1);
        assert_eq!(r0.rows, 8);
    }

    #[test]
    fn every_existing_split_still_divides_evenly_with_no_fixed_children() {
        // Byte-for-byte the pre-`fixed`-field formula: three even
        // weighted children in a 21-row area (20 usable after 2
        // dividers) split 7/7/6, the last absorbing the remainder.
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(2), weight: 1.0, fixed: None, minimized: false },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 22, cols: 20 };
        let out = regions(&layout, area);
        let rows_of = |id: PaneId| out.iter().find(|(i, _)| *i == id).unwrap().1.rows;
        assert_eq!(rows_of(0) + rows_of(1) + rows_of(2), 20);
        assert_eq!(rows_of(0), rows_of(1));
    }

    #[test]
    fn a_fixed_request_larger_than_the_area_is_clamped_so_the_weighted_sibling_still_gets_a_row() {
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: Some(10), minimized: false },
            ],
        };
        // 3 rows - 1 divider = 2 usable; a fixed request of 10 must be
        // clamped so the weighted sibling isn't squeezed to 0.
        let area = Rect { row: 0, col: 0, rows: 3, cols: 20 };
        let out = regions(&layout, area);
        let r0 = out.iter().find(|(id, _)| *id == 0).unwrap().1;
        let r1 = out.iter().find(|(id, _)| *id == 1).unwrap().1;
        assert_eq!(r0.rows, 1);
        assert_eq!(r1.rows, 1);
    }

    #[test]
    fn a_fixed_child_works_along_the_column_axis_of_a_vertical_split_too() {
        let layout = PaneLayout::Split {
            horizontal: false,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: Some(5), minimized: false },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 10, cols: 30 };
        let out = regions(&layout, area);
        let r1 = out.iter().find(|(id, _)| *id == 1).unwrap().1;
        assert_eq!(r1.cols, 5);
        assert_eq!(r1.rows, 10);
    }

    fn regions_and_dividers(layout: &PaneLayout, area: Rect) -> (Vec<(PaneId, Rect)>, Vec<(Rect, bool)>) {
        let mut out = Vec::new();
        let mut dividers = Vec::new();
        compute_regions(layout, area, &mut out, &mut dividers);
        (out, dividers)
    }

    #[test]
    fn a_minimized_child_takes_exactly_one_row_with_no_separate_divider_reserved() {
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: None, minimized: true },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 10, cols: 20 };
        let (out, dividers) = regions_and_dividers(&layout, area);
        let r0 = out.iter().find(|(id, _)| *id == 0).unwrap().1;
        let r1 = out.iter().find(|(id, _)| *id == 1).unwrap().1;
        // No divider at all between them: the minimized pane's own row
        // is the whole boundary. 0's own 9 rows + 1's own 1 row fill
        // the entire 10-row area exactly, unlike the ordinary
        // fixed-child case (which still reserves 1 row for a real
        // divider on top of the fixed child's own size).
        assert!(dividers.is_empty());
        assert_eq!(r1.rows, 1);
        assert_eq!(r0.rows, 9);
        assert_eq!(r0.row, 0);
        assert_eq!(r1.row, 9);
    }

    #[test]
    fn minimized_ignores_its_own_fixed_and_weight() {
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 5.0, fixed: Some(4), minimized: true },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 10, cols: 20 };
        let out = regions(&layout, area);
        let r1 = out.iter().find(|(id, _)| *id == 1).unwrap().1;
        assert_eq!(r1.rows, 1);
    }

    #[test]
    fn a_normal_divider_still_separates_two_ordinary_siblings_next_to_a_minimized_one() {
        // Three children: ordinary, ordinary, minimized -- the boundary
        // between the first two still gets a real divider; only the one
        // next to the minimized child is skipped.
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(2), weight: 1.0, fixed: None, minimized: true },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 11, cols: 20 };
        let (out, dividers) = regions_and_dividers(&layout, area);
        assert_eq!(dividers.len(), 1);
        let r2 = out.iter().find(|(id, _)| *id == 2).unwrap().1;
        assert_eq!(r2.rows, 1);
        // 11 rows - 1 real divider - 1 minimized row = 9 usable for the
        // two ordinary weighted siblings, split evenly.
        let rows_of = |id: PaneId| out.iter().find(|(i, _)| *i == id).unwrap().1.rows;
        assert_eq!(rows_of(0) + rows_of(1), 9);
    }
}

#[cfg(test)]
mod fg_click_tests {
    use super::*;

    #[test]
    fn decode_fg_click_recognizes_a_qualifying_left_click() {
        let ev = decode_fg_click(b"\x1b[<0;15;24M").unwrap();
        assert_eq!((ev.button, ev.col, ev.row, ev.pressed), (0, 15, 24, true));
    }

    #[test]
    fn decode_fg_click_ignores_a_release() {
        assert!(decode_fg_click(b"\x1b[<0;15;24m").is_none());
    }

    #[test]
    fn decode_fg_click_ignores_a_drag() {
        // Bit 5 (0x20) set means "motion while a button is held" -- not a
        // plain click (MouseEvent::is_left_click's own doc comment).
        assert!(decode_fg_click(b"\x1b[<32;15;24M").is_none());
    }

    #[test]
    fn decode_fg_click_ignores_a_wheel_event() {
        // Bit 6 (0x40) set means a wheel button.
        assert!(decode_fg_click(b"\x1b[<64;15;24M").is_none());
    }

    #[test]
    fn decode_fg_click_ignores_a_right_click() {
        assert!(decode_fg_click(b"\x1b[<2;15;24M").is_none());
    }

    #[test]
    fn decode_fg_click_ignores_a_non_mouse_sequence() {
        assert!(decode_fg_click(b"\x1b[A").is_none());
        assert!(decode_fg_click(b"hello").is_none());
    }

    #[test]
    fn decode_fg_click_ignores_an_incomplete_sequence() {
        assert!(decode_fg_click(b"\x1b[<0;15;24").is_none());
        assert!(decode_fg_click(b"\x1b[<").is_none());
    }

    #[test]
    fn decode_fg_click_decodes_the_first_report_even_with_a_paired_release_concatenated_after_it() {
        // A press and its own release commonly arrive in the very same
        // read over a local pty -- must not treat the whole buffer as
        // one garbled report (this exact case previously fooled the
        // detector into missing the click entirely, see this function's
        // own doc comment).
        let ev = decode_fg_click(b"\x1b[<0;15;24M\x1b[<0;15;24m").unwrap();
        assert_eq!((ev.button, ev.col, ev.row, ev.pressed), (0, 15, 24, true));
    }
}

#[cfg(test)]
mod compositor_diff_tests {
    use super::*;

    // A blank rows*cols frame with plain-space cells everywhere, cursor
    // parked at (0, 0) and visible -- the common starting point every
    // test below mutates one piece of at a time.
    fn blank(rows: usize, cols: usize) -> TerminalFrame {
        TerminalFrame { rows, cols, cells: vec![vt100::Cell::default(); rows * cols], tab_bar: String::new(), cursor: (0, 0), cursor_visible: true }
    }

    fn set(frame: &mut TerminalFrame, row: usize, col: usize, ch: char) {
        frame.cells[row * frame.cols + col].ch = ch;
    }

    #[test]
    fn identical_frames_produce_no_output_at_all() {
        let prev = blank(5, 10);
        let new = blank(5, 10);
        assert_eq!(diff_frames(&prev, &new, 5, 10), "");
    }

    #[test]
    fn a_single_changed_cell_repaints_only_that_cell() {
        let prev = blank(5, 10);
        let mut new = blank(5, 10);
        set(&mut new, 2, 3, 'x');
        let out = diff_frames(&prev, &new, 5, 10);
        // Row 2, col 3 (1-indexed: row 3, col 4), default SGR, the one
        // changed glyph, then the cursor re-assertion (still (0,0) here,
        // but always re-sent once anything was painted).
        assert!(out.starts_with("\x1b[3;4H"), "{out:?}");
        assert!(out.contains('x'), "{out:?}");
        assert!(out.ends_with("\x1b[1;1H\x1b[?25h"), "{out:?}");
    }

    #[test]
    fn a_contiguous_run_of_changed_cells_gets_one_cursor_move() {
        let prev = blank(3, 10);
        let mut new = blank(3, 10);
        set(&mut new, 1, 2, 'a');
        set(&mut new, 1, 3, 'b');
        set(&mut new, 1, 4, 'c');
        let out = diff_frames(&prev, &new, 3, 10);
        // Exactly one cursor-position escape for the whole run (row 1
        // col 2 => 1-indexed "2;3"), not one per changed cell.
        assert_eq!(out.matches("\x1b[2;3H").count(), 1, "{out:?}");
        assert!(out.contains("abc"), "{out:?}");
    }

    #[test]
    fn two_separate_runs_on_the_same_row_get_two_cursor_moves() {
        let prev = blank(3, 10);
        let mut new = blank(3, 10);
        set(&mut new, 0, 1, 'a');
        set(&mut new, 0, 7, 'b');
        let out = diff_frames(&prev, &new, 3, 10);
        assert!(out.contains("\x1b[1;2H"), "{out:?}");
        assert!(out.contains("\x1b[1;8H"), "{out:?}");
    }

    #[test]
    fn a_changed_style_with_the_same_glyph_still_repaints() {
        let prev = blank(2, 5);
        let mut new = blank(2, 5);
        new.cells[0].attrs.bold = true;
        let out = diff_frames(&prev, &new, 2, 5);
        assert!(out.contains("\x1b[1;1H"), "{out:?}");
        assert!(out.contains(&vt100::sgr_codes(vt100::Color::Default, vt100::Color::Default, new.cells[0].attrs)), "{out:?}");
    }

    #[test]
    fn tab_bar_change_alone_rewrites_only_the_tab_bar_row() {
        let prev = blank(5, 20);
        let mut new = blank(5, 20);
        new.tab_bar = "[1] bish".to_string();
        let out = diff_frames(&prev, &new, 6, 20);
        // term_rows = 6 -> tab bar pinned to row 6, followed by the
        // unchanged cursor's own re-assertion (still required once
        // anything at all was painted).
        assert_eq!(out, "\x1b[6;1H\x1b[K[1] bish\x1b[1;1H\x1b[?25h");
    }

    #[test]
    fn cursor_move_alone_with_no_cell_or_tab_bar_change_still_repositions() {
        let prev = blank(5, 20);
        let mut new = blank(5, 20);
        new.cursor = (2, 4);
        let out = diff_frames(&prev, &new, 5, 20);
        assert_eq!(out, "\x1b[3;5H\x1b[?25h");
    }

    #[test]
    fn cursor_visibility_change_alone_still_repositions() {
        let prev = blank(5, 20);
        let mut new = blank(5, 20);
        new.cursor_visible = false;
        let out = diff_frames(&prev, &new, 5, 20);
        assert_eq!(out, "\x1b[1;1H\x1b[?25l");
    }

    #[test]
    fn a_run_can_cross_what_would_be_a_pane_boundary_in_the_flattened_grid() {
        // diff_frames itself has no notion of panes at all -- it only
        // ever sees the flattened per-cell grid TerminalFrame::capture
        // already produced, so a dirty run spanning what were two
        // side-by-side panes' columns is indistinguishable from an
        // ordinary single-pane run. Exercises that no pane-boundary
        // assumption ever crept into the run-extension loop.
        let prev = blank(2, 10);
        let mut new = blank(2, 10);
        for col in 3..7 {
            set(&mut new, 0, col, 'X');
        }
        let out = diff_frames(&prev, &new, 2, 10);
        assert_eq!(out.matches("\x1b[1;4H").count(), 1, "{out:?}");
        assert!(out.contains("XXXX"), "{out:?}");
    }
}

#[cfg(test)]
mod terminal_frame_capture_tests {
    use super::*;

    // Regression: a pane's own cached `rect` (built once by
    // run_fg_job_frame, refreshed only when the *focused* pane's screen
    // size changes -- see TerminalFrame::capture's own doc comment) can
    // describe a larger area than that pane's screen *currently* has,
    // if a sibling pane's screen was independently resized in between --
    // this used to panic inside vt100::Screen::cell with an index-out-
    // of-bounds. `capture` must clamp to the screen's own live size
    // instead of trusting `rect`.
    #[test]
    fn capture_clamps_a_panes_stale_rect_to_its_screens_own_live_size() {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(2, 3)));
        let layout = CompositorLayout {
            panes: vec![PaneSnapshot { rect: Rect { row: 0, col: 0, rows: 5, cols: 10 }, screen, focused: true }],
            dividers: vec![],
        };
        let frame = TerminalFrame::capture(&layout, "", 5, 10);
        assert_eq!(frame.rows, 5);
        assert_eq!(frame.cols, 10);
    }

    #[test]
    fn capture_still_reads_every_real_cell_when_rect_matches_the_screen() {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(2, 3)));
        screen.borrow_mut().feed(b"ab");
        let layout = CompositorLayout {
            panes: vec![PaneSnapshot { rect: Rect { row: 0, col: 0, rows: 2, cols: 3 }, screen, focused: true }],
            dividers: vec![],
        };
        let frame = TerminalFrame::capture(&layout, "", 2, 3);
        assert_eq!(frame.cells[0].ch, 'a');
        assert_eq!(frame.cells[1].ch, 'b');
    }
}

#[cfg(test)]
mod compositor_frame_output_tests {
    use super::*;

    // Regression: render_compositor_frame used to lead with `\x1b[2J`
    // (erase whole display) before repainting every pane/divider/the tab
    // bar -- all of which, together, already cover every cell of the
    // terminal unconditionally (see build_compositor_frame_output's own
    // doc comment). That extra erase was visibly reproducible as a
    // flash on an otherwise perfectly ordinary discrete redraw (`bish
    // --promoted`, Enter, then Ctrl-C at the fresh prompt): an explicit
    // clear immediately followed by repainting the *same* content one
    // real terminal can still render as a blank frame in between.
    #[test]
    fn build_compositor_frame_output_never_erases_the_whole_display() {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(2, 3)));
        let layout = CompositorLayout { panes: vec![PaneSnapshot { rect: Rect { row: 0, col: 0, rows: 2, cols: 3 }, screen, focused: true }], dividers: vec![] };
        let out = build_compositor_frame_output(&layout, "tab", 3);
        assert!(!out.contains("\x1b[2J"), "{out:?}");
    }

    #[test]
    fn build_compositor_frame_output_still_paints_every_row_and_the_tab_bar() {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(2, 3)));
        screen.borrow_mut().feed(b"ab");
        let layout = CompositorLayout { panes: vec![PaneSnapshot { rect: Rect { row: 0, col: 0, rows: 2, cols: 3 }, screen, focused: true }], dividers: vec![] };
        let out = build_compositor_frame_output(&layout, "[0] tab", 3);
        assert!(out.contains("\x1b[1;1H"), "{out:?}");
        assert!(out.contains("\x1b[2;1H"), "{out:?}");
        assert!(out.contains("ab"), "{out:?}");
        // Tab bar pinned to the real last row (term_rows = 3), cleared
        // to end-of-line before its own text.
        assert!(out.contains("\x1b[3;1H\x1b[K[0] tab"), "{out:?}");
    }
}

// Regression: run_command_mode's own "recognized but malformed input"
// arms (unknown :git/:diag/:format subcommand, a failed :w, a syntax
// error, ...) used to report via sink_err -- invisible in practice while
// a Frame::Edit pane is focused, since that pane's own live content is
// rendered by writing straight to the real terminal
// (fileeditor::render_editor_frame), bypassing the session's own vt100
// Screen grid model entirely (sink_err's Grid-sink destination) until it
// later loses focus. show_command_mode_error is the fix: the exact same
// direct-to-real-terminal overlay a *successful* command's own output
// already used (build_command_output_overlay, status != 0 -> its own
// error colors), reached without needing to return out of
// run_command_mode's own loop the way CommandModeOutcome::Ran's overlay
// call does.
#[cfg(test)]
mod command_mode_error_visibility_tests {
    use super::*;

    #[test]
    fn build_command_output_overlay_uses_error_colors_for_nonzero_status() {
        let out = build_command_output_overlay("bish: git: unknown subcommand 'x'", 1, 24, 80);
        assert!(out.contains(ERROR_BG), "{out:?}");
        assert!(out.contains(ERROR_FG), "{out:?}");
        assert!(!out.contains(OUTPUT_BG), "{out:?}");
        assert!(out.contains("bish: git: unknown subcommand 'x'"), "{out:?}");
    }

    #[test]
    fn build_command_output_overlay_uses_output_colors_for_zero_status() {
        let out = build_command_output_overlay("2 substitutions on 1 line", 0, 24, 80);
        assert!(out.contains(OUTPUT_BG), "{out:?}");
        assert!(!out.contains(ERROR_BG), "{out:?}");
    }

    #[test]
    fn build_command_output_overlay_anchors_directly_above_the_prompt_row() {
        // term_rows = 24 -> command_mode_row = 22 (0-indexed) -> the
        // prompt itself lives at 1-indexed row 23; a single-line overlay
        // must land immediately above that, at row 22.
        let out = build_command_output_overlay("bish: an error", 1, 24, 80);
        assert!(out.contains("\x1b[22;1H"), "{out:?}");
        assert!(!out.contains("\x1b[23;1H"), "{out:?}");
    }
}

#[cfg(test)]
mod substitute_command_tests {
    use super::*;

    fn buf_from(text: &str) -> TextBuffer {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), text);
        buf.set_cursor(0, 0);
        buf
    }

    fn text_of(buf: &TextBuffer) -> String {
        (0..buf.line_count()).map(|l| buf.line_chars(l).into_iter().collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn parse_rejects_a_plain_word_starting_with_s() {
        assert!(parse_substitute_command("set foo").is_none());
        assert!(parse_substitute_command("sort").is_none());
        assert!(parse_substitute_command("w").is_none());
    }

    #[test]
    fn parse_a_bare_s_with_no_range_defaults_to_current_line() {
        let cmd = parse_substitute_command("s/foo/bar/").unwrap().unwrap();
        assert_eq!(cmd.from, LineRef::Current);
        assert_eq!(cmd.to, LineRef::Current);
        assert_eq!(cmd.pattern, "foo");
        assert_eq!(cmd.replacement, "bar");
        assert!(!cmd.global);
    }

    #[test]
    fn parse_accepts_g_flag_and_a_missing_trailing_delimiter() {
        let cmd = parse_substitute_command("s/foo/bar/g").unwrap().unwrap();
        assert!(cmd.global);
        // No trailing "/" at all -- still valid, matches real vim.
        let cmd = parse_substitute_command("s/foo/bar").unwrap().unwrap();
        assert_eq!(cmd.replacement, "bar");
        assert!(!cmd.global);
    }

    #[test]
    fn parse_percent_range_is_whole_buffer() {
        let cmd = parse_substitute_command("%s/foo/bar/").unwrap().unwrap();
        assert_eq!(cmd.from, LineRef::Number(1));
        assert_eq!(cmd.to, LineRef::Last);
    }

    #[test]
    fn parse_numeric_and_dot_dollar_ranges() {
        let cmd = parse_substitute_command("5,10s/foo/bar/").unwrap().unwrap();
        assert_eq!(cmd.from, LineRef::Number(5));
        assert_eq!(cmd.to, LineRef::Number(10));
        let cmd = parse_substitute_command(".,$s/foo/bar/").unwrap().unwrap();
        assert_eq!(cmd.from, LineRef::Current);
        assert_eq!(cmd.to, LineRef::Last);
    }

    #[test]
    fn parse_supports_an_alternate_delimiter() {
        // "/" is itself part of the pattern here -- "#" as the delimiter
        // is what makes that possible without escaping.
        let cmd = parse_substitute_command("s#/usr/bin#/opt/bin#").unwrap().unwrap();
        assert_eq!(cmd.pattern, "/usr/bin");
        assert_eq!(cmd.replacement, "/opt/bin");
    }

    #[test]
    fn parse_honors_an_escaped_delimiter_inside_the_pattern() {
        let cmd = parse_substitute_command(r"s/a\/b/c/").unwrap().unwrap();
        assert_eq!(cmd.pattern, "a/b");
        assert_eq!(cmd.replacement, "c");
    }

    #[test]
    fn parse_empty_pattern_is_an_error() {
        assert_eq!(parse_substitute_command("s//bar/").unwrap().unwrap_err(), "E35: no previous regular expression");
    }

    #[test]
    fn expand_replacement_handles_ampersand_and_backreferences() {
        let caps = vec!["ab".to_string(), "a".to_string(), "b".to_string()];
        assert_eq!(expand_replacement("[&]", &caps), "[ab]");
        assert_eq!(expand_replacement(r"\2\1", &caps), "ba");
        assert_eq!(expand_replacement(r"\\&", &caps), "\\ab");
        assert_eq!(expand_replacement(r"\&", &caps), "&");
    }

    #[test]
    fn expand_replacement_case_modifiers() {
        let caps = vec!["hello".to_string()];
        assert_eq!(expand_replacement(r"\u&", &caps), "Hello");
        assert_eq!(expand_replacement(r"\U&\E!", &caps), "HELLO!");
        assert_eq!(expand_replacement(r"\l\U&", &caps), "hELLO");
    }

    #[test]
    fn expand_replacement_r_inserts_a_real_newline() {
        let caps = vec!["x".to_string()];
        assert_eq!(expand_replacement(r"a\rb", &caps), "a\nb");
    }

    #[test]
    fn substitute_line_first_match_only_without_g() {
        let re = crate::regex::Regex::compile("o");
        let (text, count) = substitute_line("foo bar", &re, "0", false);
        assert_eq!(text, "f0o bar");
        assert_eq!(count, 1);
    }

    #[test]
    fn substitute_line_every_match_with_g() {
        let re = crate::regex::Regex::compile("o");
        let (text, count) = substitute_line("foo bar", &re, "0", true);
        assert_eq!(text, "f00 bar");
        assert_eq!(count, 2);
    }

    // Every case here was checked against real vim first (`vim -es -c
    // '%s/PATTERN/-/g'`) -- see substitute_line's own doc comment for
    // the exact commands and why the line's own end is never itself a
    // valid zero-width match position, except when the whole line is
    // empty.
    #[test]
    fn substitute_line_zero_width_match_makes_forward_progress_but_never_matches_past_the_last_char() {
        let re = crate::regex::Regex::compile("x*");
        let (text, count) = substitute_line("ab", &re, "-", true);
        assert_eq!(text, "-a-b");
        assert_eq!(count, 2);
    }

    #[test]
    fn substitute_line_zero_width_on_a_single_char_line() {
        let re = crate::regex::Regex::compile("x*");
        let (text, count) = substitute_line("a", &re, "-", true);
        assert_eq!(text, "-a");
        assert_eq!(count, 1);
    }

    #[test]
    fn substitute_line_zero_width_on_an_empty_line_still_substitutes_once() {
        let re = crate::regex::Regex::compile("x*");
        let (text, count) = substitute_line("", &re, "-", true);
        assert_eq!(text, "-");
        assert_eq!(count, 1);
    }

    #[test]
    fn substitute_line_a_real_match_reaching_the_end_leaves_nothing_further_to_match() {
        let re = crate::regex::Regex::compile("b*");
        let (text, count) = substitute_line("ab", &re, "-", true);
        assert_eq!(text, "-a-");
        assert_eq!(count, 2);
        let (text, count) = substitute_line("abb", &re, "-", true);
        assert_eq!(text, "-a-");
        assert_eq!(count, 2);
    }

    #[test]
    fn run_substitute_current_line_only_by_default() {
        let mut buf = buf_from("foo\nfoo\nfoo");
        buf.set_cursor(1, 0);
        let cmd = SubstituteCmd { from: LineRef::Current, to: LineRef::Current, pattern: "foo".to_string(), replacement: "bar".to_string(), global: false };
        let (subs, lines) = run_substitute(&mut buf, &cmd).unwrap();
        assert_eq!((subs, lines), (1, 1));
        assert_eq!(text_of(&buf), "foo\nbar\nfoo");
    }

    #[test]
    fn run_substitute_whole_buffer_with_global_flag() {
        let mut buf = buf_from("foo foo\nfoo\nbaz");
        let cmd = SubstituteCmd { from: LineRef::Number(1), to: LineRef::Last, pattern: "foo".to_string(), replacement: "X".to_string(), global: true };
        let (subs, lines) = run_substitute(&mut buf, &cmd).unwrap();
        assert_eq!((subs, lines), (3, 2));
        assert_eq!(text_of(&buf), "X X\nX\nbaz");
    }

    #[test]
    fn run_substitute_backreference_and_capture() {
        let mut buf = buf_from("hello world");
        let cmd = SubstituteCmd { from: LineRef::Current, to: LineRef::Current, pattern: "(hello) (world)".to_string(), replacement: r"\2 \1".to_string(), global: false };
        run_substitute(&mut buf, &cmd).unwrap();
        assert_eq!(text_of(&buf), "world hello");
    }

    #[test]
    fn run_substitute_r_splits_a_line_and_still_advances_the_range_correctly() {
        // Range is the whole 2-line buffer; line 0's own substitution
        // splits it into two lines via \r -- line 1 (originally "b")
        // must still be reached afterward, now at row 2.
        let mut buf = buf_from("a,a\nb");
        let cmd = SubstituteCmd { from: LineRef::Number(1), to: LineRef::Last, pattern: ",".to_string(), replacement: r"\r".to_string(), global: false };
        let (subs, lines) = run_substitute(&mut buf, &cmd).unwrap();
        assert_eq!(subs, 1);
        assert_eq!(lines, 1);
        assert_eq!(text_of(&buf), "a\na\nb");
    }

    #[test]
    fn run_substitute_reports_pattern_not_found() {
        let mut buf = buf_from("foo");
        let cmd = SubstituteCmd { from: LineRef::Current, to: LineRef::Current, pattern: "zzz".to_string(), replacement: "x".to_string(), global: false };
        assert_eq!(run_substitute(&mut buf, &cmd).unwrap_err(), "E486: Pattern not found: zzz");
    }

    #[test]
    fn end_to_end_percent_s_g_via_run_command_mode_parsing_path() {
        // Confirms the two pieces (parse + run) compose correctly, not
        // just each in isolation.
        let mut buf = buf_from("cat cat\ndog");
        let parsed = parse_substitute_command("%s/cat/dog/g").unwrap().unwrap();
        let (subs, lines) = run_substitute(&mut buf, &parsed).unwrap();
        assert_eq!((subs, lines), (2, 1));
        assert_eq!(text_of(&buf), "dog dog\ndog");
    }
}
