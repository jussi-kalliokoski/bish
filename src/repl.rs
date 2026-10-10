use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::rc::Rc;

use std::path::{Path, PathBuf};

use crate::lspclient;
use crate::mcp;

use crate::archive;
use crate::bishedit::Buffer as BisheditBuffer;
use crate::bishedit::completion;
use crate::bishedit::highlight::{self, HighlightContext};
use crate::bishedit::lint;
use crate::bishedit::motion;
use crate::bishedit::registers::{RegisterShape, RegisterValue, Registers};
use crate::bishedit::snippet;
use crate::bishedit::suggestion;
use crate::bishedit::textbuffer::TextBuffer;
use crate::bishedit::unicode_width::col_of;
use crate::bishedit::vimkeys::{GotoKind, KeyOutcome, Op, VimKeys, WindowCmd};
use crate::browser;
use crate::debugger;
use crate::docs;
use crate::editor::{self, Key, ReadOutcome};
use crate::exec::{self, DebugHook, ExecResult, PaneDirection, Shell, WindowAction, WindowMove};
use crate::fileeditor;
use crate::hexedit;
use crate::history::{self, History};
use crate::lexer::Lexer;
use crate::parser::{AndOr, Command, Parser, Pipeline, Program};
use crate::prompt;
use crate::pty;
use crate::session;
use crate::term;
use crate::vt100;
use crate::window::{
    Divider, EditFrameId, Frame, HexFrameId, JobFrameId, JumpEntry, MIN_PANE_WEIGHT, PaneId, PaneLayout, RESIZE_STEP, Rect, SessionId, WindowEntry,
    close_focused_pane, close_pane, completion_menu_rows, content_rows, find_parent_split_mut, focused_col_origin, folded_divider_pane, pane_rect,
    resize_focused_pane, resize_split_child_to, set_child_weight_to_fraction,
};

// One shell session: the root (id 0) or one made by `window new`
// (`Shell::new_virtual_child`). Each has its own continuation buffer, its
// own `History` (a forked persistent chain, so sibling panes' commands
// don't appear in each other's Up/Down), and its own VT100 grid, unused
// before promotion (output goes to the terminal) and capturing output
// after (`apply_window_action`), so switching windows redraws what each
// last drew.
struct SessionState {
    shell: Shell,
    /// The language-server table, shared by every session, and installed in
    /// `shell` as an opaque `exec::ServiceTable` so `::bish lsp` works
    /// without exec.rs naming a server type. Both handles share one
    /// `RefCell`; this one has the real API.
    lsp: Rc<RefCell<lspclient::Table>>,
    /// Project roots found by a `--root-cmd`, keyed by the directory asked
    /// from. Memoized because `server_target` runs every idle tick and a root
    /// command is a real process (`cargo metadata` takes most of a second
    /// cold).
    lsp_roots: HashMap<PathBuf, PathBuf>,
    /// Semantic-token requests sent and not yet collected, by uri:
    /// (request id, buffer version). Asked without waiting, since a bounded
    /// wait on the idle path would stall every quiet moment; a later idle
    /// tick applies the reply.
    lsp_semantic: HashMap<String, (i64, u64)>,
    /// The buffer version whose tokens are painted, per uri, so an unchanged
    /// buffer isn't asked about again.
    lsp_semantic_applied: HashMap<String, u64>,
    /// The in-flight `documentHighlight` and the (version, row, col) asked
    /// about: one at a time across documents, as only the driven buffer has a
    /// cursor.
    lsp_highlight: Option<(i64, u64, usize, usize)>,
    /// The (version, row, col) whose occurrences are marked, so a still
    /// cursor costs a comparison per tick, not a request.
    lsp_highlight_applied: Option<(u64, usize, usize)>,
    /// The in-flight `inlayHint` and the (version, first line, last line)
    /// asked about: one at a time, as only the driven buffer has a viewport.
    lsp_inlay: Option<(i64, u64, usize, usize)>,
    /// The (version, first line, last line) whose hints are drawn, so
    /// scrolling asks again and sitting still doesn't.
    lsp_inlay_applied: Option<(u64, usize, usize)>,
    buffer: String,
    // Whether `buffer` is off the record
    // (`leading_space_suppresses_history`), decided on a command's first
    // line and kept through continuations, so a multi-line command begun
    // with a space stays unrecorded.
    buffer_unrecorded: bool,
    // Shared with the shell: the `history` builtin in exec.rs knows nothing
    // of sessions and reaches this through `exec::HistoryAccess`, as for
    // `ServiceTable`.
    history: Rc<RefCell<History>>,
    screen: Rc<RefCell<vt100::Screen>>,
    // Set once EOF has warned about stopped jobs without exiting, cleared
    // when a command runs, so a second Ctrl-D right away exits, as bash's
    // "There are stopped jobs." does, and typing anything first re-arms it.
    warned_stopped_jobs: bool,
    // Directory history for Alt+Left/Right (`push_dir_history`,
    // `ReadOutcome::DirNav`): every directory this session's cwd changed to,
    // found by comparing `Shell::cwd` around each command so any `cd`
    // counts, with `dir_history_index` marking the position. Seeded with the
    // starting directory.
    dir_history: Vec<std::path::PathBuf>,
    dir_history_index: usize,
    // Every command command mode ran in this session (parsed and executed,
    // whatever its status), across invocations (see `run_command_mode`),
    // shown by Ctrl-L (`render_command_transcript`) while composing or from
    // Normal mode's pending overlay.
    command_transcript: Vec<TranscriptEntry>,
}

/// Everything the window manager keeps while the shell runs: the values
/// created together in `run` and passed together everywhere. Anything
/// belonging to one call (`session_id`, `edit_frame_id`, `rect`, `start`)
/// stays a parameter, so a function's arguments say what that call is
/// about. Small helpers keep narrow signatures
/// (`sync_semantic_tokens(&mut app.sessions, session_id, buf)`), which
/// disjoint field borrows allow.
struct App {
    sessions: HashMap<SessionId, SessionState>,
    // Commands to run as if typed and submitted: `::bish window split <cmd>`,
    // `window new <cmd>`, and each line of a `window restore` script. Fed in
    // at the read that would wait for a keystroke (see `run`), so history
    // expansion, multi-line input, job control and window actions all apply.
    // On the app, not a session: a queued `window split` moves focus to the
    // new pane, and the next line belongs there.
    pending_input: std::collections::VecDeque<String>,
    windows: Vec<WindowEntry>,
    current_window: usize,
    next_session_id: SessionId,
    next_window_id: u32,
    // Every fg'd job, by the id a `Frame::Job` points to (see `Frame` for why
    // jobs aren't in the stack).
    job_frames: HashMap<JobFrameId, exec::FgJob>,
    next_job_frame_id: JobFrameId,
    // `e`'s detachable editor sessions (`Frame::Edit`), likewise. The one
    // being driven isn't here: `run_edit_frame` takes it out and puts it
    // back, so during an edit this holds every other open buffer, the files
    // a cross-file edit mustn't write behind the user's back
    // (`rename_via_server`).
    edit_frames: HashMap<EditFrameId, fileeditor::EditSession>,
    // Where ids for frames made by `gd` into another file come from.
    next_edit_frame_id: EditFrameId,
    // `e --hex`'s frames (`Frame::Hex`).
    hex_frames: HashMap<HexFrameId, hexedit::HexSession>,
    next_hex_frame_id: HexFrameId,
    // `:dbg`'s attached sessions (`Frame::DebugRun`).
    debug_frames: HashMap<EditFrameId, debugger::DebugSession>,
    // `gr`'s answers, keyed like `debug_frames` (`Frame::Locations`).
    location_lists: HashMap<EditFrameId, LocationList>,
    cmd_history: History,
    /// The last `:s`, pattern and flags resolved: what `:s//repl/` and the
    /// `&` flag reach back for.
    last_substitute: Option<LastSubstitute>,
    // The register table (yank, put, `<C-r>`), one for the whole shell, as
    // vim's registers are global (see `Registers`).
    registers: Registers,
    // True from the first time a window command promotes the terminal
    // (`apply_window_action`); until then every sink is Real.
    sinks_are_grid: bool,
    // Watches the file behind the buffer being viewed, so the editor can
    // say it changed underneath (a formatter, a branch switch, another
    // pane): a session can hold a buffer for days, and `:w`'s E13 would come
    // after typing into stale text. One watcher for the app: an inotify
    // descriptor is a kernel object the loop waits on. `None` where the
    // system won't give one, losing only the warning.
    file_watcher: Option<crate::watch::Watcher>,
    /// Debug sessions over the Debug Adapter Protocol, keyed as
    /// `debug_frames`; a buffer has at most one of either.
    dap_frames: HashMap<EditFrameId, DapFrame>,
    /// What the editor shows, for anything asking from outside. A snapshot,
    /// because the buffer being edited isn't in `edit_frames` while edited;
    /// the navigation loop, which holds it, publishes here each idle tick,
    /// and `service_background_jobs` reads it.
    focus: Option<mcp::FileState>,
    /// Proposed file versions waiting for review, each with the waiting
    /// client. Queued rather than opened on arrival, since that tick can be
    /// inside a loop that couldn't redraw; the prompt and Normal mode open
    /// them (`open_pending_reviews`).
    pending_reviews: Vec<(mcp::ReviewRequest, session::Caller)>,
    term_rows: usize,
    term_cols: usize,
}

impl App {
    // The focused window's entry, which nearly every caller wants first.
    fn window(&self) -> &WindowEntry {
        &self.windows[self.current_window]
    }

    fn window_mut(&mut self) -> &mut WindowEntry {
        &mut self.windows[self.current_window]
    }

    // Registers are the part of `App` a call needs beside the rest:
    // `editor::read_line` and `fileeditor::run_insert_mode` take
    // `&mut Registers` and an idle hook reaching into the shell, two mutable
    // borrows of `App`. Lending the field out and back keeps it in `App`.
    fn with_registers<T>(&mut self, body: impl FnOnce(&mut App, &mut Registers) -> T) -> T {
        let mut registers = std::mem::take(&mut self.registers);
        let out = body(self, &mut registers);
        self.registers = registers;
        out
    }

    // The focused pane's rectangle, not the screen: a pager, the browser and
    // the hex view paint inside their pane.
    fn focused_pane_rect(&self) -> Rect {
        let window = self.window();
        pane_rect(window, window.focused_pane, self.term_rows, self.term_cols)
    }
}

// The terminal's size via `TIOCGWINSZ` on fd 0, with a conservative
// fallback if the query fails.
fn query_term_size() -> (usize, usize) {
    match pty::get_size(0) {
        Ok(ws) if ws.rows > 0 && ws.cols > 0 => (ws.rows as usize, ws.cols as usize),
        _ => (24, 80),
    }
}

// The one `exec::take_winch()` consumer, called from
// `service_background_jobs`, which every blocking loop's `on_idle` calls,
// so a resize is noticed within a tick whichever loop blocks (and from
// `run`'s loop directly). When the size changed, resizes every session's
// screen and every background job's pty to match, as a multiplexer
// does. The fg'd job isn't in `job_frames` while driven; `drive_fg_job`
// resizes it from the screen.
fn poll_and_apply_resize(app: &mut App) -> bool {
    use std::os::fd::AsRawFd;
    if !exec::take_winch() {
        return false;
    }
    let (rows, cols) = query_term_size();
    if (rows, cols) == (app.term_rows, app.term_cols) {
        return false;
    }
    app.term_rows = rows;
    app.term_cols = cols;
    for s in app.sessions.values() {
        s.screen.borrow_mut().resize(content_rows(app.term_rows), app.term_cols);
    }
    // Read from the pane tree first: resizing reaches into `sessions` and
    // `job_frames`, and the walk holds `windows`.
    let running: Vec<(JobFrameId, SessionId)> = app
        .windows
        .iter()
        .flat_map(|window| window.panes.iter())
        .filter_map(|pane| match pane.stack.last() {
            Some(Frame::Job(job_frame_id)) => Some((*job_frame_id, pane.owning_session())),
            _ => None,
        })
        .collect();
    for (job_frame_id, sid) in running {
        let (rows, cols) = app.sessions[&sid].screen.borrow().size();
        if let Some(job) = app.job_frames.get_mut(&job_frame_id) {
            let _ = pty::set_size(job.pty_master().as_raw_fd(), rows as u16, cols as u16);
        }
    }
    if app.sinks_are_grid {
        compositor_redraw(app);
    }
    true
}

// Whether `sid` is reachable from any (window, depth) besides the focused
// window's top frame: whether closing that reference would orphan the
// session (running its exit trap) or drop one of several.
fn session_referenced_elsewhere(windows: &[WindowEntry], current_window: usize, sid: SessionId) -> bool {
    for (i, w) in windows.iter().enumerate() {
        for pane in &w.panes {
            for (depth, frame) in pane.stack.iter().enumerate() {
                let matches = match frame {
                    Frame::Session(s) => *s == sid,
                    Frame::Job(_) | Frame::Edit(_) | Frame::Hex(_) | Frame::Diagnostics(_) | Frame::Locations(_) | Frame::DebugRun(_) => false,
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

// Runs `$HOME/.config/bish/config.bash`, if present, in the shell's
// top-level scope before the prompt, like bash's `~/.bashrc`: what it
// sets persists (`Shell::run_source_here`, as `source` uses). Interactive
// shells only. A missing file is skipped; a read failure or syntax error
// is reported on the shell's stderr, and the prompt still comes up.
fn load_config(shell: &mut Shell) {
    let Some(home) = std::env::var_os("HOME") else { return };
    let path = std::path::PathBuf::from(home).join(".config/bish/config.bash");
    match std::fs::read_to_string(&path) {
        Ok(src) => load_config_source(shell, &src, &path.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("bish: {}: {}", path.display(), e),
    }
}

/// `load_config` with the text in hand, testable without moving `$HOME`.
fn load_config_source(shell: &mut Shell, src: &str, name: &str) {
    shell.run_source_here(src, name);
    // Capture what config.bash set into the shell's remembered environment:
    // the snapshot was taken in `Shell::new`, before this, and the first
    // `sync_real_state_in` removes every variable it lacks.
    shell.sync_real_state_out();
}

pub fn run(mut shell: Shell, start_promoted: bool, load_rc: bool) {
    // Before the rc, which an interactive shell reads (see
    // `Shell::mark_interactive`). The config loads here, in `run`, so every
    // way into the interactive shell, the session daemon included, gets it.
    shell.mark_interactive();
    if load_rc {
        load_config(&mut shell);
    }
    // The shell survives Ctrl-C, as bash's interactive top level does; a
    // foreground child still gets it (see `term::ignore_sigint`).
    term::ignore_sigint();
    // Borrowed and restored at the end: the terminal's previous title is
    // its own.
    term::push_window_title();
    exec::install_winch_handler();
    // This loop is the window manager, so `::bish window`'s actions mean
    // something at its prompts (`Shell::windows_available`).
    shell.windows_available = true;
    // bash enables job control for an interactive shell (see
    // `Shell::enable_monitor_mode`).
    shell.enable_monitor_mode();
    // And expands aliases, again only when interactive; a script still
    // needs `shopt -s expand_aliases`, as in bash.
    shell.enable_shopt("expand_aliases");

    let history_size = shell.bishopt_int("history_size").max(0) as usize;
    let (term_rows, term_cols) = query_term_size();

    let mut sessions: HashMap<SessionId, SessionState> = HashMap::new();
    let root_screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(term_rows), term_cols)));
    let root_cwd = shell.cwd.clone();
    let root_lsp: Rc<RefCell<lspclient::Table>> = Rc::new(RefCell::new(lspclient::Table::default()));
    install_service_table(&mut shell, &root_lsp);
    let root_history = Rc::new(RefCell::new(History::load(".bish_history", history_size)));
    install_history(&mut shell, &root_history);
    sessions.insert(
        0,
        SessionState {
            shell,
            lsp: root_lsp,
            lsp_roots: HashMap::new(),
            lsp_semantic: HashMap::new(),
            lsp_semantic_applied: HashMap::new(),
            lsp_highlight: None,
            lsp_highlight_applied: None,
            lsp_inlay: None,
            lsp_inlay_applied: None,
            buffer: String::new(),
            buffer_unrecorded: false,
            history: root_history,
            screen: root_screen,
            warned_stopped_jobs: false,
            dir_history: vec![root_cwd],
            dir_history_index: 0,
            command_transcript: Vec::new(),
        },
    );
    // Everything the window manager keeps (see `App`), bound as `&mut` so
    // calls below spell it as the callees do.
    let app = &mut App {
        sessions,
        pending_input: std::collections::VecDeque::new(),
        windows: vec![WindowEntry::single(0, Frame::Session(0))],
        current_window: 0,
        next_session_id: 1,
        next_window_id: 1,
        job_frames: HashMap::new(),
        next_job_frame_id: 1,
        edit_frames: HashMap::new(),
        next_edit_frame_id: 1,
        hex_frames: HashMap::new(),
        next_hex_frame_id: 1,
        debug_frames: HashMap::new(),
        location_lists: HashMap::new(),
        cmd_history: History::load(".bish_cmd_history", history_size),
        last_substitute: None,
        registers: Registers::new(),
        sinks_are_grid: false,
        file_watcher: crate::watch::Watcher::new().ok(),
        dap_frames: HashMap::new(),
        focus: None,
        pending_reviews: Vec::new(),
        term_rows,
        term_cols,
    };
    // Set when `run_normal_mode_navigation` returns text and a cursor to
    // resume editing with, consumed by the next `read_line`.
    let mut pending_initial: Option<(String, usize)> = None;

    // `bish --promoted`: the transition `ensure_promoted` makes on the first
    // window command, done here so the first prompt already has the tab bar
    // and alternate screen; safe before the loop, with only the root
    // session. The `compositor_redraw` draws the tab bar at once.
    if start_promoted {
        ensure_promoted(&mut app.sessions, &mut app.sinks_are_grid);
        compositor_redraw(app);
    }

    loop {
        // Between one `read_line` and the next. A resize during one is
        // handled inside it, through every blocking loop's `on_idle`
        // (`service_background_jobs`), within a tick.
        poll_and_apply_resize(app);

        let session_id = app.windows[app.current_window].owning_session();

        // The focused window's top frame is a running job: just pushed by the
        // `fg_pending` branch below, or a window detached from earlier
        // (`FgOutcome::Detached`) that kept running. Drive it either way.
        if let Frame::Job(job_frame_id) = *app.windows[app.current_window].stack().last().unwrap() {
            run_fg_job_frame(app, job_frame_id, session_id);
            let _ = io::stdout().flush();
            continue;
        }

        // The same for a detached `Frame::Edit`, just pushed or returned to.
        if let Frame::Edit(edit_frame_id) = *app.windows[app.current_window].stack().last().unwrap() {
            run_edit_frame(app, edit_frame_id, session_id);
            let _ = io::stdout().flush();
            continue;
        }

        // The same for `e --hex`'s frame (`Frame::Hex`; see `run_hex_frame`
        // for why it can't reuse `run_edit_frame`).
        if let Frame::Hex(hex_frame_id) = *app.windows[app.current_window].stack().last().unwrap() {
            run_hex_frame(app, hex_frame_id, session_id);
            let _ = io::stdout().flush();
            continue;
        }

        // The same for `:diag`'s diagnostics pane (`split_diagnostics_pane`),
        // when focus moves onto it (`<C-w>j`, a click on its title bar).
        if let Frame::Diagnostics(edit_frame_id) = *app.windows[app.current_window].stack().last().unwrap() {
            run_diagnostics_frame(app, app.windows[app.current_window].focused_pane, edit_frame_id);
            let _ = io::stdout().flush();
            continue;
        }

        // The same for `gr`'s location list (`split_locations_pane`).
        if let Frame::Locations(edit_frame_id) = *app.windows[app.current_window].stack().last().unwrap() {
            run_locations_frame(app, app.windows[app.current_window].focused_pane, edit_frame_id);
            let _ = io::stdout().flush();
            continue;
        }

        // The same for `:dbg`'s debug-run pane (`split_debug_run_pane`), when
        // focused while nothing runs; a real run takes the terminal (see
        // `run_debug_run_frame`).
        if let Frame::DebugRun(edit_frame_id) = *app.windows[app.current_window].stack().last().unwrap() {
            run_debug_run_frame(app, app.windows[app.current_window].focused_pane, edit_frame_id);
            let _ = io::stdout().flush();
            continue;
        }

        // Unpromoted only: a grid tracks its cursor, and redraws use it. A
        // builtin's output without a trailing newline (`printf foo`) would
        // be erased by the next prompt redraw (see
        // `Shell::take_needs_newline`). After an external command, whose
        // output this can't see, the terminal is asked with a DSR round
        // trip (`Shell::take_ran_external`), too costly for every prompt; a
        // failed or timed-out query assumes no newline, rather than stall.
        if !app.sinks_are_grid {
            let shell = &app.sessions[&session_id].shell;
            let ran_external = shell.take_ran_external();
            // Always drained, even when the DSR query below supersedes it,
            // or a stale `true` from an earlier builtin would add a newline
            // at a later, unrelated prompt.
            let builtin_needs_newline = shell.take_needs_newline();
            let needs_newline = if ran_external { term::query_cursor_column().is_some_and(|col| col != 1) } else { builtin_needs_newline };
            if needs_newline {
                print!("\r\n");
                let _ = io::stdout().flush();
            }
        }
        // Before the prompt is built: `PROMPT_COMMAND` exists to change it.
        // Only for a primary prompt, as in bash.
        if app.sessions.get(&session_id).is_some_and(|s| s.buffer.is_empty())
            && let Some(session) = app.sessions.get_mut(&session_id)
        {
            session.shell.run_prompt_command();
        }
        let (col_origin, width) = focused_col_origin(&app.windows[app.current_window], app.sinks_are_grid, app.term_rows, app.term_cols);
        // Once per prompt: an unsplit window draws without the compositor, so
        // its title sync doesn't run, and a `cd` is what a title is for.
        sync_terminal_title(app);
        let prompt_str = {
            let session = &app.sessions[&session_id];
            if session.buffer.is_empty() { prompt::render(&session.shell, width) } else { prompt::continuation() }
        };
        // What a paste's later lines are drawn under: they're continuation
        // lines.
        let continuation_str = prompt::continuation();
        // A snapshot, not a borrow: `on_idle` below borrows `app.sessions`
        // mutably to service other windows' jobs. Nothing recorded meanwhile
        // needs to appear mid-browse; a clone is O(1) (see `History`).
        let session_history = app.sessions[&session_id].history.borrow().clone();
        // A snapshot for the same reason: `on_idle`'s borrow would conflict
        // with borrowing the shell's cwd and functions as arguments. Function
        // names only, all the command-validity check needs.
        let cwd_snapshot = app.sessions[&session_id].shell.cwd.clone();
        // Read per prompt, not per shell: a per-directory hook (mise, direnv)
        // rewrites PATH on every `cd`, and the validity check must see the
        // session's PATH (`HighlightContext::path`).
        let path_snapshot = app.sessions[&session_id].shell.raw_var_lookup("PATH");
        let known_functions: HashSet<String> = app.sessions[&session_id].shell.function_names().map(String::from).collect();
        // A snapshot too, of this session's abbreviations for this
        // language: a shell prompt is bash, `abbr`'s default, so an
        // abbreviation written without a language is live here.
        let abbrs_snapshot = snippet::for_language(&app.sessions[&session_id].shell.abbrs, snippet::DEFAULT_LANG);
        // This redraw's `::bish hl` colours, resolved once (see
        // `syntax_color_overrides`).
        let color_overrides = syntax_color_overrides(&app.sessions[&session_id].shell);
        // Per redraw, so `bishopt --set hyperlinks off` applies at the next
        // key.
        let hyperlinks = app.sessions[&session_id].shell.bishopt_bool("hyperlinks");
        // Per redraw, so `bishopt --set mouse off` hands selection back to
        // the terminal at the next prompt.
        let mouse = app.sessions[&session_id].shell.bishopt_bool("mouse");
        let highlight_ctx = HighlightContext {
            cwd: Some(cwd_snapshot.as_path()),
            known_functions: Some(&known_functions),
            path: Some(&path_snapshot),
            color_overrides: Some(&color_overrides),
            hyperlinks,
            // The prompt's vocabulary is the shell's; colon commands are
            // the colon line's.
            line_commands: None,
        };
        // Registered `complete NAME` specs, the shell data they need
        // (aliases, PATH commands, jobs), and a preamble for `-F`/`-C`
        // specs, which run in a subprocess (compgen.rs). One PATH scan per
        // prompt, not per key.
        let completions_snapshot = app.sessions[&session_id].shell.completions_snapshot();
        let default_completion_snapshot = app.sessions[&session_id].shell.default_completion_snapshot();
        let action_ctx_snapshot = app.sessions[&session_id].shell.action_context();
        let preamble_snapshot = app.sessions[&session_id].shell.functions_preamble();
        // Built from the same locals as `highlight_ctx`.
        let shell_completion = completion::ShellCompletionProvider {
            honor_gitignore: app.sessions[&session_id].shell.bishopt_bool("gitignore"),
            // Per prompt: `FIGNORE` and `force_fignore` can change between
            // commands.
            fignore: app.sessions[&session_id].shell.fignore_suffixes(),
            force_fignore: app.sessions[&session_id].shell.shopt_is_on("force_fignore"),
            cwd: Some(cwd_snapshot.as_path()),
            known_functions: Some(&known_functions),
            completions: Some(&completions_snapshot),
            default_completion: default_completion_snapshot.as_ref(),
            action_ctx: Some(&action_ctx_snapshot),
            functions_preamble: Some(&preamble_snapshot),
            hl_names: app.sessions[&session_id].shell.hl_colors().into_iter().map(|(name, _)| name).collect(),
        };
        // `session_history` is the snapshot the suggestions engine needs.
        let shell_suggestion = suggestion::HistorySuggestionProvider { history: &session_history, cwd: Some(cwd_snapshot.as_path()) };
        // `= 3*(2+7)` previews its answer instead of a history suggestion.
        // Variables are read from the process environment; unset reads as 0,
        // as in shell arithmetic.
        let arith_suggestion = suggestion::ArithSuggestionProvider {
            inner: &shell_suggestion,
            vars: &|name| std::env::var(name).ok().and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(0),
        };
        // Relative-cursor menu tricks are only safe on a single real
        // terminal; in a promoted pane the extra row could spill into a
        // neighbour or the tab bar, so grid mode positions absolutely (see
        // `redraw_with_completion_row`, `completion_menu_rows`).
        let cursor_row = app.sessions[&session_id].screen.borrow().cursor().0;
        let row_origin = completion_menu_rows(&app.windows[app.current_window], app.sinks_are_grid, cursor_row, app.term_rows, app.term_cols);
        let menu_capable = !app.sinks_are_grid || row_origin.is_some();

        // Ctrl-E's live search draws on the global status row with this copy,
        // taken before `on_idle` borrows `app.term_rows`/`app.term_cols` (see
        // `read_line`).
        let global_row_size = Some((app.term_rows, app.term_cols));
        // Read before the call: the idle hook below borrows all of `app`.
        let promoted = app.sinks_are_grid;
        // A command given to this session by `window split <cmd>` or
        // `window new <cmd>` stands in for the read (see `App::pending_input`
        // for why it arrives as an ordinary Line).
        let injected = app.pending_input.pop_front();
        let outcome = match injected {
            // No echo here: the Line path writes the prompt and command
            // into the grid on its way, so the pane reads as if typed.
            Some(line) => Ok(ReadOutcome::Line(line)),
            None => app.with_registers(|app, registers| {
                editor::read_line(
                    &prompt_str,
                    &continuation_str,
                    &session_history,
                    false,
                    app.sinks_are_grid,
                    pending_initial.take(),
                    col_origin,
                    width,
                    highlight_ctx,
                    Some(&shell_completion),
                    Some(&arith_suggestion),
                    menu_capable,
                    row_origin,
                    registers,
                    &abbrs_snapshot,
                    global_row_size,
                    // Repaints the panes when something arrived beneath
                    // this prompt (a background job's output, a client
                    // reattaching) and tells `read_line` to redraw its line
                    // on top; neither half can do the other's.
                    &mut || {
                        let mut changed = service_background_jobs(app);
                        // A review a client waits on opens here, at an idle
                        // prompt (see `pending_reviews`); then a repaint.
                        changed |= open_pending_reviews(app);
                        if changed && app.sinks_are_grid {
                            compositor_redraw(app);
                        }
                        changed
                    },
                    prompt_claims_mouse(mouse, promoted),
                    // The shell prompt isn't a mode `::bish map` knows: an
                    // empty table, and nothing can have queued keys for it.
                    Vec::new(),
                    Vec::new(),
                )
            }),
        };
        match outcome {
            // A paste's complete lines, run nothing: they go into the
            // continuation buffer, so the paste is one history entry and
            // one execution, and the partial last line returns to the
            // prompt with the cursor where it was. No history expansion:
            // that belongs to the start of a typed command, and a `!` in
            // pasted script is usually a negation or a filename.
            Ok(ReadOutcome::Pasted { lines, rest, cursor }) => {
                let session = app.sessions.get_mut(&session_id).unwrap();
                session.warned_stopped_jobs = false;
                for (i, line) in lines.iter().enumerate() {
                    if session.buffer.is_empty() {
                        session.buffer_unrecorded = starts_off_the_record(line);
                    }
                    if app.sinks_are_grid {
                        // The same feed a submitted line gets (see the Line
                        // arm).
                        let shown = if i == 0 { &prompt_str } else { &continuation_str };
                        let highlighted = highlight::render_line(line, highlight_ctx);
                        let echoed = format!("\r\x1b[K{}{}\r\n", shown, highlighted);
                        session.screen.borrow_mut().feed(echoed.as_bytes());
                    }
                    if !session.buffer.is_empty() {
                        session.buffer.push('\n');
                    }
                    session.buffer.push_str(line);
                }
                pending_initial = Some((rest, cursor));
            }
            Ok(ReadOutcome::Eof) => {
                // Whether closing this (window, top frame) reference leaves
                // the session unreferenced anywhere: `window fg` can put
                // one session atop several stacks or buried under a frame,
                // and closing one reference shouldn't fire the exit trap of
                // a session still reachable.
                let will_orphan = !session_referenced_elsewhere(&app.windows, app.current_window, session_id);
                let is_final_exit = will_orphan
                    && app.windows.len() == 1
                    && app.windows[app.current_window].panes.len() == 1
                    && app.windows[app.current_window].stack().len() == 1;
                // bash refuses the first Ctrl-D exit with a stopped job
                // ("There are stopped jobs."), and a second immediate EOF
                // confirms (see `Shell::has_stopped_jobs`,
                // `warned_stopped_jobs`). Only when exiting the process:
                // the job table is shared, so closing one window loses no
                // job.
                if is_final_exit {
                    let session = app.sessions.get_mut(&session_id).unwrap();
                    if session.buffer.is_empty() && session.shell.has_stopped_jobs() && !session.warned_stopped_jobs {
                        session.shell.sink_err("bish: There are stopped jobs.\n");
                        session.warned_stopped_jobs = true;
                        if app.sinks_are_grid {
                            compositor_redraw(app);
                        }
                        let _ = io::stdout().flush();
                        continue;
                    }
                }
                let session = app.sessions.get_mut(&session_id).unwrap();
                if !session.buffer.is_empty() {
                    session.shell.sink_err("bish: syntax error: unexpected end of input\n");
                }
                if will_orphan {
                    session.shell.run_exit_trap();
                }
                if is_final_exit {
                    // The last frame of the last window: exit, leaving the
                    // alternate screen if promotion entered it.
                    if session.shell.is_promoted() {
                        // Straight to the terminal, not a session's sink:
                        // leaving the alternate screen is a whole-terminal
                        // action.
                        print!("\x1b[?1049l");
                        let _ = io::stdout().flush();
                    }
                    break;
                }
                // Otherwise EOF closes the window's top frame, as
                // `window close` does.
                apply_window_action(app, WindowAction::Close);
            }
            // Reported only when promoted (`ctrl_l_reports` is
            // `app.sinks_are_grid`); unpromoted, `read_line` clears the
            // real screen itself. Promoted, a raw clear would wipe the
            // compositor's frame with nothing to repaint it, and repainting
            // alone changes nothing. So the session's grid is cleared
            // (`\x1b[H\x1b[2J`, which in vt100.rs clears the live grid and
            // keeps scrollback, as a real Ctrl-L does), resetting its
            // cursor to (0, 0), so the next redraw shows the prompt at the
            // pane's top.
            Ok(ReadOutcome::CtrlL) => {
                if app.sinks_are_grid {
                    app.sessions.get_mut(&session_id).unwrap().screen.borrow_mut().feed(b"\x1b[H\x1b[2J");
                    compositor_redraw(app);
                }
            }
            // A left click at the prompt, handed back by `read_line` (see
            // `ReadOutcome::Mouse`). Before promotion there's nothing to
            // click. A miss, or a click on the focused tab or pane, resumes
            // typing through `pending_initial`. A real change freezes the
            // typed text into this session's grid
            // (`freeze_input_with_text`; `SessionState.buffer` doesn't
            // mirror `read_line`'s keystrokes) and switches; the next
            // `read_line` starts fresh in the focused session, never moving
            // typed text to another pane.
            Ok(ReadOutcome::Mouse { event, text, cursor }) => {
                let target = if app.sinks_are_grid { hit_test_click(app, event) } else { None };
                match target {
                    Some(ClickTarget::Window(idx)) if idx != app.current_window => {
                        freeze_input_with_text(app.sessions.get_mut(&session_id).unwrap(), &text);
                        app.current_window = idx;
                        compositor_redraw(app);
                    }
                    Some(ClickTarget::Pane(pane_id)) if pane_id != app.windows[app.current_window].focused_pane => {
                        freeze_input_with_text(app.sessions.get_mut(&session_id).unwrap(), &text);
                        // Going to a pane is how a minimized one comes back.
                        restore_if_minimized(&mut app.windows[app.current_window], pane_id);
                        app.windows[app.current_window].focused_pane = pane_id;
                        compositor_redraw(app);
                    }
                    Some(ClickTarget::Divider(divider)) => {
                        // Resume the prompt with what was typed.
                        pending_initial = Some((text, cursor));
                        drag_divider(app, &divider);
                    }
                    _ => pending_initial = Some((text, cursor)),
                }
            }
            Ok(ReadOutcome::Interrupted { text }) => {
                let session = app.sessions.get_mut(&session_id).unwrap();
                // `read_line`'s Ctrl-C prints a bare "^C\r\n", which on a
                // pane's last row can land on the tab bar.
                // `compositor_redraw` repaints everything, but from the
                // grid, where the prompt never was, which would blank the
                // line and flash. So the line is recorded into the grid
                // first, as a submitted command is, which also leaves `^C`
                // in the scrollback, as bash does. Fed before
                // `buffer.clear()`, which decides whether it was a
                // continuation prompt.
                if app.sinks_are_grid {
                    freeze_interrupted_line(session, &text);
                }
                // Ctrl-C abandons a pending multi-line construct, as in bash.
                session.buffer.clear();
                // Re-arms the stopped-jobs warning, as a Line does.
                session.warned_stopped_jobs = false;
                if app.sinks_are_grid {
                    compositor_redraw(app);
                }
            }
            Ok(ReadOutcome::DirNav(kind)) => {
                let session = app.sessions.get_mut(&session_id).unwrap();
                session.warned_stopped_jobs = false;
                navigate_dir(session, kind);
                // As for Ctrl-C: the prompt was never in the grid, so it's
                // written there before the repaint. DirNav fires only at an
                // empty buffer, and this writes the post-`cd` prompt
                // `read_line` is about to draw.
                if app.sinks_are_grid {
                    freeze_idle_prompt(session);
                    compositor_redraw(app);
                }
            }
            Ok(ReadOutcome::Opener { text, cursor }) => {
                // The opener paints in one pane's rect, which needs the
                // compositor, as Normal mode does.
                ensure_promoted(&mut app.sessions, &mut app.sinks_are_grid);
                match run_opener(app) {
                    Some((target, destination)) => act_on_opener_choice(app, target, destination, Some(&text)),
                    // Put away without a pick: the line comes back as it was.
                    None => {
                        pending_initial = Some((text, cursor));
                        if app.sinks_are_grid {
                            compositor_redraw(app);
                        }
                    }
                }
            }
            Ok(ReadOutcome::NormalMode { text, cursor, wheel }) => {
                ensure_promoted(&mut app.sessions, &mut app.sinks_are_grid);
                match run_normal_mode_navigation(app, session_id, None, NavStart::Prompt { text, cursor, wheel }, None) {
                    Ok((NavExit::Resume(t, c), _)) => {
                        pending_initial = Some((t, c));
                        // Back to typing: the global mode-line row this
                        // excursion used belongs to neither `read_line` nor
                        // `compositor_redraw`, so it's erased explicitly,
                        // before the repaint, whose closing cursor
                        // placement the prompt draws against (as
                        // `run_edit_frame` does on exit).
                        print!("{}", erase_global_status_row(app.term_rows));
                        if app.sinks_are_grid {
                            compositor_redraw(app);
                        }
                    }
                    // The mode line goes here too: `<C-w>s` from Normal
                    // mode leaves through this arm, and what gets focus
                    // (usually a new pane's prompt) has nothing to draw on
                    // that row; a frame owning it redraws it. `OpenAt`
                    // comes only from `gd`, which needs a file, and this
                    // buffer is the prompt line, so it can't arrive;
                    // treated as a detach if it does.
                    Ok((NavExit::Detached | NavExit::Quit | NavExit::OpenAt { .. }, _)) => {
                        pending_initial = None;
                        print!("{}", erase_global_status_row(app.term_rows));
                        // Then repaint, as in the Resume arm: the erase
                        // leaves the cursor on that row, and the prompt
                        // positions by column only.
                        if app.sinks_are_grid {
                            compositor_redraw(app);
                        }
                        let _ = io::stdout().flush();
                    }
                    Err(e) => {
                        app.sessions.get(&session_id).unwrap().shell.sink_err(&format!("bish: error reading input: {}\n", e));
                        break;
                    }
                }
            }
            Ok(ReadOutcome::Line(line)) => {
                let mut window_action = None;
                let mut fg_pending = false;
                let mut edit_pending = false;
                {
                    let session = app.sessions.get_mut(&session_id).unwrap();
                    // Any input, even a blank Enter, re-arms the
                    // stopped-jobs warning: only an immediate second Ctrl-D
                    // exits.
                    session.warned_stopped_jobs = false;

                    // Only for a command's first line. History expansion
                    // (`!!`, `!-n`, `!n`, `!$`, `!/prefix`, `!?text`; see
                    // `history::expand`) and the off-the-record check
                    // belong to the start of a command: a heredoc body or
                    // continued line may hold a literal `!`.
                    if session.buffer.is_empty() {
                        session.buffer_unrecorded = starts_off_the_record(&line);
                    }
                    let line = if session.buffer.is_empty() {
                        let history_snapshot = session.history.borrow().clone();
                        match history::expand(&line, &history_snapshot) {
                            Ok(history::Expansion::Substituted(s)) => s,
                            Ok(history::Expansion::UnrecognizedBang(rest)) => format!("({})", rest),
                            Err(msg) => {
                                session.shell.sink_err(&format!("{}\n", msg));
                                if app.sinks_are_grid {
                                    compositor_redraw(app);
                                }
                                continue;
                            }
                        }
                    } else {
                        line
                    };
                    let session = app.sessions.get_mut(&session_id).unwrap();

                    // Feed the prompt and the typed line into the grid:
                    // `read_line` draws them only on the real screen, so a
                    // promoted session's next `compositor_redraw` would
                    // wipe them, leaving only output. With this a pane
                    // reads like a terminal: prompt, command, output. The
                    // expanded line is echoed, as bash shows what `!!`
                    // became. The leading `\r\x1b[K` overwrites an idle
                    // prompt `freeze_idle_prompt` left on this row when the
                    // pane lost focus; safe, as the grid is this pane's
                    // own.
                    if app.sinks_are_grid {
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
                        Ok(toks) => match Parser::new(session.shell.expand_aliases(toks)).parse_program() {
                            Ok(prog) => {
                                // Before recording, so `record` tags the
                                // entry with where it runs, for the
                                // suggestion engine; also used below to
                                // notice any directory change, however it
                                // happened.
                                let cwd_before = session.shell.cwd.clone();
                                // Recorded whatever its status, as bash
                                // records what was typed, unless typed with
                                // a leading space (see
                                // `starts_off_the_record`).
                                if !session.buffer_unrecorded {
                                    session.history.borrow_mut().record(&session.buffer, Some(&cwd_before));
                                }
                                // Before running and after history has it:
                                // a hook timing or logging wants the line
                                // history kept.
                                let submitted = session.buffer.clone();
                                run_shell_hooks(&mut app.sessions, session_id, "shell:exec:pre", &submitted);
                                let session = app.sessions.get_mut(&session_id).unwrap();
                                // Sessions sharing this process (see
                                // `new_virtual_child`) have their variables
                                // and umask applied before running and
                                // captured after, or a sibling's last
                                // command would leak in. Cheap when this
                                // session was already synced.
                                session.shell.sync_real_state_in();
                                // What a peer asking about the editor is
                                // told during this line: the state now,
                                // unchanged until `run_program` returns
                                // (`session::service_while_busy`).
                                let state = editor_state(app);
                                session::answer_while_busy(Some(Box::new(move |question: &[u8]| mcp::answer_from_state(&state, question))));
                                let session = app.sessions.get_mut(&session_id).unwrap();
                                let result = session.shell.run_program(&prog);
                                session::answer_while_busy(None);
                                session.shell.sync_real_state_out();
                                if session.shell.cwd != cwd_before {
                                    push_dir_history(session, session.shell.cwd.clone());
                                }
                                let moved_to = (session.shell.cwd != cwd_before).then(|| session.shell.cwd.to_string_lossy().into_owned());
                                // Before `cwd:change`: this is about the
                                // command that ended, and a directory
                                // change follows from it.
                                run_shell_hooks(&mut app.sessions, session_id, "shell:exec:post", &submitted);
                                let session = app.sessions.get_mut(&session_id).unwrap();
                                session.buffer.clear();
                                // After the command and the state sync, so
                                // a hook asking where it is gets what the
                                // next prompt shows.
                                if let Some(moved_to) = moved_to {
                                    run_shell_hooks(&mut app.sessions, session_id, "shell:cwd:change", &moved_to);
                                }
                                match result {
                                    ExecResult::Window(action) => window_action = Some(action),
                                    ExecResult::Fg => fg_pending = true,
                                    ExecResult::Edit => edit_pending = true,
                                    // The exit trap already ran where this
                                    // came from (see `ExecResult::Exit`); any
                                    // session's `exit` ends the process.
                                    ExecResult::Exit(code) => {
                                        session::stop_listening();
                                        // The terminal's title, restored: this
                                        // path never reaches the end of `run`.
                                        term::pop_window_title();
                                        std::process::exit(code)
                                    }
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
                    apply_window_action(app, action);
                } else if fg_pending {
                    // A pty-attached background job was fg'd (see
                    // `ExecResult::Fg`). Pushed as a `Frame::Job` on the
                    // focused stack (the frame holds an id into
                    // `app.job_frames`; see `Frame`) and driven by
                    // `run_fg_job_frame`, as the loop's top does for a job
                    // returned to.
                    let fg_job =
                        app.sessions.get_mut(&session_id).unwrap().shell.take_pending_fg().expect("ExecResult::Fg implies a job was stashed");
                    let job_frame_id = app.next_job_frame_id;
                    app.next_job_frame_id += 1;
                    app.job_frames.insert(job_frame_id, fg_job);
                    app.windows[app.current_window].stack_mut().push(Frame::Job(job_frame_id));

                    run_fg_job_frame(app, job_frame_id, session_id);
                } else if edit_pending {
                    // `e [file]` (see `ExecResult::Edit`). Promoted first,
                    // always: unlike a job, which only needs the compositor
                    // once promoted, `e` always renders through the
                    // compositor and pane rects, one code path.
                    ensure_promoted(&mut app.sessions, &mut app.sinks_are_grid);
                    let args = app.sessions.get_mut(&session_id).unwrap().shell.take_pending_edit();
                    let opened = match fileeditor::parse_edit_args(&args) {
                        Ok(targets) => {
                            // A directory argument becomes what's picked
                            // from it, before anything opens.
                            let targets = expand_browse_targets(app, targets, true, session_id);
                            // Read now: browsing waits on input, and the
                            // terminal may have been resized.
                            let rect = pane_rect(
                                &app.windows[app.current_window],
                                app.windows[app.current_window].focused_pane,
                                app.term_rows,
                                app.term_cols,
                            );
                            open_edit_targets(app, &targets, session_id, rect)
                        }
                        Err(e) => {
                            app.sessions.get_mut(&session_id).unwrap().shell.sink_err(&format!("bish: e: {}\n", e));
                            Vec::new()
                        }
                    };
                    if opened.is_empty() {
                        // Nothing opened (a bad flag, or every file
                        // failed): the pane keeps its shell, with the
                        // messages in it.
                        compositor_redraw(app);
                    }
                    // Pushed back to front so the first file named is on
                    // top: `e a b` shows `a`, then `b` once it's closed, as
                    // `vim a b` does. Not driven here: the loop's top
                    // dispatches on the frame kind, which also hands
                    // control to the next file when one closes.
                    for frame in opened.iter().rev() {
                        app.windows[app.current_window].stack_mut().push(*frame);
                    }
                } else if app.sinks_are_grid {
                    // Once promoted, the usual case: a command ran and its
                    // output went to the session's grid, so show it. Only
                    // `fg` streams live; this redraws once the command
                    // finished.
                    compositor_redraw(app);
                }
            }
            Err(e) => {
                app.sessions.get(&session_id).unwrap().shell.sink_err(&format!("bish: error reading input: {}\n", e));
                break;
            }
        }
        let _ = io::stdout().flush();
    }
    // The protocol's goodbye to every language server. `Drop` would kill
    // them anyway (an orphaned rust-analyzer indexes forever), but a server
    // told to shut down writes its cache, for a faster next start.
    if let Some(lsp) = app.sessions.values().next().map(|s| Rc::clone(&s.lsp)) {
        lsp.borrow_mut().shutdown_all();
    }
    term::pop_window_title();
}

// What a failed `:w`, `:wq` or `:x` says. A read-only buffer gets vim's
// E45 rather than E212's "can't open file for writing", which would be
// wrong (nothing was opened); read-only stops overwriting this file,
// not writing the text elsewhere (`TextBuffer::save`).
fn write_error(tb: &TextBuffer, e: &io::Error) -> String {
    if e.kind() == io::ErrorKind::PermissionDenied && tb.is_readonly() {
        return "bish: E45: 'readonly' option is set (:w FILE writes it elsewhere)".to_string();
    }
    format!("bish: E212: Can't open file for writing: {e}")
}

// `e DIR`: a directory is a place to pick buffers from, so it's replaced,
// before anything opens, by one target per path chosen in the browser,
// each with the directory argument's flags (`e --hex dumps/` opens the
// picks as hex). A directory picked with Tab is browsed in turn. Esc or
// `q` contributes nothing. A non-directory passes through.
fn expand_browse_targets(
    app: &mut App,
    targets: Vec<fileeditor::EditTarget>,
    // Whether the browser may change the shell's directory: not for
    // `bish tool edit`, a one-shot command (see `browser::Outcome`).
    can_change_directory: bool,
    session_id: SessionId,
) -> Vec<fileeditor::EditTarget> {
    // Only look up the cwd (and stat) when there's something to resolve.
    if targets.iter().all(|t| t.path.is_none()) {
        return targets;
    }
    let cwd = app.sessions[&session_id].shell.cwd.clone();
    let mut out = Vec::new();
    // A worklist, so a directory picked in the browser means what one named
    // on the command line does: pick from it, until every argument is files
    // or nothing. Expansions go back on the front, so `e a.sh dir/ b.sh`
    // keeps `dir/`'s files between the two.
    let mut queue: std::collections::VecDeque<fileeditor::EditTarget> = targets.into();
    while let Some(target) = queue.pop_front() {
        let Some(path) = target.path.clone() else {
            out.push(target);
            continue;
        };
        let start = browser::resolve_start(&cwd, Some(&path));
        // A zip, and a directory inside one, is browsed like a directory
        // (`archive::is_browsable`), so `e some.zip` shows its contents.
        // Except with `--hex`, which wants the archive's bytes; a directory
        // has none, so it still browses.
        let browsable = start.is_dir() || (!target.hex && archive::is_browsable(&start.to_string_lossy()));
        if !browsable {
            out.push(target);
            continue;
        }
        // `e` may be what promoted this session, with nothing painted yet, so
        // the tab bar is drawn first, as at every `ensure_promoted`.
        if app.sinks_are_grid {
            compositor_redraw(app);
        }
        let browsed = run_browse_frame(
            app,
            &start,
            can_change_directory,
            app.sessions.get(&session_id).is_none_or(|s| s.shell.bishopt_bool("gitignore")),
            session_id,
        );
        match browsed {
            Ok(BrowseOutcome::Chosen(chosen)) => {
                for picked in chosen.into_iter().rev() {
                    queue.push_front(fileeditor::EditTarget { path: Some(picked.display().to_string()), ..target.clone() });
                }
            }
            // Ctrl-Y: no files, the shell moved instead, through
            // `Shell::change_directory`, which refuses under `set -r`.
            Ok(BrowseOutcome::ChangeDirectory(dir)) => {
                let shell = &mut app.sessions.get_mut(&session_id).unwrap().shell;
                if let Err(e) = shell.change_directory(&dir) {
                    shell.sink_err(&format!("bish: e: cd: {}: {}\n", dir.display(), e));
                }
            }
            Ok(BrowseOutcome::Nothing) => {}
            Err(e) => {
                app.sessions.get_mut(&session_id).unwrap().shell.sink_err(&format!("bish: e: {}: {}\n", path, e));
            }
        }
    }
    out
}

// Opens every target `e` was given, in order, returning the frames that
// came up; shared by the `e` builtin and `bish tool edit`, which differ
// only in what they push and drive. A target that fails is reported and
// skipped, as vim does; the message goes to the session's output, under
// the editor frames.
fn open_edit_targets(app: &mut App, targets: &[fileeditor::EditTarget], session_id: SessionId, rect: Rect) -> Vec<Frame> {
    let mut opened = Vec::new();
    for target in targets {
        let result = open_one_edit_target(app, target, rect);
        match result {
            Ok(frame) => opened.push(frame),
            Err(e) => {
                let what = target.path.as_deref().unwrap_or("<unnamed>");
                app.sessions.get_mut(&session_id).unwrap().shell.sink_err(&format!("bish: e: {}: {}\n", what, e));
            }
        }
    }
    opened
}

// The `--hex` fork: a target becomes a `Frame::Edit` over a `TextBuffer`
// or a `Frame::Hex`. Separate so `bish tool edit` reuses it with its own
// failure reporting (`run_edit_impl`).
fn open_one_edit_target(app: &mut App, target: &fileeditor::EditTarget, rect: Rect) -> io::Result<Frame> {
    if target.hex {
        // An archive member has no path for the hex editor, so its bytes are
        // passed directly, read-only like the text editor's
        // (`fileeditor::compressed_text`).
        if let Some(path) = target.path.as_deref()
            && let Some(bytes) = fileeditor::compressed_bytes(path)
        {
            let bytes = bytes.map_err(io::Error::other)?;
            let session = hexedit::HexSession::from_bytes(bytes, std::path::Path::new(path), rect)?;
            let id = app.next_hex_frame_id;
            app.next_hex_frame_id += 1;
            app.hex_frames.insert(id, session);
            return Ok(Frame::Hex(id));
        }
        let session = hexedit::HexSession::open(target.path.as_deref().map(std::path::Path::new), rect, target.readonly)?;
        let id = app.next_hex_frame_id;
        app.next_hex_frame_id += 1;
        app.hex_frames.insert(id, session);
        return Ok(Frame::Hex(id));
    }
    let mut session = fileeditor::EditSession::open(target.path.as_deref(), normal_mode_content_rows(rect))?;
    // `--readonly` only adds the restriction; compressed content is
    // already read-only.
    if target.readonly {
        session.buffer.set_readonly(true);
    }
    let id = app.next_edit_frame_id;
    app.next_edit_frame_id += 1;
    app.edit_frames.insert(id, session);
    Ok(Frame::Edit(id))
}

// `bish tool edit FILE`: one session, one window, straight into a
// `Frame::Edit`, without the tab bar and window switching `run()` has.
// `run_edit_frame` is reused unchanged, so it's the same editor `e`
// drives. `ensure_promoted` switches to the alternate screen;
// `render_editor_frame` paints directly, so skipping `compositor_redraw`
// loses only the tab bar. A window command (`<C-w>`, `:split`) isn't
// blocked: `run_edit_frame` returns when focus leaves the pane, and this
// treats that as a quit.
pub fn run_edit(args: &[String]) -> i32 {
    match fileeditor::parse_edit_args(args) {
        Ok(targets) => run_edit_impl(&targets, false),
        Err(e) => {
            eprintln!("bish tool edit: {e}");
            2
        }
    }
}

// `bish tool debug FILE`: opens FILE as `run_edit` does and attaches a
// debug session, as `:dbg` does (see debugger.rs).
pub fn run_edit_debug(path: &str) -> i32 {
    run_edit_impl(&[fileeditor::EditTarget { path: Some(path.to_string()), ..Default::default() }], true)
}

// `attach_debug` comes with exactly one target, and attaches to it.
fn run_edit_impl(targets: &[fileeditor::EditTarget], attach_debug: bool) -> i32 {
    let mut shell = exec::Shell::new();
    shell.enable_monitor_mode();
    let root_cwd = shell.cwd.clone();

    let history_size = shell.bishopt_int("history_size").max(0) as usize;
    // Before the `SessionState` literal, which moves `shell` into its first
    // field before `history:` is evaluated.
    let edit_history = Rc::new(RefCell::new(History::load(".bish_history", history_size)));
    let (term_rows, term_cols) = query_term_size();

    let mut sessions: HashMap<SessionId, SessionState> = HashMap::new();
    let root_screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(term_rows), term_cols)));
    let root_lsp: Rc<RefCell<lspclient::Table>> = Rc::new(RefCell::new(lspclient::Table::default()));
    install_service_table(&mut shell, &root_lsp);
    sessions.insert(
        0,
        SessionState {
            shell,
            lsp: root_lsp,
            lsp_roots: HashMap::new(),
            lsp_semantic: HashMap::new(),
            lsp_semantic_applied: HashMap::new(),
            lsp_highlight: None,
            lsp_highlight_applied: None,
            lsp_inlay: None,
            lsp_inlay_applied: None,
            buffer: String::new(),
            buffer_unrecorded: false,
            history: edit_history,
            screen: root_screen,
            warned_stopped_jobs: false,
            dir_history: vec![root_cwd],
            dir_history_index: 0,
            command_transcript: Vec::new(),
        },
    );
    // The interactive shell's state (see `App`): `bish tool edit` is a
    // one-session shell that starts in the editor.
    let app = &mut App {
        sessions,
        pending_input: std::collections::VecDeque::new(),
        windows: vec![WindowEntry::single(0, Frame::Session(0))],
        current_window: 0,
        next_session_id: 1,
        next_window_id: 1,
        job_frames: HashMap::new(),
        next_job_frame_id: 1,
        edit_frames: HashMap::new(),
        next_edit_frame_id: 1,
        hex_frames: HashMap::new(),
        next_hex_frame_id: 1,
        debug_frames: HashMap::new(),
        location_lists: HashMap::new(),
        cmd_history: History::load(".bish_cmd_history", history_size),
        last_substitute: None,
        registers: Registers::new(),
        sinks_are_grid: false,
        file_watcher: crate::watch::Watcher::new().ok(),
        dap_frames: HashMap::new(),
        focus: None,
        pending_reviews: Vec::new(),
        term_rows,
        term_cols,
    };

    ensure_promoted(&mut app.sessions, &mut app.sinks_are_grid);

    // `bish tool edit some-dir/` browses it, as the builtin does; `rect` is
    // read afterwards, since browsing waits on input.
    let targets = expand_browse_targets(
        app,
        targets.to_vec(),
        // `bish tool edit` exits with the editor, so no directory change.
        false,
        0,
    );
    let rect = pane_rect(&app.windows[app.current_window], app.windows[app.current_window].focused_pane, app.term_rows, app.term_cols);
    // Unlike the `e` builtin, a failure aborts: a one-shot command line's
    // bad path is a usage error deserving an exit status.
    let mut opened: Vec<Frame> = Vec::new();
    for target in &targets {
        match open_one_edit_target(app, target, rect) {
            Ok(frame) => opened.push(frame),
            Err(e) => {
                // Leave the alternate screen `promote_if_needed` entered.
                print!("\x1b[?1049l");
                let _ = io::stdout().flush();
                eprintln!("bish: {}: {}", target.path.as_deref().unwrap_or("<unnamed>"), e);
                return 1;
            }
        }
    }
    if attach_debug {
        // Only ever one target here (see run_edit_debug), always a text
        // one -- `bish tool debug` has no `--hex` of its own to parse.
        let edit_frame_id: EditFrameId = 1;
        let path = targets[0].path.as_deref().unwrap_or_default();
        match debugger::DebugSession::attach(std::path::Path::new(path)) {
            Ok(debug_session) => {
                app.edit_frames.get_mut(&edit_frame_id).unwrap().buffer.set_readonly(true);
                app.debug_frames.insert(edit_frame_id, debug_session);
                let (pane_id, _screen) = split_debug_run_pane(app, edit_frame_id);
                let sid = app.windows[app.current_window].pane(pane_id).owning_session();
                render_debug_run_title(&app.sessions[&sid].screen, app.term_cols, "attached -- :dbg run to start");
            }
            Err(e) => {
                print!("\x1b[?1049l");
                let _ = io::stdout().flush();
                eprintln!("bish: dbg: {}: {}", path, e);
                return 1;
            }
        }
    }
    // Back-to-front, so the first file named is the one on top -- see
    // the `e` builtin's own identical push in run().
    for frame in opened.iter().rev() {
        app.windows[app.current_window].stack_mut().push(*frame);
    }

    // Loops rather than driving `edit_frame_id` once: with several files
    // open, closing the top one has to hand control to the next frame
    // down, which is exactly what `run()`'s own main loop does for the
    // builtin. Terminates because run_edit_frame only ever returns once
    // its own frame is no longer this pane's top one (it pops on `:q`,
    // and keeps driving through a no-op detach itself -- see its own
    // `Detached` arm).
    loop {
        match *app.windows[app.current_window].stack().last().unwrap() {
            Frame::Edit(id) => run_edit_frame(app, id, 0),
            Frame::Hex(id) => run_hex_frame(app, id, 0),
            _ => break,
        }
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
fn handle_command_mode(
    app: &mut App,
    session_id: SessionId,
    editing: Option<&mut TextBuffer>,
    seed: Option<String>,
    // Keys a mapping produced that this colon line has to finish
    // delivering -- see editor::read_line's own `queued` parameter.
    queued: Vec<Key>,
    // The `/` pattern most recently searched for, which lives on the
    // caller's own `VimKeys` -- `:s//repl/` and the `r` flag are the
    // only things in here that want it, and a caller with no search
    // history of its own passes "".
    last_search: &str,
) -> CommandModeOutcome {
    let outcome = run_command_mode(app, session_id, editing, seed, queued, last_search);
    match outcome {
        CommandModeOutcome::Action(ref action) => {
            apply_window_action(app, action.clone());
        }
        CommandModeOutcome::Quit
        | CommandModeOutcome::Cancelled
        | CommandModeOutcome::Ran { .. }
        | CommandModeOutcome::Symbols(_)
        | CommandModeOutcome::RunNormal(_)
        | CommandModeOutcome::ConfirmSubstitute(_)
        | CommandModeOutcome::NoHighlight => {
            if app.sinks_are_grid {
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
                compositor_redraw(app);
            }
        }
    }
    outcome
}

// Drives the job behind `job_frame_id`, just pushed or left from a
// detach, through `drive_fg_job`, and handles the ending: an exit pops
// the frame and records the status; a detach puts the still-running job
// back in `job_frames` and enters Normal mode for this window, so
// `window next`/`previous` can leave it.
fn run_fg_job_frame(app: &mut App, job_frame_id: JobFrameId, session_id: SessionId) {
    // Taken out of `app.job_frames` so `on_idle` can borrow the map to
    // service other windows' jobs (`service_background_jobs`).
    let mut job = app.job_frames.remove(&job_frame_id).expect("Frame::Job always has a live app.job_frames entry");
    let focused_screen = app.sessions[&session_id].screen.clone();
    let mut tab_bar = tab_bar_line(app);
    sync_terminal_title(app);
    let mut layout = snapshot_window(&app.windows[app.current_window], &app.sessions, app.term_rows, app.term_cols);
    // Kept across `drive_fg_job` calls, so a quiet job's output diffs
    // against what's on screen (`render_compositor_frame_diff`); reset only
    // on `FgOutcome::Resized`, forcing a full repaint.
    let mut frame_cache: Option<TerminalFrame> = None;
    // A loop: a click meant for the job (`FgOutcome::MouseClick`) or a
    // resize (`FgOutcome::Resized`) is handled in place and the same job
    // driven on; any other outcome returns.
    loop {
        // A copy, not `&mut app.term_rows`: `on_idle` needs that as a real
        // `&mut usize` for resizing, and capturing it here too would
        // conflict. A resize mid-drive is caught by `drive_fg_job`, and
        // this copy is renewed by the `Resized` arm.
        let redraw_rows = app.term_rows;
        let redraw_cols = app.term_cols;
        let outcome = drive_fg_job(
            &mut job,
            &focused_screen,
            || render_compositor_frame_diff(&layout, &tab_bar, redraw_rows, redraw_cols, &mut frame_cache),
            || {
                service_background_jobs(app);
            },
        );
        match outcome {
            FgOutcome::Exited(status) => {
                app.windows[app.current_window].stack_mut().pop();
                let session = app.sessions.get_mut(&session_id).unwrap();
                session.shell.last_status = status;
                // The job is over, so a `<( )` feeding it can be torn down
                // (`Shell::drain_proc_subs`).
                session.shell.finished_foreground_job();
                if app.sinks_are_grid {
                    compositor_redraw(app);
                }
                return;
            }
            FgOutcome::Detached => {
                app.job_frames.insert(job_frame_id, job);
                // Normal mode, as the detach key gives from an idle prompt
                // (`ReadOutcome::NormalMode`); `:` reaches command mode
                // from there. `NavStart::JobDetach`: the top frame is the
                // job, so any "resume editing" result is discarded, and the
                // loop re-drives the job.
                let _ = run_normal_mode_navigation(app, session_id, None, NavStart::JobDetach, None);
                // Back to driving the job, which redraws only on new
                // output, so the excursion's mode line would linger;
                // erased.
                if app.sinks_are_grid {
                    compositor_redraw(app);
                }
                return;
            }
            FgOutcome::Stopped => {
                // Not back into `job_frames`: it's a Stopped job now,
                // reachable with `jobs`, `fg` and `bg`
                // (`Shell::park_stopped_fg_job`).
                app.windows[app.current_window].stack_mut().pop();
                let session = app.sessions.get_mut(&session_id).unwrap();
                let (id, cmd_text) = session.shell.park_stopped_fg_job(job);
                session.shell.sink_err(&format!("\n[{}]+  Stopped                 {}\n", id, cmd_text));
                session.shell.last_status = 148;
                if app.sinks_are_grid {
                    compositor_redraw(app);
                }
                return;
            }
            FgOutcome::MouseClick(ev) => {
                // Before promotion there's nothing to hit-test, so the
                // click goes to the job.
                let target = if app.sinks_are_grid { hit_test_click(app, ev) } else { None };
                match target {
                    Some(ClickTarget::Window(idx)) if idx != app.current_window => {
                        app.job_frames.insert(job_frame_id, job);
                        freeze_focused_idle_prompt(app);
                        app.current_window = idx;
                        if app.sinks_are_grid {
                            compositor_redraw(app);
                        }
                        return;
                    }
                    Some(ClickTarget::Pane(pane_id)) if pane_id != app.windows[app.current_window].focused_pane => {
                        app.job_frames.insert(job_frame_id, job);
                        app.windows[app.current_window].focused_pane = pane_id;
                        if app.sinks_are_grid {
                            compositor_redraw(app);
                        }
                        return;
                    }
                    // A miss, or the focused tab or pane: the click was for
                    // the job. The SGR press `drive_fg_job` held back is
                    // rebuilt and forwarded, and the loop drives on.
                    _ => {
                        let seq = format!("\x1b[<{};{};{}M", ev.button, ev.col, ev.row);
                        let _ = job.pty_master().write_all(seq.as_bytes());
                    }
                }
            }
            FgOutcome::Resized => {
                // `poll_and_apply_resize` resized every session's screen
                // and background job's pty, and `drive_fg_job` this job's
                // pty. What's left is this function's pane rects and tab
                // bar snapshot. No forced redraw: the job's next output,
                // prompt after `SIGWINCH`, redraws.
                layout = snapshot_window(&app.windows[app.current_window], &app.sessions, app.term_rows, app.term_cols);
                tab_bar = tab_bar_line(app);
                sync_terminal_title(app);
                // The cache describes the old shape; resetting it makes the
                // next redraw a full repaint.
                frame_cache = None;
            }
        }
    }
}

// Editor frames (`Frame::Edit`, `Frame::Hex`) render full-screen over the
// shell and freeze into the session's grid when focus moves, which
// would destroy the scrollback beneath. So, like a full-screen program,
// they switch the grid to its alternate buffer on the way in and back on
// the way out; `?1049l` restores the cursor to where the next prompt
// goes. Both are idempotent, so entering on every refocus is fine.
fn enter_alt_screen(screen: &Rc<RefCell<vt100::Screen>>) {
    screen.borrow_mut().feed(b"\x1b[?1049h");
}

fn leave_alt_screen(screen: &Rc<RefCell<vt100::Screen>>) {
    screen.borrow_mut().feed(b"\x1b[?1049l");
}

// `e --hex`'s counterpart to `run_edit_frame`: drives the hex session
// behind `hex_frame_id`. Not through `run_normal_mode_navigation`, which
// works on a `NavBuffer` (a `TextBuffer` or a grid); hexedit.rs owns the
// view, and this provides the rect, moves state and registers in and
// out, and handles the ending, as `run_diagnostics_frame` does. No mouse
// reporting: clicks mean nothing in a hex dump.
fn run_hex_frame(app: &mut App, hex_frame_id: HexFrameId, session_id: SessionId) {
    // Taken out, as `run_edit_frame` does, leaving the map free for
    // `on_idle`.
    let mut session = app.hex_frames.remove(&hex_frame_id).expect("Frame::Hex always has a live app.hex_frames entry");
    // See `enter_alt_screen`.
    enter_alt_screen(&app.sessions[&session_id].screen);
    // The shared register table, moved in while this frame is driven (see
    // `HexSession`'s `registers`).
    session.attach_registers(&mut app.registers);
    // Captured before a window command can move focus, as in
    // `run_edit_frame`.
    let own_window = app.current_window;
    let own_pane_id = app.windows[app.current_window].focused_pane;

    let Ok(_guard) = term::RawGuard::enable(0) else {
        session.detach_registers(&mut app.registers);
        app.hex_frames.insert(hex_frame_id, session);
        return;
    };
    // The first entry can be where promotion happened, with a blank
    // alternate screen.
    compositor_redraw(app);

    let quit = loop {
        let rect = pane_rect(&app.windows[app.current_window], own_pane_id, app.term_rows, app.term_cols);
        {
            let frame = session.render(rect, app.term_rows, app.term_cols);
            record_paint(rect, &frame);
            print!("{frame}");
        }
        let _ = io::stdout().flush();

        // Waiting for input out here rather than in `read_key_idle`'s
        // `on_idle`, as `run_normal_mode_navigation` does: only here is
        // `session` free for the redraw a reattached client needs
        // (`compositor_redraw` alone paints this pane blank).
        while !term::stdin_ready(editor::IDLE_POLL_MS) {
            if service_background_jobs(app) {
                let rect = pane_rect(&app.windows[app.current_window], own_pane_id, app.term_rows, app.term_cols);
                {
                    let frame = session.render(rect, app.term_rows, app.term_cols);
                    record_paint(rect, &frame);
                    print!("{frame}");
                }
                let _ = io::stdout().flush();
            }
        }
        let key = match editor::read_key_idle(&mut || {}) {
            Ok(Some(k)) => k,
            // EOF or an error: drop the frame, as `run_edit_frame`'s Err
            // arm does.
            Ok(None) | Err(_) => break true,
        };

        // Cloned, since the idle hook needs all of `app`; a `History` clone
        // is O(1).
        let history = app.cmd_history.clone();
        let outcome = session.handle_key(key, rect, app.term_rows, app.term_cols, &history, &mut || {
            service_background_jobs(app);
        });
        match outcome {
            hexedit::HexOutcome::Continue => {}
            // This pane repaints on the next `session.render`; everything
            // else is redrawn here.
            hexedit::HexOutcome::Redraw => {
                compositor_redraw(app);
            }
            hexedit::HexOutcome::Quit => break true,
            // Like `Window` below: if putting the opener away left this
            // frame on top, keep driving it.
            hexedit::HexOutcome::Opener => {
                if let Some((target, destination)) = run_opener(app) {
                    act_on_opener_choice(app, target, destination, None);
                }
                if app.windows[app.current_window].stack().last() != Some(&Frame::Hex(hex_frame_id)) {
                    break false;
                }
                compositor_redraw(app);
            }
            hexedit::HexOutcome::Window(cmd, count) => {
                dispatch_window_cmd(app, cmd, count);
                // A `<C-w>` that changed nothing for this pane leaves the
                // frame on top; keep driving, as `run_edit_frame`'s
                // Detached arm does.
                if app.windows[app.current_window].stack().last() != Some(&Frame::Hex(hex_frame_id)) {
                    break false;
                }
            }
        }
    };

    session.detach_registers(&mut app.registers);
    if quit {
        app.windows[app.current_window].stack_mut().pop();
        // `run_edit_frame`'s exit cleanup, in its order: back to the
        // primary screen with the scrollback, then erase the colon line's
        // text from the status row, before the redraw, whose closing cursor
        // placement the prompt uses.
        leave_alt_screen(&app.sessions[&session_id].screen);
        print!("{}{}", fileeditor::CURSOR_SHAPE_RESET, erase_global_status_row(app.term_rows));
        if app.sinks_are_grid {
            compositor_redraw(app);
        }
        let _ = io::stdout().flush();
        return;
    }

    // Focus moved: freeze this frame into its pane's grid so a redraw shows
    // the dump, and put the session back. The rect is recomputed: the
    // window command may have resized panes.
    let rect = if own_window < app.windows.len() && app.windows[own_window].panes.iter().any(|p| p.id == own_pane_id) {
        pane_rect(&app.windows[own_window], own_pane_id, app.term_rows, app.term_cols)
    } else {
        pane_rect(&app.windows[app.current_window], app.windows[app.current_window].focused_pane, app.term_rows, app.term_cols)
    };
    // Resized first: a split made this pane narrower, and freezing a
    // full-width frame into the old grid would clip it, as
    // `refresh_editor_frame` avoids.
    app.sessions[&session_id].screen.borrow_mut().resize(rect.rows, rect.cols);
    session.freeze(&app.sessions[&session_id].screen, rect);
    app.hex_frames.insert(hex_frame_id, session);
    // `dispatch_window_cmd` redrew before the freeze, so another is needed
    // or this pane stays blank (`<C-w>s` from a hex frame).
    if app.sinks_are_grid {
        compositor_redraw(app);
    }
    // The status row still holds this frame's status line; a prompt doesn't
    // draw over it, so it's erased.
    print!("{}{}", fileeditor::CURSOR_SHAPE_RESET, erase_global_status_row(app.term_rows));
    let _ = io::stdout().flush();
}

// `e`'s counterpart to `run_fg_job_frame`: drives the editor session
// behind `edit_frame_id` through `run_normal_mode_navigation` (an editor
// pane is the same Normal mode; see `NavBuffer`). This provides the rect,
// moves state in and out, and handles the ending.
fn run_edit_frame(app: &mut App, edit_frame_id: EditFrameId, session_id: SessionId) {
    // Taken out, as `run_fg_job_frame` does, so `on_idle` could borrow the
    // map.
    let session = app.edit_frames.remove(&edit_frame_id).expect("Frame::Edit always has a live app.edit_frames entry");
    // Drawn on the alternate screen, not over the scrollback (see
    // `enter_alt_screen`).
    enter_alt_screen(&app.sessions[&session_id].screen);
    // `%` refreshed when this starts driving a session; `:w`, `:wq` and `:x`
    // refresh it too (`fileeditor::set_last_filename`).
    fileeditor::set_last_filename(&session.buffer, &mut app.registers);
    let mut buffer = session.buffer;
    let mut vk = session.vk;
    // Starting values: `:bishopt --set wrap on` can change them mid-edit,
    // and `run_normal_mode_navigation` re-reads them after every command.
    let color_overrides = syntax_color_overrides(&app.sessions[&session_id].shell);
    apply_view_options(&app.sessions[&session_id].shell, &mut buffer);
    // The buffer exists with its options resolved: `editor:file:open`'s
    // moment, so a hook configuring a language server has a buffer.
    run_hooks(&mut app.sessions, session_id, "editor:file:open", &buffer);
    open_language_server_document(&mut app.sessions, session_id, &buffer);
    // Where this frame lives, captured before a window command can move
    // focus: the Detached arm freezes into this pane's rect, not whatever is
    // focused after the command (a split's new pane, another window).
    let own_window = app.current_window;
    let own_pane_id = app.windows[app.current_window].focused_pane;
    // A loop: a `Window` key always ends `run_normal_mode_navigation` as
    // `NavExit::Detached`, even when the action changed nothing for this
    // pane (`<C-w>n` with one window, `<C-w>h` with nowhere to go). See the
    // `Detached` arm for why this frame, still on top, must keep being
    // driven.
    loop {
        let outcome =
            run_normal_mode_navigation(app, session_id, Some(edit_frame_id), NavStart::Edit(Box::new(buffer), Box::new(vk)), Some(&color_overrides));
        match outcome {
            Ok((NavExit::Quit, ref state)) => {
                // Before the frame goes, while there's a buffer for a hook
                // to see, as vim's BufUnload and BufDelete are.
                if let Some((tb, _)) = state {
                    run_hooks(&mut app.sessions, session_id, "editor:file:close", tb);
                    // Same moment: the buffer's URI still resolves.
                    close_language_server_document(&mut app.sessions, session_id, tb);
                }
                app.windows[app.current_window].stack_mut().pop();
                // Back to the primary screen with the scrollback, cursor
                // where the next prompt goes (`enter_alt_screen`).
                leave_alt_screen(&app.sessions[&session_id].screen);
                // Close a `:diag` pane below the file too, rather than
                // orphan it.
                if let Some(diag_pane) = diagnostics_sibling(&app.windows[app.current_window], edit_frame_id) {
                    close_pane(&mut app.windows[app.current_window], diag_pane);
                    close_orphaned_sessions(&mut app.sessions, &app.windows);
                }
                // And a `gr` location list, whose entries refer to a frame
                // id about to be reused.
                if let Some(pane) = locations_sibling(&app.windows[app.current_window], edit_frame_id) {
                    close_pane(&mut app.windows[app.current_window], pane);
                    close_orphaned_sessions(&mut app.sessions, &app.windows);
                }
                app.location_lists.remove(&edit_frame_id);
                // And an attached `:dbg` session's DebugRun pane: its
                // buffer is going.
                if app.debug_frames.remove(&edit_frame_id).is_some()
                    && let Some(debug_pane) = debug_run_sibling(&app.windows[app.current_window], edit_frame_id)
                {
                    close_pane(&mut app.windows[app.current_window], debug_pane);
                    close_orphaned_sessions(&mut app.sessions, &app.windows);
                }
                // The colon line (`:q`, `:wq`) paints the global status row
                // directly, outside the grid, so the compositor's repaint
                // never touches it and "q!" would stay. Erased before the
                // redraw, whose closing cursor placement the prompt draws
                // against.
                print!("{}{}", fileeditor::CURSOR_SHAPE_RESET, erase_global_status_row(app.term_rows));
                if app.sinks_are_grid {
                    compositor_redraw(app);
                }
                let _ = io::stdout().flush();
                return;
            }
            // `gd` landed in another file. This one is stashed as for a
            // detach, the target opens as a new frame on top of the pane's
            // stack, and the main loop drives it, so `:q` there pops
            // straight back here.
            Ok((NavExit::OpenAt { path, line, character, encoding, root }, state)) => {
                if let Some((b, v)) = state {
                    app.edit_frames.insert(edit_frame_id, fileeditor::EditSession { buffer: b, vk: v });
                }
                // Already open in this pane: bring that frame to the front
                // rather than open the file twice. Nothing closes, so
                // `Ctrl-I` can step forward again and unsaved work is never
                // dropped.
                let existing = app.windows[own_window].pane(own_pane_id).stack.iter().position(
                    |frame| matches!(frame, Frame::Edit(id) if app.edit_frames.get(id).and_then(|s| s.buffer.path()) == Some(path.as_path())),
                );
                if let Some(at) = existing {
                    let stack = &mut app.windows[own_window].pane_mut(own_pane_id).stack;
                    let frame = stack.remove(at);
                    stack.push(frame);
                    if let Frame::Edit(id) = frame
                        && let Some(session) = app.edit_frames.get_mut(&id)
                    {
                        let starts = fileeditor::line_starts_of(&session.buffer);
                        let offset = fileeditor::diagnostic_offset(&session.buffer, &starts, line, character, encoding);
                        let (row, col) = fileeditor::diagnostic_position(&session.buffer, offset);
                        session.buffer.set_cursor(row, col);
                    }
                    return;
                }
                let rect = pane_rect(&app.windows[own_window], own_pane_id, app.term_rows, app.term_cols);
                match fileeditor::EditSession::open(path.to_str(), normal_mode_content_rows(rect)) {
                    Ok(mut opened) => {
                        // Converted here: a `(line, character)` becomes a
                        // cursor only with this file's text, which didn't
                        // exist until now.
                        let starts = fileeditor::line_starts_of(&opened.buffer);
                        let offset = fileeditor::diagnostic_offset(&opened.buffer, &starts, line, character, encoding);
                        let (row, col) = fileeditor::diagnostic_position(&opened.buffer, offset);
                        opened.buffer.set_cursor(row, col);
                        opened.buffer.lsp_root = root;
                        let id = app.next_edit_frame_id;
                        app.next_edit_frame_id += 1;
                        app.edit_frames.insert(id, opened);
                        app.windows[own_window].pane_mut(own_pane_id).stack.push(Frame::Edit(id));
                    }
                    Err(e) => {
                        app.sessions.get_mut(&session_id).unwrap().shell.sink_err(&format!("bish: gd: {}: {}\n", path.display(), e));
                    }
                }
                return;
            }
            Ok((NavExit::Detached, Some((b, v)))) => {
                // This frame still tops the focused pane, so nothing took
                // over. Freezing would bake its content into the grid,
                // where nothing updates it while an editor is driven (see
                // `freeze_editor_frame`), and a later redraw would paint it
                // over whatever a real exit draws. Keep driving.
                if app.windows[app.current_window].stack().last() == Some(&Frame::Edit(edit_frame_id)) {
                    buffer = b;
                    vk = v;
                    continue;
                }
                // A real focus change, or the pane shows something else now
                // (`window fg` pushed a frame): freeze. The rect is
                // recomputed, since window actions can resize panes
                // (`window balance`, `+`, `-`).
                let rect = if own_window < app.windows.len() && app.windows[own_window].panes.iter().any(|p| p.id == own_pane_id) {
                    pane_rect(&app.windows[own_window], own_pane_id, app.term_rows, app.term_cols)
                } else {
                    // This pane or its window is gone: fall back to where
                    // focus landed.
                    pane_rect(&app.windows[app.current_window], app.windows[app.current_window].focused_pane, app.term_rows, app.term_cols)
                };
                // Resized first: a split narrows this pane, and freezing a
                // full-width frame into the old grid would clip it, as
                // `refresh_editor_frame` avoids.
                app.sessions[&session_id].screen.borrow_mut().resize(rect.rows, rect.cols);
                fileeditor::freeze_editor_frame(&app.sessions[&session_id].screen, &b, &v, rect, Some(&color_overrides));
                app.edit_frames.insert(edit_frame_id, fileeditor::EditSession { buffer: b, vk: v });
                // `dispatch_window_cmd` redrew before the freeze, so
                // another is needed or the pane shows a blank alternate
                // screen; as in `run_hex_frame`.
                if app.sinks_are_grid {
                    compositor_redraw(app);
                }
                return;
            }
            Ok((NavExit::Resume(..), _)) | Ok((NavExit::Detached, None)) => {
                unreachable!("NavStart::Edit never produces NavExit::Resume, and always hands its buffer back")
            }
            Err(e) => {
                // An I/O error reading a key, treated as the main loop
                // treats one: sink it and drop the session.
                app.windows[app.current_window].stack_mut().pop();
                leave_alt_screen(&app.sessions[&session_id].screen);
                app.sessions.get_mut(&session_id).unwrap().shell.sink_err(&format!("bish: e: error reading input: {}\n", e));
                if app.sinks_are_grid {
                    compositor_redraw(app);
                }
                return;
            }
        }
    }
}

// The editor pane whose diagnostics `edit_frame_id` names, if live in
// this window: `diagnostics_sibling` the other way, for refocusing the
// editor when the diagnostics pane collapses.
fn editor_pane_for(window: &WindowEntry, edit_frame_id: EditFrameId) -> Option<PaneId> {
    window.panes.iter().find(|p| p.stack.last() == Some(&Frame::Edit(edit_frame_id))).map(|p| p.id)
}

// Extra rows while the selected item's detail shows (Space).
const DIAG_DETAIL_ROWS: usize = 2;
// Rows the diagnostics pane claims while focused: its list, plus
// `DIAG_DETAIL_ROWS` while a detail shows, capped at half of `budget`
// ("only the space it needs, up to 50%"). `budget` is the split's total
// (the editor's rows and the minimized pane's one divider row), captured
// when the pane is focused and kept, or the cap would shrink as the pane
// grew.
fn diagnostics_pane_rows(diagnostics_len: usize, show_detail: bool, budget: usize) -> usize {
    let max_rows = (budget / 2).max(1);
    let wanted = if show_detail { diagnostics_len + DIAG_DETAIL_ROWS } else { diagnostics_len };
    wanted.max(1).min(max_rows)
}

// What a diagnostics or locations pane does to its own geometry and its
// editor sibling. Plain functions over `&mut App`, which, being one
// borrow, doesn't conflict with the idle hook.
fn set_pane_expanded(app: &mut App, pane_id: PaneId, rows: usize) {
    if let Some((_, children, idx)) = find_parent_split_mut(&mut app.windows[app.current_window].layout, pane_id) {
        children[idx].minimized = false;
        children[idx].fixed = Some(rows);
    }
}

fn set_pane_minimized(app: &mut App, pane_id: PaneId) {
    if let Some((_, children, idx)) = find_parent_split_mut(&mut app.windows[app.current_window].layout, pane_id) {
        children[idx].minimized = true;
        children[idx].fixed = None;
    }
}

// Hands focus back to the editor pane a sibling pane belongs to.
fn focus_editor_pane(app: &mut App, edit_frame_id: EditFrameId) {
    if let Some(editor_pane) = editor_pane_for(app.window(), edit_frame_id) {
        app.window_mut().focused_pane = editor_pane;
    }
}

// Repaints the editor pane beside a diagnostics or locations pane at
// the size the pane's expansion left it.
fn refresh_editor_frame(app: &App, edit_frame_id: EditFrameId) {
    let Some(editor_pane) = editor_pane_for(app.window(), edit_frame_id) else { return };
    let Some(session) = app.edit_frames.get(&edit_frame_id) else { return };
    let rect = pane_rect(app.window(), editor_pane, app.term_rows, app.term_cols);
    let sid = app.window().pane(editor_pane).owning_session();
    let color_overrides = syntax_color_overrides(&app.sessions[&sid].shell);
    let screen = &app.sessions[&sid].screen;
    screen.borrow_mut().resize(rect.rows, rect.cols);
    fileeditor::freeze_editor_frame(screen, &session.buffer, &session.vk, rect, Some(&color_overrides));
}

// The diagnostics pane's interactive list, dispatched from `run`'s loop
// for a `Frame::Diagnostics` top frame. Not built on `Buffer` and
// `VimKeys` motions: a flat list needs no word motions, registers or
// Insert mode. A bare `VimKeys` parses `<C-w>` chords (to
// `dispatch_window_cmd`) and `j`/`k` with counts; every other key is
// handled before `vk.feed`. Selection and detail state are local, so
// each entry starts at row 0, collapsed.
fn run_diagnostics_frame(app: &mut App, pane_id: PaneId, edit_frame_id: EditFrameId) {
    // Raw mode for this loop's duration, as every key-reading loop holds:
    // otherwise the terminal stays as the last loop's guard left it,
    // cooked, echoing and line-buffered. On the unlikely failure, return
    // with the pane as it was.
    let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else { return };

    let mut vk = VimKeys::new();
    let mut selected: usize = 0;
    let mut expanded: Option<usize> = None;

    // Captured before the pane grows: see `diagnostics_pane_rows`.
    let budget = editor_pane_for(&app.windows[app.current_window], edit_frame_id)
        .map(|id| pane_rect(&app.windows[app.current_window], id, app.term_rows, app.term_cols).rows + 1)
        .unwrap_or(app.term_rows);

    let diagnostics_len = app.edit_frames.get(&edit_frame_id).map(|s| s.buffer.diagnostics.len()).unwrap_or(0);
    set_pane_expanded(app, pane_id, diagnostics_pane_rows(diagnostics_len, false, budget));
    refresh_editor_frame(app, edit_frame_id);
    compositor_redraw(app);

    loop {
        let rect = pane_rect(&app.windows[app.current_window], pane_id, app.term_rows, app.term_cols);
        if let Some(session) = app.edit_frames.get(&edit_frame_id) {
            render_diagnostics_list_frame(&session.buffer, rect, selected, expanded);
        }

        let key = match editor::read_key_idle(&mut || {
            service_background_jobs(app);
        }) {
            Ok(Some(k)) => k,
            // EOF or an error: leave the pane as it last rendered.
            Ok(None) | Err(_) => return,
        };

        if let Key::Mouse(ev) = key {
            if ev.is_left_click() {
                let row0 = (ev.row as usize).saturating_sub(1);
                let diagnostics_len = app.edit_frames.get(&edit_frame_id).map(|s| s.buffer.diagnostics.len()).unwrap_or(0);
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

        // Leaving: collapse to the title row and give focus to the editor
        // pane, refreshing this pane's title first (it matters only after
        // `f` changed the count).
        let leave = |app: &mut App, jump: bool| {
            let sid = app.windows[app.current_window].pane(pane_id).owning_session();
            let screen = app.sessions[&sid].screen.clone();
            if let Some(session) = app.edit_frames.get_mut(&edit_frame_id) {
                if jump && let Some(d) = session.buffer.diagnostics.get(selected).cloned() {
                    let (line, col) = fileeditor::diagnostic_position(&session.buffer, d.start);
                    session.buffer.set_cursor(line, col);
                }
                render_diagnostics_title(&screen, rect.cols, &session.buffer.diagnostics);
            }
            set_pane_minimized(app, pane_id);
            focus_editor_pane(app, edit_frame_id);
        };

        match key {
            Key::Enter => {
                leave(app, true);
                refresh_editor_frame(app, edit_frame_id);
                compositor_redraw(app);
                return;
            }
            Key::Escape | Key::Char('q') => {
                leave(app, false);
                refresh_editor_frame(app, edit_frame_id);
                compositor_redraw(app);
                return;
            }
            Key::Char(' ') => {
                expanded = if expanded == Some(selected) { None } else { Some(selected) };
                let diagnostics_len = app.edit_frames.get(&edit_frame_id).map(|s| s.buffer.diagnostics.len()).unwrap_or(0);
                set_pane_expanded(app, pane_id, diagnostics_pane_rows(diagnostics_len, expanded.is_some(), budget));
                refresh_editor_frame(app, edit_frame_id);
                compositor_redraw(app);
            }
            // `f` applies the selected item's fix, only while its detail,
            // with the `[f] autofix` hint, is showing. Re-diagnoses
            // afterwards (see `apply_fix`) and stays expanded, so several
            // fixes can follow.
            Key::Char('f') if expanded == Some(selected) => {
                let applied = app.edit_frames.get_mut(&edit_frame_id).map(|session| {
                    let Some(d) = session.buffer.diagnostics.get(selected).cloned() else { return false };
                    if !fileeditor::apply_fix(&mut session.buffer, &d) {
                        return false;
                    }
                    session.buffer.diagnostics = fileeditor::diagnose_buffer(&session.buffer);
                    true
                });
                if applied == Some(true) {
                    let diagnostics_len = app.edit_frames.get(&edit_frame_id).map(|s| s.buffer.diagnostics.len()).unwrap_or(0);
                    selected = selected.min(diagnostics_len.saturating_sub(1));
                    expanded = None;
                    set_pane_expanded(app, pane_id, diagnostics_pane_rows(diagnostics_len, false, budget));
                    refresh_editor_frame(app, edit_frame_id);
                    compositor_redraw(app);
                }
            }
            _ => match vk.feed(key) {
                KeyOutcome::Motion(motion::Motion::Down, count) => {
                    let diagnostics_len = app.edit_frames.get(&edit_frame_id).map(|s| s.buffer.diagnostics.len()).unwrap_or(0);
                    selected = (selected + count.unwrap_or(1).max(1)).min(diagnostics_len.saturating_sub(1));
                    expanded = None;
                    compositor_redraw(app);
                }
                KeyOutcome::Motion(motion::Motion::Up, count) => {
                    selected = selected.saturating_sub(count.unwrap_or(1).max(1));
                    expanded = None;
                    compositor_redraw(app);
                }
                KeyOutcome::Window(cmd, count) => {
                    dispatch_window_cmd(app, cmd, count);
                    return;
                }
                _ => {}
            },
        }
    }
}

// One row per location: where, and the source line if readable. The
// same shape as `render_diagnostics_list_frame` (selected row reversed,
// blanks padded), so the two panes behave alike.
fn render_locations_list_frame(list: &LocationList, rect: Rect, selected: usize) {
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
    if list.items.is_empty() {
        out.push_str(&format!("\x1b[{};{}H", rect.row + 1, rect.col + 1));
        out.push_str(&pad(&list.title, rect.cols));
        row = 1;
    } else {
        // Scrolled to keep the selection visible: a list up to
        // `MAX_LOCATIONS` is often taller than a pane capped at half the
        // window.
        let first = selected.saturating_sub(rect.rows.saturating_sub(1));
        for (i, item) in list.items.iter().enumerate().skip(first) {
            if row >= rect.rows {
                break;
            }
            let text = pad(&item.label, rect.cols);
            out.push_str(&format!("\x1b[{};{}H", rect.row + row + 1, rect.col + 1));
            if i == selected {
                out.push_str("\x1b[7m");
                out.push_str(&text);
                out.push_str("\x1b[0m");
            } else {
                out.push_str(&text);
            }
            row += 1;
        }
    }
    while row < rect.rows {
        out.push_str(&format!("\x1b[{};{}H", rect.row + row + 1, rect.col + 1));
        out.push_str(&" ".repeat(rect.cols));
        row += 1;
    }
    out.push_str("\x1b[?25l");
    record_paint(rect, &out);
    print!("{}", out);
    let _ = io::stdout().flush();
}

// The location pane's list, `run_diagnostics_frame`'s sibling with the
// same shape: a bare `VimKeys` for `<C-w>` and counted `j`/`k`, other keys
// handled first, selection local. Enter differs: a location may be in
// another file, so it may open one, hence the frame-id counter.
fn run_locations_frame(app: &mut App, pane_id: PaneId, edit_frame_id: EditFrameId) {
    let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else { return };
    let mut vk = VimKeys::new();
    let mut selected: usize = 0;

    let budget = editor_pane_for(&app.windows[app.current_window], edit_frame_id)
        .map(|id| pane_rect(&app.windows[app.current_window], id, app.term_rows, app.term_cols).rows + 1)
        .unwrap_or(app.term_rows);
    let len = app.location_lists.get(&edit_frame_id).map(|l| l.items.len()).unwrap_or(0);
    set_pane_expanded(app, pane_id, diagnostics_pane_rows(len, false, budget));
    refresh_editor_frame(app, edit_frame_id);
    compositor_redraw(app);

    loop {
        let rect = pane_rect(&app.windows[app.current_window], pane_id, app.term_rows, app.term_cols);
        if let Some(list) = app.location_lists.get(&edit_frame_id) {
            render_locations_list_frame(list, rect, selected);
        }
        let key = match editor::read_key_idle(&mut || {
            service_background_jobs(app);
        }) {
            Ok(Some(k)) => k,
            Ok(None) | Err(_) => return,
        };
        let len = app.location_lists.get(&edit_frame_id).map(|l| l.items.len()).unwrap_or(0);

        if let Key::Mouse(ev) = key {
            if ev.is_left_click() {
                let row0 = (ev.row as usize).saturating_sub(1);
                if row0 >= rect.row {
                    let first = selected.saturating_sub(rect.rows.saturating_sub(1));
                    let idx = first + (row0 - rect.row);
                    if idx < len {
                        selected = idx;
                    }
                }
            }
            continue;
        }

        // Collapse to the title row and focus the editor pane, as the
        // diagnostics pane does.
        let collapse = |app: &mut App| {
            if let Some(list) = app.location_lists.get(&edit_frame_id) {
                let sid = app.windows[app.current_window].pane(pane_id).owning_session();
                render_locations_title(&app.sessions[&sid].screen, rect.cols, &list.title);
            }
            set_pane_minimized(app, pane_id);
            focus_editor_pane(app, edit_frame_id);
        };

        match key {
            Key::Enter => {
                let chosen = app
                    .location_lists
                    .get(&edit_frame_id)
                    .and_then(|l| l.items.get(selected))
                    .map(|item| (item.path.clone(), item.line, item.character, item.encoding));
                collapse(app);
                if let Some((path, line, character, encoding)) = chosen {
                    let here = app.edit_frames.get(&edit_frame_id).and_then(|s| s.buffer.path().map(|p| p.to_path_buf()));
                    if here.as_deref() == Some(path.as_path()) {
                        // Already open: move its cursor, as the diagnostics
                        // pane does.
                        if let Some(session) = app.edit_frames.get_mut(&edit_frame_id) {
                            let starts = fileeditor::line_starts_of(&session.buffer);
                            let offset = fileeditor::diagnostic_offset(&session.buffer, &starts, line, character, encoding);
                            let (row, col) = fileeditor::diagnostic_position(&session.buffer, offset);
                            session.buffer.set_cursor(row, col);
                        }
                    } else {
                        open_location_frame(app, &path, line, character, encoding);
                    }
                }
                refresh_editor_frame(app, edit_frame_id);
                compositor_redraw(app);
                return;
            }
            Key::Escape | Key::Char('q') => {
                collapse(app);
                refresh_editor_frame(app, edit_frame_id);
                compositor_redraw(app);
                return;
            }
            _ => match vk.feed(key) {
                KeyOutcome::Motion(motion::Motion::Down, count) => {
                    selected = (selected + count.unwrap_or(1).max(1)).min(len.saturating_sub(1));
                    compositor_redraw(app);
                }
                KeyOutcome::Motion(motion::Motion::Up, count) => {
                    selected = selected.saturating_sub(count.unwrap_or(1).max(1));
                    compositor_redraw(app);
                }
                KeyOutcome::Window(cmd, count) => {
                    dispatch_window_cmd(app, cmd, count);
                    return;
                }
                _ => {}
            },
        }
    }
}

// Opens `path` as an Edit frame on the focused pane's stack with the
// cursor at the server position: `gd`'s shape, from the location pane.
fn open_location_frame(app: &mut App, path: &Path, line: usize, character: usize, encoding: crate::lsp::PositionEncoding) {
    open_file_frame(app, path, Some((line, character, encoding)));
}

/// `open_location_frame` without a position from a server: the file at
/// its top, as the opener wants.
fn open_file_frame(app: &mut App, path: &Path, at: Option<(usize, usize, crate::lsp::PositionEncoding)>) {
    let pane_id = app.windows[app.current_window].focused_pane;
    let rect = pane_rect(&app.windows[app.current_window], pane_id, app.term_rows, app.term_cols);
    match fileeditor::EditSession::open(path.to_str(), normal_mode_content_rows(rect)) {
        Ok(mut opened) => {
            if let Some((line, character, encoding)) = at {
                let starts = fileeditor::line_starts_of(&opened.buffer);
                let offset = fileeditor::diagnostic_offset(&opened.buffer, &starts, line, character, encoding);
                let (row, col) = fileeditor::diagnostic_position(&opened.buffer, offset);
                opened.buffer.set_cursor(row, col);
            }
            let id = app.next_edit_frame_id;
            app.next_edit_frame_id += 1;
            app.edit_frames.insert(id, opened);
            app.windows[app.current_window].pane_mut(pane_id).stack.push(Frame::Edit(id));
        }
        Err(e) => {
            let sid = app.windows[app.current_window].pane(pane_id).owning_session();
            app.sessions.get_mut(&sid).unwrap().shell.sink_err(&format!("bish: {}: {}\n", path.display(), e));
        }
    }
}

// How a browse ended. `Nothing` covers cancelling and EOF.
enum BrowseOutcome {
    Nothing,
    Chosen(Vec<std::path::PathBuf>),
    ChangeDirectory(std::path::PathBuf),
}

// `e DIR`'s browsing loop: browser.rs owns the listing, layout and
// rendering; this owns raw mode, keys and the `service_background_jobs`
// idle hook, so jobs drain and resizes are noticed while browsing. The
// focused pane's rect is recomputed every iteration, as a resize can
// land between keys. Restoring the pane is the caller's: it opens frames
// over it or redraws it.
fn run_browse_frame(
    app: &mut App,
    start: &std::path::Path,
    can_change_directory: bool,
    // The `gitignore` bishopt, read per browse, so `bishopt --set gitignore
    // off` applies to the next `e .`.
    honor_gitignore: bool,
    session_id: SessionId,
) -> Result<BrowseOutcome, String> {
    // Opened before raw mode, so an unreadable path is an ordinary error
    // rather than a flicker into the browser.
    let mut browser = browser::Browser::open(start)?;
    browser.set_can_change_directory(can_change_directory);
    browser.set_honor_gitignore(honor_gitignore)?;
    if let Some(session) = app.sessions.get(&session_id) {
        browser.set_colors(ui_colors(&session.shell));
    }
    // Raw mode, as in `run_diagnostics_frame`: the last loop left the
    // terminal cooked.
    let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else {
        return Err("not a terminal".to_string());
    };
    let pane_id = app.windows[app.current_window].focused_pane;

    let outcome = loop {
        let rect = pane_rect(&app.windows[app.current_window], pane_id, app.term_rows, app.term_cols);
        let frame = browser.render(rect, app.term_rows, app.term_cols);
        record_paint(rect, &frame);
        print!("{frame}");
        let _ = io::stdout().flush();

        let key = match editor::read_key_idle(&mut || {
            service_background_jobs(app);
        }) {
            Ok(Some(k)) => k,
            // EOF or an error: nothing chosen.
            Ok(None) | Err(_) => break BrowseOutcome::Nothing,
        };

        // `:` opens the same command mode the editor does, so `:q` leaves,
        // as from any pane takeover. `editing: None`, as there's no buffer,
        // makes `q` and `q!` leave this frame (see `run_command_mode`'s
        // `editing`).
        if key == editor::Key::Char(':') {
            // Raw mode is the colon line's while it runs.
            let outcome = handle_command_mode(
                app,
                session_id,
                None,
                None,
                // Nothing maps into the browser's colon line.
                Vec::new(),
                // No buffer, so no search for `:s//` to reach back to.
                "",
            );
            // Pick up what `:bishopt` changed before the next redraw, as the
            // editor's `:` arm does.
            let honor = app.sessions.get(&session_id).is_none_or(|s| s.shell.bishopt_bool("gitignore"));
            if let Err(e) = browser.set_honor_gitignore(honor) {
                browser.set_message(e);
            }
            match outcome {
                CommandModeOutcome::Quit => break BrowseOutcome::Nothing,
                // A command's output goes in the status row, where the
                // browser speaks.
                CommandModeOutcome::Ran { output, .. } if !output.trim().is_empty() => {
                    browser.set_message(output.trim().replace('\n', " "));
                }
                // `:normal` has no buffer to run against here.
                CommandModeOutcome::Ran { .. }
                | CommandModeOutcome::Cancelled
                | CommandModeOutcome::Action(_)
                | CommandModeOutcome::Symbols(_)
                | CommandModeOutcome::RunNormal(_)
                | CommandModeOutcome::ConfirmSubstitute(_)
                | CommandModeOutcome::NoHighlight => {}
            }
            // The colon line drew over the status row; the pane behind the
            // browser must come back before it repaints.
            if app.sinks_are_grid {
                compositor_redraw(app);
            }
            continue;
        }

        match browser.handle_key(key, rect) {
            browser::Outcome::Continue => {}
            browser::Outcome::Cancelled => break BrowseOutcome::Nothing,
            browser::Outcome::Accepted(paths) => break BrowseOutcome::Chosen(paths),
            browser::Outcome::ChangeDirectory(dir) => break BrowseOutcome::ChangeDirectory(dir),
        }
    };

    // `render` hides the cursor (no insertion point in a grid), so show it
    // again for the caller. The hint line is on the global status row,
    // outside every pane, which `compositor_redraw` can't clean, so it's
    // erased here.
    print!("\x1b[?25h{}{}", fileeditor::CURSOR_SHAPE_RESET, erase_global_status_row(app.term_rows));
    let _ = io::stdout().flush();
    Ok(outcome)
}

// Draws the diagnostics pane's expanded list straight to the terminal
// while it's the driven pane, as `render_normal_mode_frame` does. The
// cursor is hidden: there's no insertion point here.
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
                    out.push_str(&pad(&format!("  [{}] {}", d.label(), d.message), rect.cols));
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
    record_paint(rect, &out);
    print!("{}", out);
    let _ = io::stdout().flush();
}

// The `Frame::DebugRun` pane's idle view, when focused while nothing
// runs (a run drives the terminal itself; see debugger.rs). Just a
// status line and `<C-w>`, Escape or `q` to leave.
fn run_debug_run_frame(app: &mut App, pane_id: PaneId, edit_frame_id: EditFrameId) {
    let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else { return };
    let mut vk = VimKeys::new();

    // As in `run_diagnostics_frame`: repaint everything, or the editor pane
    // keeps its old, larger content.
    compositor_redraw(app);

    loop {
        let rect = pane_rect(&app.windows[app.current_window], pane_id, app.term_rows, app.term_cols);
        let status = if app.debug_frames.contains_key(&edit_frame_id) {
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
        record_paint(rect, &out);
        print!("{out}");
        let _ = io::stdout().flush();

        let key = match editor::read_key_idle(&mut || {}) {
            Ok(Some(k)) => k,
            Ok(None) | Err(_) => return,
        };
        match key {
            Key::Escape | Key::Char('q') => {
                focus_editor_pane(app, edit_frame_id);
                compositor_redraw(app);
                return;
            }
            _ => {
                if let KeyOutcome::Window(cmd, count) = vk.feed(key) {
                    dispatch_window_cmd(app, cmd, count);
                    return;
                }
            }
        }
    }
}

// The one-time switch to promoted mode, from two places: every window
// command (`apply_window_action`) and Ctrl+G's Normal mode, which doesn't
// go through command dispatch. `promote_if_needed` (the alternate
// screen) is idempotent and shared across sessions, so calling it again
// is harmless; the switch of every session's sink to its grid happens
// once, guarded by `sinks_are_grid`.
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

// Freezes the focused window's idle prompt into its grid
// (`freeze_idle_prompt`) if it's idle (a Session on top, not a running
// job), as `split_focused_pane` and `focus_pane_direction` do before
// moving focus. `apply_window_action` and Normal mode's `<C-w>gg`/`<C-w>G`
// call it first, since any action may move focus, and a prompt drawn
// only on the real terminal would otherwise be lost when coming back.
fn freeze_focused_idle_prompt(app: &mut App) {
    if matches!(app.windows[app.current_window].stack().last(), Some(Frame::Session(_))) {
        let sid = app.windows[app.current_window].owning_session();
        freeze_idle_prompt(app.sessions.get_mut(&sid).unwrap());
    }
}

// The `::bish window` script that rebuilds every window as it stands:
// name, pane tree, each pane's directory. Every window is re-created,
// not the first reused, so restoring adds rather than overwriting a
// layout. No sizes (see `WindowEntry::rebuild_commands`).
fn layout_script(app: &App) -> String {
    use crate::serialize::quote_literal;
    let mut out = String::from("# A bish layout. `::bish window restore <this file>` rebuilds it.\n");
    for window in &app.windows {
        out.push('\n');
        match &window.name {
            Some(name) => out.push_str(&format!("::bish window new --name {}\n", quote_literal(name))),
            None => out.push_str("::bish window new\n"),
        }
        for command in window.rebuild_commands() {
            out.push_str(&command);
            out.push('\n');
        }
        // Directories last, once every pane exists: a pane inherits the cwd
        // of the one it was split from.
        for (position, pane) in window.panes_in_layout_order().iter().enumerate() {
            let sid = window.pane(*pane).owning_session();
            let cwd = app.sessions[&sid].shell.cwd.to_string_lossy().to_string();
            out.push_str(&format!("::bish window focus {}\n", position + 1));
            out.push_str(&format!("cd {}\n", quote_literal(&cwd)));
        }
        // Leave this window on the pane that had focus.
        out.push_str(&format!("::bish window focus {}\n", window.focused_position()));
    }
    // Finally the window in front, by name, the only handle meaning the
    // same in another process.
    if let Some(name) = app.windows.get(app.current_window).and_then(|w| w.name.as_ref()) {
        out.push_str(&format!("\n::bish window select {}\n", quote_literal(name)));
    }
    out
}

// Hands a `window new <cmd>` or `window split <cmd>` command to the new
// session, to run at its next prompt like a typed line (see
// `App::pending_input`).
fn queue_pending_input(app: &mut App, command: Option<String>) {
    if let Some(command) = command {
        app.pending_input.push_back(command);
    }
}

// Performs a window action on the session and window state repl.rs owns
// (see `ExecResult::Window` for why `Shell` can't). Redraws afterwards for
// every action, a refused close included, so its message shows.
fn apply_window_action(app: &mut App, action: WindowAction) {
    ensure_promoted(&mut app.sessions, &mut app.sinks_are_grid);
    freeze_focused_idle_prompt(app);

    match action {
        WindowAction::Next => {
            app.current_window = (app.current_window + 1) % app.windows.len();
        }
        WindowAction::Previous => {
            app.current_window = (app.current_window + app.windows.len() - 1) % app.windows.len();
        }
        WindowAction::New { name, command } => {
            let parent_id = app.windows[app.current_window].owning_session();
            let child_history = Rc::new(RefCell::new(app.sessions[&parent_id].history.borrow().fork()));
            let mut child_shell = app.sessions[&parent_id].shell.new_virtual_child();
            // The same table every session sees, so a server started for
            // one pane serves all.
            let child_lsp = Rc::clone(&app.sessions[&parent_id].lsp);
            install_service_table(&mut child_shell, &child_lsp);
            install_history(&mut child_shell, &child_history);
            let screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(app.term_rows), app.term_cols)));
            child_shell.set_sink_grid(screen.clone());
            let child_cwd = child_shell.cwd.clone();
            let sid = app.next_session_id;
            app.next_session_id += 1;
            app.sessions.insert(
                sid,
                SessionState {
                    shell: child_shell,
                    lsp: child_lsp,
                    lsp_roots: HashMap::new(),
                    lsp_semantic: HashMap::new(),
                    lsp_semantic_applied: HashMap::new(),
                    lsp_highlight: None,
                    lsp_highlight_applied: None,
                    lsp_inlay: None,
                    lsp_inlay_applied: None,
                    buffer: String::new(),
                    buffer_unrecorded: false,
                    // A fork of the parent's `History`: the new pane's
                    // Up/Down sees what the parent could, then the two
                    // diverge.
                    history: Rc::clone(&child_history),
                    screen,
                    warned_stopped_jobs: false,
                    dir_history: vec![child_cwd],
                    dir_history_index: 0,
                    command_transcript: Vec::new(),
                },
            );
            let wid = app.next_window_id;
            app.next_window_id += 1;
            let mut window = WindowEntry::single(wid, Frame::Session(sid));
            window.name = name;
            app.windows.push(window);
            app.current_window = app.windows.len() - 1;
            queue_pending_input(app, command);
        }
        // Naming a window, listing them and selecting one: what makes a
        // workflow scriptable (see `WindowAction`).
        WindowAction::Rename(name) => app.windows[app.current_window].name = name,
        // `select` is checked in the builtin against the windows, so the
        // index is good here; a miss is an ordinary failing command.
        WindowAction::Select(index) => app.current_window = index.min(app.windows.len().saturating_sub(1)),
        WindowAction::Close => {
            if app.windows[app.current_window].stack().len() > 1 {
                // Pops a frame off the focused pane's stack: always fine,
                // and orphans nothing, as what's revealed was already live.
                app.windows[app.current_window].stack_mut().pop();
            } else if app.windows[app.current_window].panes.len() > 1 {
                // The focused pane is down to its session, but the window
                // has other panes: close just this pane rather than the
                // window.
                close_focused_pane(&mut app.windows[app.current_window]);
                close_orphaned_sessions(&mut app.sessions, &app.windows);
            } else if app.windows.len() == 1 {
                let sid = app.windows[app.current_window].owning_session();
                app.sessions[&sid].shell.sink_err("bish: window close: cannot close the last window -- exit the shell instead\n");
                compositor_redraw(app);
                return;
            } else {
                app.windows.remove(app.current_window);
                if app.current_window >= app.windows.len() {
                    app.current_window = app.windows.len() - 1;
                }
                close_orphaned_sessions(&mut app.sessions, &app.windows);
            }
        }
        WindowAction::FgSession(target_id) => {
            let target_frame = app.windows.iter().find(|w| w.id == target_id).map(|w| *w.stack().last().unwrap());
            let cur_sid = app.windows[app.current_window].owning_session();
            match target_frame {
                Some(frame @ Frame::Session(_)) => app.windows[app.current_window].stack_mut().push(frame),
                // A running job can be in one place at a time (see
                // `Frame`), unlike a session.
                Some(Frame::Job(_)) => {
                    app.sessions[&cur_sid].shell.sink_err("bish: window: fg: that window is running a job, not a session\n");
                }
                // Likewise an open editor session.
                Some(Frame::Edit(_) | Frame::Hex(_)) => {
                    app.sessions[&cur_sid].shell.sink_err("bish: window: fg: that window is running an editor, not a session\n");
                }
                // Likewise a diagnostics pane, which belongs below one Edit
                // frame's pane.
                Some(Frame::Diagnostics(_)) => {
                    app.sessions[&cur_sid].shell.sink_err("bish: window: fg: that window is showing diagnostics, not a session\n");
                }
                // Likewise a location list.
                Some(Frame::Locations(_)) => {
                    app.sessions[&cur_sid].shell.sink_err("bish: window: fg: that window is showing a location list, not a session\n");
                }
                // Likewise the debug-run pane.
                Some(Frame::DebugRun(_)) => {
                    app.sessions[&cur_sid].shell.sink_err("bish: window: fg: that window is running a debugged script, not a session\n");
                }
                None => {
                    app.sessions[&cur_sid].shell.sink_err(&format!("bish: window: fg: no such window: {}\n", target_id));
                }
            }
        }
        WindowAction::Zoom => {
            app.windows[app.current_window].toggle_zoom();
        }
        WindowAction::FocusPosition(position) => {
            if !app.windows[app.current_window].focus_position(position) {
                let sid = app.windows[app.current_window].owning_session();
                let panes = app.windows[app.current_window].panes.len();
                app.sessions[&sid].shell.sink_err(&format!("bish: window: focus: no pane {position} (this window has {panes})\n"));
            }
        }
        WindowAction::Save(path) => {
            let script = layout_script(app);
            let sid = app.windows[app.current_window].owning_session();
            match path {
                Some(path) => {
                    if let Err(e) = std::fs::write(&path, &script) {
                        app.sessions[&sid].shell.sink_err(&format!("bish: window: save: {path}: {e}\n"));
                    }
                }
                None => app.sessions[&sid].shell.sink_out(&script),
            }
        }
        WindowAction::Restore(path) => {
            let sid = app.windows[app.current_window].owning_session();
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    // Queued, not run here: each line is an ordinary
                    // command, and several move focus to the pane the
                    // previous one made (`App::pending_input`).
                    for line in text.lines() {
                        let line = line.trim();
                        if !line.is_empty() && !line.starts_with('#') {
                            app.pending_input.push_back(line.to_string());
                        }
                    }
                }
                Err(e) => app.sessions[&sid].shell.sink_err(&format!("bish: window: restore: {path}: {e}\n")),
            }
        }
        WindowAction::MoveWindow(where_to) => {
            let last = app.windows.len() - 1;
            let from = app.current_window;
            let to = match where_to {
                WindowMove::Left => from.saturating_sub(1),
                WindowMove::Right => (from + 1).min(last),
                // Clamped rather than refused: a number past the tab bar's
                // end means the end.
                WindowMove::To(position) => (position - 1).min(last),
            };
            if to != from {
                let window = app.windows.remove(from);
                app.windows.insert(to, window);
                app.current_window = to;
            }
        }
        WindowAction::SetLayout(named) => {
            let window = &mut app.windows[app.current_window];
            let named = named.unwrap_or_else(|| window.next_layout());
            window.apply_layout(named);
        }
        WindowAction::SwapPane => {
            let window = &mut app.windows[app.current_window];
            window.unzoom();
            let other = window.pane_after(window.focused_pane);
            window.swap_panes(window.focused_pane, other);
        }
        WindowAction::BreakPane => {
            let window = &mut app.windows[app.current_window];
            let focused = window.focused_pane;
            match window.detach_pane(focused) {
                Some(pane) => {
                    let wid = app.next_window_id;
                    app.next_window_id += 1;
                    app.windows.push(WindowEntry::from_pane(wid, pane));
                    app.current_window = app.windows.len() - 1;
                }
                // The pane already has a window to itself.
                None => {
                    let sid = app.windows[app.current_window].owning_session();
                    app.sessions[&sid].shell.sink_err(
                        "bish: window: break: this window has only one pane
",
                    );
                }
            }
        }
        WindowAction::JoinPane { from, horizontal } => {
            let source = app.windows.iter().position(|w| w.id == from);
            let cur_sid = app.windows[app.current_window].owning_session();
            match source {
                None => app.sessions[&cur_sid].shell.sink_err(&format!(
                    "bish: window: join: no such window: {from}
"
                )),
                Some(index) if index == app.current_window => {
                    app.sessions[&cur_sid].shell.sink_err(
                        "bish: window: join: that is this window
",
                    );
                }
                Some(index) => {
                    // A window down to its last pane goes with it:
                    // `detach_pane` refuses there (a window needs a pane),
                    // and leaving an empty window isn't what "join" means.
                    let pane = match app.windows[index].panes.len() {
                        1 => {
                            let mut emptied = app.windows.remove(index);
                            if index < app.current_window {
                                app.current_window -= 1;
                            }
                            emptied.panes.pop().expect("a window always has at least one pane")
                        }
                        _ => {
                            let focused = app.windows[index].focused_pane;
                            app.windows[index].detach_pane(focused).expect("more than one pane means one can be taken")
                        }
                    };
                    freeze_focused_idle_prompt(app);
                    let landed = app.windows[app.current_window].attach_pane(pane, horizontal);
                    app.windows[app.current_window].focused_pane = landed;
                }
            }
        }
        // Everything below moves focus or reshapes the layout, which a zoom
        // would hide, so each restores the split first.
        WindowAction::Split { horizontal, command } => {
            app.windows[app.current_window].unzoom();
            split_focused_pane(app, horizontal);
            queue_pending_input(app, command);
        }
        WindowAction::FocusPane(direction) => {
            app.windows[app.current_window].unzoom();
            focus_pane_direction(&mut app.windows[app.current_window], &mut app.sessions, direction, app.term_rows, app.term_cols);
        }
        WindowAction::Balance => {
            app.windows[app.current_window].unzoom();
            balance_panes(&mut app.windows[app.current_window].layout);
        }
        WindowAction::Minimize => {
            app.windows[app.current_window].unzoom();
            minimize_focused_pane(&mut app.windows[app.current_window]);
        }
        WindowAction::SizeUp => {
            app.windows[app.current_window].unzoom();
            resize_focused_pane(&mut app.windows[app.current_window], RESIZE_STEP);
        }
        WindowAction::SizeDown => {
            app.windows[app.current_window].unzoom();
            resize_focused_pane(&mut app.windows[app.current_window], -RESIZE_STEP);
        }
        WindowAction::SetSize(spec) => {
            app.windows[app.current_window].unzoom();
            set_focused_pane_size(&mut app.windows[app.current_window], spec, app.term_rows, app.term_cols);
        }
    }
    compositor_redraw(app);
}

// Splits the focused pane: the new half gets a cloned session (as
// `window new` makes) and focus. See `PaneLayout` for `horizontal`, and
// `insert_sibling` for why repeated splits in one direction stay flat.
fn split_focused_pane(app: &mut App, horizontal: bool) -> SessionId {
    let parent_id = app.windows[app.current_window].owning_session();
    let child_history = Rc::new(RefCell::new(app.sessions[&parent_id].history.borrow().fork()));
    let mut child_shell = app.sessions[&parent_id].shell.new_virtual_child();
    // The same table every session sees.
    let child_lsp = Rc::clone(&app.sessions[&parent_id].lsp);
    install_service_table(&mut child_shell, &child_lsp);
    install_history(&mut child_shell, &child_history);
    let screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(app.term_rows), app.term_cols)));
    child_shell.set_sink_grid(screen.clone());
    let child_cwd = child_shell.cwd.clone();
    let sid = app.next_session_id;
    app.next_session_id += 1;
    app.sessions.insert(
        sid,
        SessionState {
            shell: child_shell,
            lsp: child_lsp,
            lsp_roots: HashMap::new(),
            lsp_semantic: HashMap::new(),
            lsp_semantic_applied: HashMap::new(),
            lsp_highlight: None,
            lsp_highlight_applied: None,
            lsp_inlay: None,
            lsp_inlay_applied: None,
            buffer: String::new(),
            buffer_unrecorded: false,
            // A fork of the parent's `History`, as in `WindowAction::New`.
            history: Rc::clone(&child_history),
            screen,
            warned_stopped_jobs: false,
            dir_history: vec![child_cwd],
            dir_history_index: 0,
            command_transcript: Vec::new(),
        },
    );

    let window = &mut app.windows[app.current_window];
    let new_pane_id = window.add_pane(vec![Frame::Session(sid)], horizontal, false);

    // The split pane loses focus to its new sibling, so freeze its idle
    // prompt into its grid first (`freeze_idle_prompt`). Only at an idle
    // prompt (a Session on top): after detaching from a job, the pane's grid
    // is being written by the running job, and a frozen prompt would land
    // in its output.
    if matches!(app.windows[app.current_window].stack().last(), Some(Frame::Session(_))) {
        freeze_idle_prompt(app.sessions.get_mut(&parent_id).unwrap());
    }
    app.windows[app.current_window].focused_pane = new_pane_id;
    sid
}

// `:diag`'s sibling to `split_focused_pane`: a session forked only so the
// pane has a screen to composite (see `Frame`), with
// `Frame::Diagnostics` on top, starting minimized (`SplitChild`), focus
// staying in the file. Always a horizontal split, the pane at the
// bottom.
fn split_diagnostics_pane(app: &mut App, edit_frame_id: EditFrameId) -> PaneId {
    let parent_id = app.windows[app.current_window].owning_session();
    let child_history = Rc::new(RefCell::new(app.sessions[&parent_id].history.borrow().fork()));
    let mut child_shell = app.sessions[&parent_id].shell.new_virtual_child();
    // The same table every session sees.
    let child_lsp = Rc::clone(&app.sessions[&parent_id].lsp);
    install_service_table(&mut child_shell, &child_lsp);
    install_history(&mut child_shell, &child_history);
    let screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(app.term_rows), app.term_cols)));
    child_shell.set_sink_grid(screen.clone());
    let child_cwd = child_shell.cwd.clone();
    let sid = app.next_session_id;
    app.next_session_id += 1;
    app.sessions.insert(
        sid,
        SessionState {
            shell: child_shell,
            lsp: child_lsp,
            lsp_roots: HashMap::new(),
            lsp_semantic: HashMap::new(),
            lsp_semantic_applied: HashMap::new(),
            lsp_highlight: None,
            lsp_highlight_applied: None,
            lsp_inlay: None,
            lsp_inlay_applied: None,
            buffer: String::new(),
            buffer_unrecorded: false,
            history: Rc::clone(&child_history),
            screen,
            warned_stopped_jobs: false,
            dir_history: vec![child_cwd],
            dir_history_index: 0,
            command_transcript: Vec::new(),
        },
    );

    let window = &mut app.windows[app.current_window];
    let new_pane_id = window.add_pane(vec![Frame::Session(sid), Frame::Diagnostics(edit_frame_id)], true, true);

    new_pane_id
}

// The location list's counterpart: the same shape, horizontal,
// minimized, focus left in the file.
fn split_locations_pane(app: &mut App, edit_frame_id: EditFrameId) -> PaneId {
    let parent_id = app.windows[app.current_window].owning_session();
    let child_history = Rc::new(RefCell::new(app.sessions[&parent_id].history.borrow().fork()));
    let mut child_shell = app.sessions[&parent_id].shell.new_virtual_child();
    let child_lsp = Rc::clone(&app.sessions[&parent_id].lsp);
    install_service_table(&mut child_shell, &child_lsp);
    install_history(&mut child_shell, &child_history);
    let screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(app.term_rows), app.term_cols)));
    child_shell.set_sink_grid(screen.clone());
    let child_cwd = child_shell.cwd.clone();
    let sid = app.next_session_id;
    app.next_session_id += 1;
    app.sessions.insert(
        sid,
        SessionState {
            shell: child_shell,
            lsp: child_lsp,
            lsp_roots: HashMap::new(),
            lsp_semantic: HashMap::new(),
            lsp_semantic_applied: HashMap::new(),
            lsp_highlight: None,
            lsp_highlight_applied: None,
            lsp_inlay: None,
            lsp_inlay_applied: None,
            buffer: String::new(),
            buffer_unrecorded: false,
            history: Rc::clone(&child_history),
            screen,
            warned_stopped_jobs: false,
            dir_history: vec![child_cwd],
            dir_history_index: 0,
            command_transcript: Vec::new(),
        },
    );
    let window = &mut app.windows[app.current_window];
    let new_pane_id = window.add_pane(vec![Frame::Session(sid), Frame::Locations(edit_frame_id)], true, true);
    new_pane_id
}

fn locations_sibling(window: &WindowEntry, edit_frame_id: EditFrameId) -> Option<PaneId> {
    window.panes.iter().find(|p| p.stack.last() == Some(&Frame::Locations(edit_frame_id))).map(|p| p.id)
}

// The collapsed title row, shaped as `render_diagnostics_title`'s (a
// reverse-video pill in a dashed divider), so the two panes look alike.
fn render_locations_title(screen: &Rc<RefCell<vt100::Screen>>, cols: usize, title: &str) {
    let pill: String = format!(" {title} ").chars().take(cols).collect();
    let pill_len = pill.chars().count();
    let left = 2.min(cols.saturating_sub(pill_len));
    let right = cols.saturating_sub(pill_len + left);
    let mut framed = String::from("\r\x1b[K");
    framed.push_str(&"\u{2500}".repeat(left));
    framed.push_str("\x1b[7m");
    framed.push_str(&pill);
    framed.push_str("\x1b[0m");
    framed.push_str(&"\u{2500}".repeat(right));
    screen.borrow_mut().feed(framed.as_bytes());
}

// Creates the location pane if needed, records the list and repaints,
// from `gr`, while its buffer is still the caller's.
fn sync_locations_pane(app: &mut App, edit_frame_id: EditFrameId, list: LocationList) {
    let pane_id = locations_sibling(&app.windows[app.current_window], edit_frame_id).unwrap_or_else(|| split_locations_pane(app, edit_frame_id));
    let rect = pane_rect(&app.windows[app.current_window], pane_id, app.term_rows, app.term_cols);
    let sid = app.windows[app.current_window].pane(pane_id).owning_session();
    render_locations_title(&app.sessions[&sid].screen, rect.cols, &list.title);
    app.location_lists.insert(edit_frame_id, list);
    compositor_redraw(app);
}

// The `:diag` pane below `edit_frame_id`'s editor pane, if there is one.
fn diagnostics_sibling(window: &WindowEntry, edit_frame_id: EditFrameId) -> Option<PaneId> {
    window.panes.iter().find(|p| p.stack.last() == Some(&Frame::Diagnostics(edit_frame_id))).map(|p| p.id)
}

// `:dbg`'s counterpart: horizontal, minimized, focus left in the file.
// Its session runs nothing; it exists so `DebugRun` has a
// `Frame::Session` beneath and a screen. The debugged script's `Shell`
// is in the `debugger::DebugSession` in `debug_frames`, rendering into
// this same screen (`Shell::set_sink_grid`), so its output shows as any
// session's does.
fn split_debug_run_pane(app: &mut App, edit_frame_id: EditFrameId) -> (PaneId, Rc<RefCell<vt100::Screen>>) {
    let parent_id = app.windows[app.current_window].owning_session();
    let child_history = Rc::new(RefCell::new(app.sessions[&parent_id].history.borrow().fork()));
    let mut child_shell = app.sessions[&parent_id].shell.new_virtual_child();
    // The same table every session sees.
    let child_lsp = Rc::clone(&app.sessions[&parent_id].lsp);
    install_service_table(&mut child_shell, &child_lsp);
    install_history(&mut child_shell, &child_history);
    let screen = Rc::new(RefCell::new(vt100::Screen::new(content_rows(app.term_rows), app.term_cols)));
    child_shell.set_sink_grid(screen.clone());
    let child_cwd = child_shell.cwd.clone();
    let sid = app.next_session_id;
    app.next_session_id += 1;
    app.sessions.insert(
        sid,
        SessionState {
            shell: child_shell,
            lsp: child_lsp,
            lsp_roots: HashMap::new(),
            lsp_semantic: HashMap::new(),
            lsp_semantic_applied: HashMap::new(),
            lsp_highlight: None,
            lsp_highlight_applied: None,
            lsp_inlay: None,
            lsp_inlay_applied: None,
            buffer: String::new(),
            buffer_unrecorded: false,
            history: Rc::clone(&child_history),
            screen: screen.clone(),
            warned_stopped_jobs: false,
            dir_history: vec![child_cwd],
            dir_history_index: 0,
            command_transcript: Vec::new(),
        },
    );

    let window = &mut app.windows[app.current_window];
    let new_pane_id = window.add_pane(vec![Frame::Session(sid), Frame::DebugRun(edit_frame_id)], true, true);

    (new_pane_id, screen)
}

// A debug session over the Debug Adapter Protocol, for a program in a
// language bish doesn't know. `debugger::DebugSession` runs a bish script
// in this process, so breakpoints work in subshells and a pause blocks
// in place. An adapter is a subprocess sending events when it likes, so
// this one is polled from the editor's idle tick (`sync_debug_adapter`)
// and the editor stays live; a stop arrives rather than blocks.
struct DapFrame {
    session: crate::dapclient::Session,
    /// What's debugged, for the status line: the adapter takes a program,
    /// not the source file shown.
    program: String,
    /// The thread of the last stop, which every step needs and only a
    /// `stopped` event gives.
    thread: Option<i64>,
    /// The stack as of the last stop, innermost first. Empty while
    /// running.
    frames: Vec<crate::dap::StackFrame>,
    /// The pending `stackTrace` request, for the idle tick to look for.
    awaiting_stack: Option<i64>,
    /// A pending `print`/`locals` answer and its label.
    awaiting_answer: Option<(i64, String)>,
    /// A stop seen but the cursor not yet moved: that waits for the stack,
    /// which gives the line.
    landing: bool,
}

impl DapFrame {
    /// The frame being looked at, the innermost; what `scopes` and `evaluate`
    /// mean by `frameId`.
    fn current_frame(&self) -> Option<i64> {
        self.frames.first().map(|f| f.id)
    }

    fn describe_stop(&self) -> String {
        match self.frames.first() {
            Some(frame) => {
                let reason = match self.session.execution() {
                    crate::dapclient::Execution::Stopped(stop) => stop.reason.clone(),
                    crate::dapclient::Execution::Running => "running".to_string(),
                };
                let where_ = match &frame.source_path {
                    Some(path) => format!("{}:{}", path.rsplit('/').next().unwrap_or(path), frame.line),
                    None => format!("{}:{}", frame.name, frame.line),
                };
                format!("bish: dbg: {reason} in {} ({where_})", frame.name)
            }
            None => "bish: dbg: stopped".to_string(),
        }
    }
}

/// `[--adapter=COMMAND] PROGRAM`, `:dbg launch`'s arguments. `None` with no
/// program, the only requirement (adapters have defaults). The program
/// is the last word and an `--adapter=` runs up to it, since an adapter
/// is a command line (`gdb -i=dap`, `lldb-dap --port 0`). No quoting: a
/// path with a space can't be given, and a half-working quote would be
/// worse.
fn parse_launch_arguments(rest: &str) -> Option<(Option<Vec<String>>, String)> {
    let words: Vec<&str> = rest.split_whitespace().collect();
    let (program, before) = words.split_last()?;
    if program.starts_with("--adapter=") {
        return None; // an adapter and no program to run under it
    }
    let adapter = before
        .first()
        .and_then(|w| w.strip_prefix("--adapter="))
        .map(|first| std::iter::once(first).chain(before[1..].iter().copied()).map(str::to_string).collect());
    Some((adapter, program.to_string()))
}

/// The adapter for a language when none was named. Deliberately small:
/// `gdb` has spoken this protocol since version 14, is on most machines
/// with a compiler, and debugs every language listed; anything else is
/// `--adapter=`. Go isn't listed: gdb loads a Go binary but doesn't
/// understand goroutines, so its view is misleading. Delve speaks the
/// protocol, but its invocation isn't guessed; `--adapter=` names it.
fn default_debug_adapter(language: &str) -> Option<Vec<String>> {
    match language {
        "c" | "cpp" | "c++" | "rust" | "zig" => Some(vec!["gdb".to_string(), "-i=dap".to_string()]),
        _ => None,
    }
}

// The `:dbg` pane below `edit_frame_id`'s editor pane, if a session is
// attached.
fn debug_run_sibling(window: &WindowEntry, edit_frame_id: EditFrameId) -> Option<PaneId> {
    window.panes.iter().find(|p| p.stack.last() == Some(&Frame::DebugRun(edit_frame_id))).map(|p| p.id)
}

// The debug-run pane's collapsed title, the dashes and pill of
// `render_diagnostics_title`, shown while idle (running or paused draws
// directly; see `PauseState::render`).
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

// The diagnostics pane's one-row title, fed into its grid as
// `freeze_editor_frame` does: a reverse-video pill holding the title in
// an otherwise ordinary dashed divider, so the minimized pane's row is
// the divider below the editor (see `SplitChild`).
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

// Creates the diagnostics pane if needed (`split_diagnostics_pane`),
// rewrites its title and repaints, from `:diag` right after it computes
// `tb.diagnostics`, while that buffer is the caller's.
fn sync_diagnostics_pane(app: &mut App, edit_frame_id: EditFrameId, diagnostics: &[lint::Diagnostic]) {
    let pane_id = diagnostics_sibling(&app.windows[app.current_window], edit_frame_id).unwrap_or_else(|| split_diagnostics_pane(app, edit_frame_id));
    let rect = pane_rect(&app.windows[app.current_window], pane_id, app.term_rows, app.term_cols);
    let sid = app.windows[app.current_window].pane(pane_id).owning_session();
    render_diagnostics_title(&app.sessions[&sid].screen, rect.cols, diagnostics);
    compositor_redraw(app);
}

// Repaints the diagnostics pane's title for the focused editor pane, if
// one is open, never creating it: called on the idle path, where a
// server publication isn't a reason to split. It keeps the problem count
// current as the server reports.
fn refresh_diagnostics_title(app: &mut App, diagnostics: &[lint::Diagnostic]) {
    let window = &app.windows[app.current_window];
    let Some(Frame::Edit(edit_frame_id)) = window.panes.iter().find(|p| p.id == window.focused_pane).and_then(|p| p.stack.last()).copied() else {
        return;
    };
    let Some(pane_id) = diagnostics_sibling(window, edit_frame_id) else { return };
    let rect = pane_rect(window, pane_id, app.term_rows, app.term_cols);
    let sid = window.pane(pane_id).owning_session();
    render_diagnostics_title(&app.sessions[&sid].screen, rect.cols, diagnostics);
    // The grid alone changes nothing on screen, and the earlier redraw
    // preceded this; callers repaint the editor pane right after, which is
    // what their `diagnosed` flag is for.
    compositor_redraw(app);
}

// Captures a pane's idle prompt into its grid when the pane is about to
// lose focus: `read_line` draws the live prompt only on the terminal,
// and once another pane is visible, redraws come from grids, so this
// pane would show blank. Like the Line handler's feed for a submitted
// line, without its trailing "\r\n", leaving the cursor after the
// prompt. The leading `\r\x1b[K` overwrites a previous freeze on the
// row, or the prompt would appear twice.
fn freeze_idle_prompt(session: &mut SessionState) {
    // The pane's grid width, not the terminal's.
    let cols = session.screen.borrow().size().1;
    let prompt_str = if session.buffer.is_empty() { prompt::render(&session.shell, cols) } else { prompt::continuation() };
    let framed = format!("\r\x1b[K{}", prompt_str);
    session.screen.borrow_mut().feed(framed.as_bytes());
}

// `freeze_idle_prompt` plus `text`, the unsubmitted buffer, so Normal
// mode's pane view shows what was typed. Returns the prompt it used, so
// the caller can find the column `text` starts at for the
// `ScreenBuffer` cursor.
fn freeze_input_with_text(session: &mut SessionState, text: &str) -> String {
    let cols = session.screen.borrow().size().1;
    let prompt_str = if session.buffer.is_empty() { prompt::render(&session.shell, cols) } else { prompt::continuation() };
    let framed = format!("\r\x1b[K{}{}", prompt_str, text);
    session.screen.borrow_mut().feed(framed.as_bytes());
    prompt_str
}

// Records the line Ctrl-C abandoned into the grid as the terminal shows
// it (prompt, text, `^C`), leaving the cursor on the next row. See
// `ReadOutcome::Interrupted`, and the Line handler's echo for a command
// that ran.
fn freeze_interrupted_line(session: &mut SessionState, text: &str) {
    freeze_input_with_text(session, text);
    session.screen.borrow_mut().feed(b"^C\r\n");
}

// A session is unreferenced once the last window holding it at any
// depth closes (`window fg` can bury it in another window). Called only
// when Close removes a window; no other action orphans one.
fn close_orphaned_sessions(sessions: &mut HashMap<SessionId, SessionState>, windows: &[WindowEntry]) {
    let referenced: std::collections::HashSet<SessionId> = windows
        .iter()
        .flat_map(|w| w.panes.iter())
        .flat_map(|p| {
            p.stack.iter().filter_map(|f| match f {
                Frame::Session(id) => Some(*id),
                Frame::Job(_) | Frame::Edit(_) | Frame::Hex(_) | Frame::Diagnostics(_) | Frame::Locations(_) | Frame::DebugRun(_) => None,
            })
        })
        .collect();
    sessions.retain(|id, _| referenced.contains(id));
}

// Picks up a changed `divider_budget` before the layout is resolved:
// every visible change passes through here.
fn refresh_divider_budget(sessions: &HashMap<SessionId, SessionState>, window: &mut WindowEntry) {
    if let Some(session) = sessions.get(&window.owning_session()) {
        window.divider_budget = session.shell.bishopt_int("divider_budget").max(0) as usize;
    }
}

// Full redraw of the focused window's panes and the tab bar, which
// together cover every cell, so nothing is erased first
// (`build_compositor_frame_output`). A full redraw rather than a diff:
// these happen on discrete events, and it heals anything written to the
// terminal directly in between.
fn compositor_redraw(app: &App) {
    let tab_bar = tab_bar_line(app);
    sync_terminal_title(app);
    let layout = snapshot_window(&app.windows[app.current_window], &app.sessions, app.term_rows, app.term_cols);
    render_compositor_frame(&layout, &tab_bar, app.term_rows);
}

// How `drive_fg_job`'s loop ended.
enum FgOutcome {
    // The job exited or was killed (a forwarded Ctrl-C).
    Exited(i32),
    // The detach key (Ctrl+G) instead of an exit. The job is untouched,
    // still running and on top; the caller decides what's next (Normal
    // mode, from which `window next`/`previous` can leave).
    Detached,
    // The job stopped (a forwarded Ctrl-Z through its pty, or
    // `kill -STOP`). Unlike Detached, it's now a Stopped job for
    // `run_fg_job_frame` to file (`Shell::park_stopped_fg_job`).
    Stopped,
    // A left click (`MouseEvent::is_left_click`) intercepted from stdin
    // instead of forwarded (see `decode_fg_click`). `run_fg_job_frame`
    // hit-tests it: a miss or the job's own pane forwards it; the tab bar or
    // another pane takes focus there, the job running on, like a detach.
    MouseClick(editor::MouseEvent),
    // The job's screen changed size mid-drive. `poll_and_apply_resize` has
    // resized everything else and `drive_fg_job` this job's pty; the caller
    // must replace its stale pane-rect and tab-bar snapshot before
    // resuming.
    Resized,
}

const BRACKETED_PASTE_ENABLE: &str = "\x1b[?2004h";
const BRACKETED_PASTE_DISABLE: &str = "\x1b[?2004l";

// Turns the terminal's mouse reporting (SGR mode 1006, button-event mode
// 1002) on or off to match what the job asked of its virtual screen
// (`vt100::Screen::mouse_reporting`, which merges DECSET 1000, 1002,
// 1003 and 1006 into one flag). Only from `drive_fg_job`, where the job
// owns stdin and its requests are read; it's turned off again before
// returning, whatever the job left.
fn sync_mouse_reporting(enabled: &mut bool, screen: &Rc<RefCell<vt100::Screen>>) {
    let wants = screen.borrow().mouse_reporting;
    if wants == *enabled {
        return;
    }
    print!("{}", if wants { term::MOUSE_REPORTING_ENABLE } else { term::MOUSE_REPORTING_DISABLE });
    let _ = io::stdout().flush();
    *enabled = wants;
}

// The same for DECSET 2004, bracketed paste
// (`vt100::Screen::bracketed_paste`). Without telling the real terminal,
// a paste reaches a job like vim as fast typing, and its autoindent
// mangles every line.
fn sync_bracketed_paste(enabled: &mut bool, screen: &Rc<RefCell<vt100::Screen>>) {
    let wants = screen.borrow().bracketed_paste;
    if wants == *enabled {
        return;
    }
    print!("{}", if wants { BRACKETED_PASTE_ENABLE } else { BRACKETED_PASTE_DISABLE });
    let _ = io::stdout().flush();
    *enabled = wants;
}

// The same for the job's pty size, kept equal to `screen`'s, which
// repl.rs sizes to the pane. `screen` changes only from outside
// (`poll_and_apply_resize` through `on_idle`); this catches the pty up.
// Returns whether it changed, so the caller rebuilds stale geometry
// (`FgOutcome::Resized`).
fn sync_pty_size(last: &mut (u16, u16), job: &mut exec::FgJob, screen: &Rc<RefCell<vt100::Screen>>) -> bool {
    use std::os::fd::AsRawFd;
    let (rows, cols) = screen.borrow().size();
    let (rows, cols) = (rows as u16, cols as u16);
    if (rows, cols) == *last {
        return false;
    }
    let _ = pty::set_size(job.pty_master().as_raw_fd(), rows, cols);
    *last = (rows, cols);
    true
}

// The first SGR mouse report in `seq` (`\x1b[<Cb;Cx;CyM/m`), if it's a
// left click; `None` otherwise (incomplete, malformed, other buttons),
// meaning forward it unchanged. Scans for the first `M`/`m`: a press and
// its release, or several clicks, can arrive in one read. Separate so
// it's tested without a pty, like editor.rs's `decode_sgr_mouse_final`.
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

// Drives a job pushed as a `Frame::Job`: reads its pty master into
// `screen`, calls `redraw` after each batch, and forwards stdin bytes
// to the master unmodified, so the job's pty turns Ctrl-C into SIGINT
// for its group as a terminal would. `on_idle` runs on ticks with
// neither output nor input (`service_background_jobs`), as `read_line`'s
// does at a prompt.
//
// Ctrl+G (0x07), or the NUL Ctrl+Space sends, means detach: the job keeps
// running, this shell just stops watching it, like switching away from a
// multiplexer pane. Mouse reports and bracketed pastes reach the job
// through the same forwarding (see `sync_mouse_reporting`,
// `sync_bracketed_paste`).
fn drive_fg_job(job: &mut exec::FgJob, screen: &Rc<RefCell<vt100::Screen>>, mut redraw: impl FnMut(), mut on_idle: impl FnMut()) -> FgOutcome {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    pty::set_nonblocking(job.pty_master().as_raw_fd());
    let _raw_guard = term::RawGuard::enable(0).ok();

    // Checked first (a resumed job may have asked already) and after each
    // batch of output, where a new DECSET request could arrive.
    let mut mouse_enabled = false;
    sync_mouse_reporting(&mut mouse_enabled, screen);
    let mut bracketed_paste_enabled = false;
    sync_bracketed_paste(&mut bracketed_paste_enabled, screen);
    let mut pty_size = {
        let (r, c) = screen.borrow().size();
        (r as u16, c as u16)
    };

    let mut buf = [0u8; 4096];
    // How many chunks are drained per tick before checking input, exit and
    // redrawing: a firehose job (`yes`) can keep the buffer full forever, so
    // reading until WouldBlock might never return.
    const MAX_READS_PER_TICK: u32 = 16;
    let outcome = loop {
        // Every iteration (a borrow and a comparison, no syscall unless
        // changed), not only when idle: a firehose job can keep this loop
        // draining, and its pty should still catch up. See `sync_pty_size`.
        if sync_pty_size(&mut pty_size, job, screen) {
            break FgOutcome::Resized;
        }

        // Drain before checking whether the job ended: a fast command
        // (`echo`, `pwd`) can write everything and exit within
        // microseconds, and checking `poll_untraced` first would stop
        // before reading output already in the pty.
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

        // A short poll paces the loop without spinning while staying
        // responsive.
        let ready = term::stdin_ready(10);
        if ready {
            let n = crate::platform::read_bytes(0, &mut buf).map(|n| n as isize).unwrap_or(-1);
            if n > 0 {
                // Ctrl+G, or Ctrl+Space's NUL: a raw byte, as nothing
                // decodes keys while a job holds the terminal.
                if n == 1 && matches!(buf[0], 0x07 | 0x00) {
                    break FgOutcome::Detached;
                }
                // Ctrl-Z (0x1a) is signalled rather than forwarded (see
                // `FgJob::send_stop` for why forwarding doesn't stop the
                // job). Nor is it sent as data: a terminal delivers only
                // the signal.
                if n == 1 && buf[0] == 0x1a {
                    job.send_stop();
                    continue;
                }
                // A lone ESC may be an Escape key or the start of a
                // sequence whose rest hasn't arrived (common through nested
                // terminals). Forwarded alone, a program like vim would
                // time it out as Escape (`ttimeoutlen`, about 50ms) and
                // read the rest as separate keys. So wait briefly and send
                // the whole sequence in one write, as a keyboard emits it.
                let mut seq = buf[..n as usize].to_vec();
                if n == 1 && buf[0] == 0x1b && term::stdin_ready(50) {
                    let more = crate::platform::read_bytes(0, &mut buf).map(|n| n as isize).unwrap_or(-1);
                    if more > 0 {
                        seq.extend_from_slice(&buf[..more as usize]);
                    }
                }
                // An SGR mouse report (`\x1b[<Cb;Cx;CyM/m`): wait for the
                // rest as for a lone ESC, then decode it. A left click
                // (`decode_fg_click`) may be for bish's tab bar or another
                // pane, so it goes back to the caller; anything else is
                // forwarded unchanged.
                if seq.starts_with(b"\x1b[<") {
                    // Whether the first report has ended, not just whether
                    // `seq` ends in `M`/`m`, which a quickly following
                    // release could make it.
                    while !seq[3..].iter().any(|b| *b == b'M' || *b == b'm') && term::stdin_ready(50) {
                        let more = crate::platform::read_bytes(0, &mut buf).map(|n| n as isize).unwrap_or(-1);
                        if more <= 0 {
                            break;
                        }
                        seq.extend_from_slice(&buf[..more as usize]);
                    }
                    if let Some(ev) = decode_fg_click(&seq) {
                        break FgOutcome::MouseClick(ev);
                    }
                }
                // The keyboard sends arrows as CSI (`ESC [ A`); if the job
                // switched to application cursor-key mode (DECCKM),
                // re-encode as SS3 (`ESC O A`). See
                // `Screen::app_cursor_keys` for why bish translates.
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
    // Always, however the loop ended: the terminal has no notion of this
    // job's mouse mode, and leaving it on would leak mouse sequences into
    // whatever reads stdin next. Harmless if never enabled.
    if mouse_enabled {
        print!("{}", term::MOUSE_REPORTING_DISABLE);
        let _ = io::stdout().flush();
    }
    // The same cleanup: `read_line` doesn't understand paste brackets, so a
    // paste after the job exits would arrive with them as garbage.
    if bracketed_paste_enabled {
        print!("{BRACKETED_PASTE_DISABLE}");
        let _ = io::stdout().flush();
    }
    redraw();
    outcome
}

// One buffer as anything outside this process reads it: positions as
// 0-based (line, character), which only this side can compute, since a
// diagnostic is a flat offset and the buffer knows where lines start.
fn file_state(tb: &TextBuffer, focused: bool) -> Option<mcp::FileState> {
    let path = tb.path()?.to_path_buf();
    let selection = tb.selections.first().map(|range| {
        let (from, to) = if range.from <= range.to { (range.from, range.to) } else { (range.to, range.from) };
        (from.0, from.1, to.0, to.1)
    });
    let selected_text = match tb.selections.first() {
        Some(range) => motion::extract_text(tb, range),
        None => String::new(),
    };
    let diagnostics = tb
        .diagnostics
        .iter()
        .map(|d| mcp::Diag {
            message: d.message.clone(),
            severity: d.severity,
            start: fileeditor::diagnostic_position(tb, d.start),
            end: fileeditor::diagnostic_position(tb, d.end),
            source: d.source.clone(),
            code: d.code.to_string(),
        })
        .collect();
    Some(mcp::FileState { path, line_count: tb.line_count(), focused, dirty: tb.is_dirty(), selection, selected_text, diagnostics })
}

/// Everything bish will say about itself: the focused buffer as the
/// navigation loop published it, and every other in `edit_frames`.
fn editor_state(app: &App) -> mcp::EditorState {
    let mut files: Vec<mcp::FileState> = app.focus.iter().cloned().collect();
    for session in app.edit_frames.values() {
        // The focused buffer isn't here (see `App::focus`), but a second
        // pane on the same file can be, and readers expect one entry per
        // path.
        if files.iter().any(|f| Some(f.path.as_path()) == session.buffer.path()) {
            continue;
        }
        if let Some(state) = file_state(&session.buffer, false) {
            files.push(state);
        }
    }
    mcp::EditorState { files }
}

/// Publishes what the focused editor shows, returning whether it changed,
/// which is what fires a selection notification.
fn publish_focus(app: &mut App, buf: &NavBuffer) -> bool {
    match buf.as_editable() {
        Some(tb) => publish_focus_buffer(app, tb),
        // A pane not showing a file publishes that, so a reader can tell "no
        // file" from "the same file".
        None => {
            let changed = app.focus.is_some();
            app.focus = None;
            changed
        }
    }
}

fn publish_focus_buffer(app: &mut App, tb: &TextBuffer) -> bool {
    // Not gated on a listener: a peer becomes known when it first speaks, a
    // few milliseconds before its first question, in the same tick before
    // publishing, so a gate would answer that question with nothing open.
    // Always current costs a comparison per tick.
    let next = file_state(tb, true);
    if next == app.focus {
        return false;
    }
    app.focus = next;
    true
}

/// Tells every peer what's selected now, if the snapshot changed.
/// Debounced by construction: only on an idle tick, and only on a
/// change, so moving within a line says nothing.
fn notify_selection(app: &App) {
    let Some(payload) = mcp::selection_notification(app.focus.as_ref()) else { return };
    session::push_rpc_notification(&payload);
}

// Keeps fg'd jobs alive while another loop owns the terminal: the
// `on_idle` of `drive_fg_job`, `read_line` and Normal mode. A
// `Frame::Job` can be in any pane, so every pane of every window is
// drained, bounded as `drive_fg_job`'s drain is so a firehose can't hold
// it. Returns whether a session client just attached, for a caller
// driving a view the grid doesn't hold (a `Frame::Edit`) to redraw it.
fn service_background_jobs(app: &mut App) -> bool {
    use std::io::Read;

    // Every blocking loop calls this as its idle hook, so a setting change
    // passes through here however it was made; `divider_budget`, kept on
    // the window, catches up here.
    for window in app.windows.iter_mut() {
        refresh_divider_budget(&app.sessions, window);
    }
    // `<( )` producers are coroutines here and progress only when given a
    // turn; every blocking loop reaches this, so it's where they get one,
    // or a producer would fill its pipe, park and never resume
    // (`wc -l < <(seq 1 20000)` would stop at one pipe buffer).
    for session in app.sessions.values() {
        session.shell.pump_coroutines();
    }
    // The window list every session's `window` builtin reads, so `window ls`
    // and `select` see the windows that exist now.
    let snapshot = window_snapshot(app);
    for session in app.sessions.values_mut() {
        session.shell.windows = snapshot.clone();
    }

    // One non-blocking tick per language server, here for the same reason:
    // the one callback every loop runs. The table is shared, so any
    // session's handle reaches all of it.
    if let Some(lsp) = app.sessions.values().next().map(|s| Rc::clone(&s.lsp)) {
        lsp.borrow_mut().service_all();
    }

    // A no-op unless this process is a `bish session` daemon
    // (`session::install_bridge`); see session.rs for why this call is all
    // the wiring needed.
    session::service_current_bridge();
    // `bish session capture` asks what the focused pane shows; the grid is
    // here, so it's answered here, rendered only when asked.
    session::answer_capture_requests(|| focused_pane_text(app));
    // Answers peers' questions here, where `App` is; the bridge holds no
    // session state. The state is built only when a question waits. A
    // review's answer is someone's decision, so it's set aside with its
    // asker for the prompt or editor to open (`pending_reviews`).
    let asked = &*app;
    let mut reviews = Vec::new();
    session::answer_rpc_requests_with(|question, caller| {
        if let Some(review) = mcp::ReviewRequest::from_question(question) {
            reviews.push((review, caller));
            return None;
        }
        mcp::answer(&editor_state(asked), question)
    });
    app.pending_reviews.extend(reviews);
    // A client just attached: one full repaint, so its blank terminal
    // catches up (`take_bridge_just_attached`). Not enough for a driven
    // `Frame::Edit`, whose content isn't in the grid, hence the return
    // value.
    let just_attached = session::take_bridge_just_attached();
    // Background jobs' output since the last tick, into the grid of the
    // session that started each (`Shell::drain_background_output`); the job
    // table is shared, so one call covers every session.
    let bg_output = app.sessions.values().next().is_some_and(|s| s.shell.drain_background_output());
    // Only a client attaching repaints here. Background output doesn't:
    // this can't know what's drawn over the pane (a half-typed prompt, a
    // browser, the diagnostics list), and a blind redraw would wipe it; it's
    // reported through the return value for each caller to repaint.
    if just_attached && app.sinks_are_grid {
        compositor_redraw(app);
    }
    // The new client's TERM and COLORTERM go to every session's remembered
    // environment (`Shell::set_terminal_capability_env`): one client serves
    // the whole daemon, and unfocused windows need it later. `set_var` here
    // wouldn't stick (see that method).
    if let Some((term, colorterm)) = session::take_pending_capability() {
        for session in app.sessions.values_mut() {
            session.shell.set_terminal_capability_env(&term, &colorterm);
        }
    }

    // A resize repaints from the grids, blanking a driven `Frame::Edit`, so
    // its result is returned for the caller to follow up.
    let resized = poll_and_apply_resize(app);

    const MAX_READS_PER_TICK: u32 = 16;
    let mut buf = [0u8; 4096];
    for i in 0..app.windows.len() {
        let pane_ids: Vec<PaneId> = app.windows[i].panes.iter().map(|p| p.id).collect();
        for pane_id in pane_ids {
            // No focused-pane exemption: `run_fg_job_frame` *removes* the
            // job it's driving from `app.job_frames` for the whole time it
            // holds it (re-inserting on the way out), so the lookup below
            // already misses for a job someone else is reading -- the
            // skip that used to be here was belt-and-braces, and it cost
            // the one case it actually covered. Ctrl+G out of a
            // running job and normal mode owns that pane while nothing
            // reads its pty: the job stalled the moment its buffer
            // filled, and everything it had said appeared in one lump
            // only once normal mode was left. It keeps running now, into
            // its own session's grid, with the reader's viewport left
            // where they put it and the mode line reporting what arrived
            // (see ScreenBuffer::new_lines).
            let job_frame_id = match app.windows[i].pane(pane_id).stack.last() {
                Some(Frame::Job(id)) => *id,
                _ => continue,
            };
            let sid = app.windows[i].pane(pane_id).owning_session();
            let screen = app.sessions[&sid].screen.clone();
            let job = match app.job_frames.get_mut(&job_frame_id) {
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
                    app.windows[i].pane_mut(pane_id).stack.pop();
                    app.job_frames.remove(&job_frame_id);
                    app.sessions.get_mut(&sid).unwrap().shell.last_status = status;
                }
                exec::FgWait::Stopped => {
                    app.windows[i].pane_mut(pane_id).stack.pop();
                    let job = app.job_frames.remove(&job_frame_id).unwrap();
                    let (id, cmd_text) = app.sessions.get_mut(&sid).unwrap().shell.park_stopped_fg_job(job);
                    app.sessions[&sid].shell.sink_err(&format!("\n[{}]+  Stopped                 {}\n", id, cmd_text));
                    app.sessions.get_mut(&sid).unwrap().shell.last_status = 148;
                }
                exec::FgWait::Running => {}
            }
        }
    }
    // Any of these is the caller's cue to follow up with its own view
    // redraw if it has one the grid can't express (see the just_attached
    // comment above). A resize additionally means the caller's own cached
    // pane rect is stale, so it has to re-measure before redrawing.
    just_attached || bg_output || resized
}

// Raw stdin read for drive_fg_job. Renamed via link_name so the local
// name doesn't collide with anything already in scope (drive_fg_job also
// imports the `Read` trait, a different namespace, but this keeps the
// pattern consistent with pty.rs's own `c_open` reasoning). Deliberately
// not going through std::io::Stdin here (its internal buffering could
// swallow bytes term::stdin_ready's poll() wouldn't know about) -- same
// reasoning as editor.rs's own read_byte.
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
    // This window's session's `ui_col_divider` (see theme.rs), resolved
    // once per frame -- `draw_divider` writes straight into the frame
    // string and has no Shell to ask.
    divider_sgr: String,
    panes: Vec<PaneSnapshot>,
    dividers: Vec<Divider>,
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

// `window size <N>`/`<N>%`/`<N>/<M>`: sizes the focused pane along the
// axis its parent split divides. A no-op if the window isn't split.
//
// Panes are sized by weight (see `SplitChild`), so this solves for the
// weight giving the requested share `f` with the siblings' weights
// (`sum_others`) fixed: `f * sum_others / (1 - f)`. A cell count is
// turned into a share using the pane's current rendered size and weight,
// which ignores the rounding remainder `compute_regions` hands the last
// child; close enough, and the next redraw settles it.
fn set_focused_pane_size(window: &mut WindowEntry, spec: exec::SizeSpec, term_rows: usize, term_cols: usize) {
    let focused = window.focused_pane;
    let current_rect = pane_rect(window, focused, term_rows, term_cols);
    let Some((horizontal, children, idx)) = find_parent_split_mut(&mut window.layout, focused) else {
        return;
    };
    let axis_size = if horizontal { current_rect.rows } else { current_rect.cols };
    let old_weight = children[idx].weight.max(MIN_PANE_WEIGHT);
    let total_weight: f64 = children.iter().map(|c| c.weight.max(MIN_PANE_WEIGHT)).sum();
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
    set_child_weight_to_fraction(children, idx, fraction);
}

// Resolves a window's layout against the terminal size, resizing each
// session's grid to its pane's rectangle on the way, so a grid always
// matches the pane that last showed it.
fn snapshot_window(window: &WindowEntry, sessions: &HashMap<SessionId, SessionState>, term_rows: usize, term_cols: usize) -> CompositorLayout {
    let area = Rect { row: 0, col: 0, rows: content_rows(term_rows), cols: term_cols };
    let mut regions = Vec::new();
    let mut dividers = Vec::new();
    window.regions(area, &mut regions, &mut dividers);

    let panes = regions
        .into_iter()
        .map(|(pane_id, rect)| {
            let sid = window.pane(pane_id).owning_session();
            let screen = sessions[&sid].screen.clone();
            let (srows, scols) = screen.borrow().size();
            // A folded pane has no room on screen (see `collapsed_runs`);
            // resizing it to nothing would throw its content away.
            if rect.rows > 0 && rect.cols > 0 && (srows, scols) != (rect.rows, rect.cols) {
                screen.borrow_mut().resize(rect.rows, rect.cols);
            }
            PaneSnapshot { rect, screen, focused: pane_id == window.focused_pane }
        })
        .collect();

    CompositorLayout {
        divider_sgr: sessions
            .get(&window.owning_session())
            .map(|s| crate::theme::sgr(crate::theme::Ui::Divider, Some(&ui_colors(&s.shell))))
            .unwrap_or_default(),
        panes,
        dividers,
    }
}

// Draws every pane into its rectangle, then the dividers. Used by
// `compositor_redraw`, and by `render_compositor_frame_diff` when it has
// no frame to diff against.
//
// No `\x1b[2J`: the panes and dividers tile the whole content area,
// `render_row` writes every column, and the tab bar row is cleared with
// `\x1b[K`, so every cell is overwritten anyway. An erase followed by
// the same content can show as a blank frame, a visible flash.
fn render_compositor_frame(layout: &CompositorLayout, tab_bar: &str, term_rows: usize) {
    print!("{}", build_compositor_frame_output(layout, tab_bar, term_rows));
    let _ = io::stdout().flush();
}

// The string-building half of `render_compositor_frame`, for tests.
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

    for divider in &layout.dividers {
        draw_divider(&mut out, divider.rect, divider.horizontal, divider.folded, &layout.divider_sgr);
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

// What's painted on the terminal's content area: every pane flattened
// into one grid, plus the tab bar and the cursor. Dividers aren't
// captured; a divider cell reads as the default cell on both sides of a
// comparison, so it never shows as changed.
struct TerminalFrame {
    rows: usize,
    cols: usize,
    cells: Vec<vt100::Cell>,
    tab_bar: String,
    cursor: (usize, usize),
    cursor_visible: bool,
}

impl TerminalFrame {
    // `pane.rect` can be larger than `pane.screen` by now: the layout is
    // re-snapshotted only when the focused pane's size changes, and a resize
    // can shrink a sibling's grid without moving the focused one. Reads are
    // clamped to the screen's live size, or `Screen::cell` panics.
    fn capture(layout: &CompositorLayout, tab_bar: &str, rows: usize, cols: usize) -> TerminalFrame {
        let mut cells = vec![vt100::Cell::default(); rows * cols];
        for pane in &layout.panes {
            let screen = pane.screen.borrow();
            let (screen_rows, screen_cols) = screen.size();
            for r in 0..pane.rect.rows.min(screen_rows) {
                let last = pane.rect.cols.min(screen_cols);
                for c in 0..last {
                    let (row, col) = (pane.rect.row + r, pane.rect.col + c);
                    if row < rows && col < cols {
                        // A wide character whose second half falls outside
                        // this pane is drawn as a space: the terminal would
                        // draw it two columns wide, into the neighbouring
                        // pane.
                        let clipped = c + 1 == last && c + 1 < screen_cols && screen.cell(r, c + 1).continuation;
                        cells[row * cols + col] = match clipped {
                            true => vt100::Cell { ch: ' ', continuation: false, ..screen.cell(r, c) },
                            false => screen.cell(r, c),
                        };
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

// The redraw for a foreground job's output, which can arrive many times
// a second from a full-screen program even when nothing visible changes.
// A full repaint each time flashes, so this repaints only the cells that
// differ from `cache`, the last painted frame.
//
// `None` in `cache` means start fresh (the first redraw, or
// `run_fg_job_frame` invalidated it on `FgOutcome::Resized`): a full
// repaint, which also fixes anything written to the terminal directly
// meanwhile, and the result seeds `cache`.
//
// Only this path diffs. Every other redraw is a discrete event where a
// full repaint costs nothing noticeable, and the cache lives within one
// `run_fg_job_frame` call, so nothing else has to invalidate it.
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

// The escape codes taking the terminal from `prev` to `new`, both the
// same shape (the caller checks). Separate from the printing so it can
// be tested.
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
            // Paint each run of differing cells with one cursor move. A run
            // can cross pane boundaries; the flattened grid makes that
            // transparent.
            let run_start = col;
            while col < term_cols && new.cells[row * term_cols + col] != prev.cells[row * term_cols + col] {
                col += 1;
            }
            out.push_str(&format!("\x1b[{};{}H", row + 1, run_start + 1));
            let mut last_style: Option<(vt100::Color, vt100::Color, vt100::CellAttrs)> = None;
            for c in run_start..col {
                let cell = new.cells[row * term_cols + c];
                // See render_row: the second half of a wide character
                // is a column the terminal already covered.
                if cell.continuation {
                    continue;
                }
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

    // The cursor needs re-asserting after any write, or when it moved or
    // changed visibility without a cell changing.
    if !out.is_empty() || new.cursor != prev.cursor || new.cursor_visible != prev.cursor_visible {
        out.push_str(&format!("\x1b[{};{}H", new.cursor.0 + 1, new.cursor.1 + 1));
        out.push_str(if new.cursor_visible { "\x1b[?25h" } else { "\x1b[?25l" });
    }

    out
}

// Draws a plain single-line divider along a strip from
// `compute_regions`. Where nested dividers meet, the one drawn second
// wins; there are no junction glyphs.
fn draw_divider(out: &mut String, rect: Rect, horizontal: bool, folded: Option<usize>, sgr: &str) {
    out.push_str(sgr);
    if horizontal {
        out.push_str(&format!("\x1b[{};{}H", rect.row + 1, rect.col + 1));
        match folded {
            // Drawn like `render_diagnostics_title`'s pill, so a folded run
            // reads as a divider with something to say.
            Some(n) => out.push_str(&folded_divider_line(rect.cols, n)),
            None => out.push_str(&"─".repeat(rect.cols)),
        }
    } else {
        for r in 0..rect.rows {
            out.push_str(&format!("\x1b[{};{}H", rect.row + r + 1, rect.col + 1));
            out.push('│');
        }
    }
    if !sgr.is_empty() {
        out.push_str("\x1b[0m");
    }
}

// `───  4 panes  ─────`: what a folded run draws instead of a line. A
// vertical divider is one column wide, so it stays a plain line.
fn folded_divider_line(cols: usize, panes: usize) -> String {
    let pill: String = format!(" \u{22EF} {panes} pane{} ", if panes == 1 { "" } else { "s" }).chars().take(cols).collect();
    let pill_len = pill.chars().count();
    let left = 2.min(cols.saturating_sub(pill_len));
    let right = cols.saturating_sub(pill_len + left);
    format!("{}\x1b[7m{pill}\x1b[0m{}", "─".repeat(left), "─".repeat(right))
}

// Shrinks the focused pane to its divider and moves focus off it.
// `fixed` is cleared too: `minimized` wins over it (see `SplitChild`).
//
// Focus moves because a one-row pane is nowhere to type. It goes to a
// sibling in the same split, chosen from the layout rather than the
// geometry: any neighbour will do, unlike `<C-w>j`'s spatial question.
//
// A pane with no sibling is left alone: a window showing only a divider
// isn't worth being able to reach.
fn minimize_focused_pane(window: &mut WindowEntry) {
    let target = window.focused_pane;
    let Some((_, children, idx)) = find_parent_split_mut(&mut window.layout, target) else {
        return;
    };
    let sibling = if idx + 1 < children.len() { idx + 1 } else { idx.checked_sub(1).unwrap_or(idx) };
    if sibling == idx {
        return;
    }
    let Some(landing) = first_leaf_of(&children[sibling].layout) else { return };
    children[idx].minimized = true;
    children[idx].fixed = None;
    window.focused_pane = landing;
}

// The first pane inside a subtree -- where focus lands when it has to
// leave a pane and the sibling may itself be a split.
fn first_leaf_of(layout: &PaneLayout) -> Option<PaneId> {
    match layout {
        PaneLayout::Leaf(id) => Some(*id),
        PaneLayout::Split { children, .. } => children.iter().find_map(|c| first_leaf_of(&c.layout)),
    }
}

// Brings `pane` back if it was minimized. Called wherever focus lands on
// a pane deliberately (a `<C-w>` move, a click), so going to a pane is
// how you get it back.
fn restore_if_minimized(window: &mut WindowEntry, pane: PaneId) {
    if let Some((_, children, idx)) = find_parent_split_mut(&mut window.layout, pane)
        && children[idx].minimized
    {
        children[idx].minimized = false;
    }
}

// `<C-w>hjkl`: focuses the nearest pane in `direction`. Panes whose
// span across the other axis overlaps the focused pane's win over ones
// only diagonally that way; among those, the nearest along the axis.
// Plain centre distance would pick a diagonal pane over an adjacent one
// whenever sizes differ a lot. A no-op when nothing lies that way.
fn focus_pane_direction(
    window: &mut WindowEntry,
    sessions: &mut HashMap<SessionId, SessionState>,
    direction: PaneDirection,
    term_rows: usize,
    term_cols: usize,
) {
    let area = Rect { row: 0, col: 0, rows: content_rows(term_rows), cols: term_cols };
    let mut regions = Vec::new();
    let mut dividers = Vec::new();
    window.regions(area, &mut regions, &mut dividers);
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
        // The pane losing focus freezes its prompt into its grid (see
        // `freeze_idle_prompt`), as split does, unless a detached job is
        // still running there (see `split_focused_pane`).
        if matches!(window.stack().last(), Some(Frame::Session(_))) {
            let old_sid = window.owning_session();
            freeze_idle_prompt(sessions.get_mut(&old_sid).unwrap());
        }
        // Going to a pane is how you get a minimized one back.
        restore_if_minimized(window, id);
        window.focused_pane = id;
    }
}

fn ranges_overlap(a_start: usize, a_len: usize, b_start: usize, b_len: usize) -> bool {
    a_start < b_start + b_len && b_start < a_start + a_len
}

fn rect_center(rect: &Rect) -> (usize, usize) {
    (rect.col + rect.cols / 2, rect.row + rect.rows / 2)
}

// Whether the prompt should ask the terminal for mouse reports, which
// isn't the `mouse` bishopt alone.
//
// Reporting takes away the terminal's own wheel scrolling and drag
// selection, so it's only worth it where bish has something in their
// place. Before promotion there are no tabs, panes or dividers to click,
// and `read_line` drops wheel events, so the terminal would just stop
// scrolling. Once promoted, bish owns the scrollback and the layout.
fn prompt_claims_mouse(mouse_bishopt: bool, sinks_are_grid: bool) -> bool {
    mouse_bishopt && sinks_are_grid
}

thread_local! {
    // The last frame painted by a view that draws the terminal directly
    // (the editor, hex view, browser, list panes, pager, a review), and
    // where. See `focused_pane_text`. Thread-local, as the process is
    // single-threaded.
    static PAINTED: std::cell::RefCell<Option<(Rect, String)>> = const { std::cell::RefCell::new(None) };
    // How many views that open over a pane (the pager, a review) are open.
    // Under one, the pane's own frame isn't what's on screen.
    static OVERLAYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Records `frame`, just painted into `rect`, as what that pane shows:
/// the only record there is for views that bypass the pane's grid.
pub(crate) fn record_paint(rect: Rect, frame: &str) {
    PAINTED.with(|p| *p.borrow_mut() = Some((rect, frame.to_string())));
}

// A view open over the focused pane for as long as this lives.
struct OverlayOpen;

impl OverlayOpen {
    fn new() -> OverlayOpen {
        OVERLAYS.with(|o| o.set(o.get() + 1));
        OverlayOpen
    }
}

impl Drop for OverlayOpen {
    fn drop(&mut self) {
        OVERLAYS.with(|o| o.set(o.get().saturating_sub(1)));
    }
}

// What the focused pane shows, as plain text: a line per row, trailing
// blanks trimmed, blank rows at the bottom dropped. No SGR codes; this is
// what `bish session capture` prints, for scripts to grep.
//
// An editor, hex view, browser or list, or the pager or a review over the
// pane, isn't in the grid; for those it's the last frame they painted
// (see `record_paint`), cut to the pane.
fn focused_pane_text(app: &App) -> String {
    let window = &app.windows[app.current_window];
    let painted_over = OVERLAYS.with(|o| o.get()) > 0 || !matches!(window.focused().stack.last(), Some(Frame::Session(_) | Frame::Job(_)));
    let painted = if painted_over { PAINTED.with(|p| p.borrow().clone()) } else { None };
    let rows: Vec<String> = match painted {
        Some((rect, frame)) => painted_rows(rect, &frame, app.term_rows, app.term_cols),
        None => {
            let screen = app.sessions[&window.owning_session()].screen.borrow();
            let (rows, _cols) = screen.size();
            (0..rows).map(|row| screen.row_text(row)).collect()
        }
    };
    let mut lines: Vec<String> = rows
        .into_iter()
        .map(|mut line| {
            while line.ends_with(' ') {
                line.pop();
            }
            line
        })
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.push(String::new());
    lines.join("\n")
}

// What `frame` shows inside `rect`, a row per line. The frame positions
// itself absolutely, so it's replayed onto a `term_rows` by `term_cols`
// screen and the pane read back out.
fn painted_rows(rect: Rect, frame: &str, term_rows: usize, term_cols: usize) -> Vec<String> {
    let mut screen = vt100::Screen::new(term_rows.max(rect.row + rect.rows), term_cols.max(rect.col + rect.cols));
    screen.feed(frame.as_bytes());
    (rect.row..rect.row + rect.rows)
        .map(|row| (rect.col..rect.col + rect.cols).map(|col| screen.cell(row, col)).filter(|cell| !cell.continuation).map(|cell| cell.ch).collect())
        .collect()
}

fn render_row(out: &mut String, screen: &vt100::Screen, row: usize, cols: usize) {
    let mut last: Option<(vt100::Color, vt100::Color, vt100::CellAttrs)> = None;
    let (_, screen_cols) = screen.size();
    for c in 0..cols {
        let mut cell = screen.cell(row, c);
        // Nothing is written for a wide character's second half: the terminal
        // already moved two columns for the first.
        if cell.continuation {
            continue;
        }
        // A wide character whose second half is past the edge is written as a
        // space, as its second column isn't this pane's.
        if c + 1 == cols && c + 1 < screen_cols && screen.cell(row, c + 1).continuation {
            cell.ch = ' ';
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

// A read-only `bishedit::Buffer` over a pane's content: its scrollback,
// oldest first, then its live grid, as one sequence of lines. Its
// cursor is its own; nothing here touches the session's content or
// cursor. Marks (m, `, ') live here too.
struct ScreenBuffer {
    screen: Rc<RefCell<vt100::Screen>>,
    cursor: (usize, usize),
    vtop: usize,
    vheight: usize,
    marks: HashMap<char, (usize, usize)>,
    // `ignorecase`/`smartcase`, taken from the session's shell when normal
    // mode starts. An excursion is short; a change meanwhile isn't worth
    // following.
    ignorecase: bool,
    smartcase: bool,
    // The `iskeyword` bishopt, taken at the same time; see
    // `bishedit::Buffer::word_chars`.
    iskeyword: String,
    // Visual mode's committed selections, in order; the active one is in
    // `VimKeys`. `Z` pushes one, `y` yanks them all (plus the active one)
    // joined, and `y` and Escape clear them.
    selections: Vec<motion::MotionRange>,
    // `line_count()` when the excursion started. Normal mode doesn't follow
    // new output, so the mode line reports how much arrived (see
    // `normal_mode_status_text`): the only hint that a job is still
    // writing.
    entry_line_count: usize,
    // The `new_lines()` value the mode line last showed, so the idle loop
    // redraws only when something arrived.
    reported_new_lines: usize,
}

// The scrollback length for `ScreenBuffer`'s addressing: 0 while the
// alternate screen is showing (a full-screen program like vim; see
// `switch_alt_screen`). Scrollback belongs to the primary grid. Counting
// it above the alternate screen put the program's cursor deep in stale
// history, so normal mode opened showing that instead of the program.
fn addressable_scrollback_len(s: &vt100::Screen) -> usize {
    if s.using_alternate { 0 } else { s.scrollback.len() }
}

// The last row of the live grid that holds anything, `None` if it's all
// blank. A grid is always allocated at full height. Scans upward, so a
// shell pane with blank lower rows stops quickly.
fn last_written_row(s: &vt100::Screen) -> Option<usize> {
    let (rows, cols) = s.size();
    (0..rows).rev().find(|&row| (0..cols).any(|col| s.cell(row, col).ch != ' '))
}

// How many lines normal mode can address: the visible scrollback plus
// the written part of the live grid.
//
// For a shell pane nothing below the cursor is written (and
// `freeze_idle_prompt` puts the prompt on its row), but a program like
// vim parks the cursor on line 1 and fills every row. So the last line is
// whichever is further down: the cursor's row or the last written row.
fn addressable_len(s: &vt100::Screen) -> usize {
    let (cursor_row, _) = s.cursor();
    let last = last_written_row(s).unwrap_or(0).max(cursor_row);
    (addressable_scrollback_len(s) + last + 1).max(1)
}

impl ScreenBuffer {
    fn new(screen: Rc<RefCell<vt100::Screen>>, vheight: usize) -> ScreenBuffer {
        let (sb_len, cur_row, cur_col) = {
            let s = screen.borrow();
            let (row, col) = s.cursor();
            (addressable_scrollback_len(&s), row, col)
        };
        // Starts at the live cursor, in this combined addressing.
        let cursor = (sb_len + cur_row, cur_col);
        let vheight = vheight.max(1);
        let vtop = cursor.0.saturating_sub(vheight - 1);
        let entry_line_count = addressable_len(&screen.borrow());
        ScreenBuffer {
            screen,
            cursor,
            vtop,
            vheight,
            marks: HashMap::new(),
            selections: Vec::new(),
            entry_line_count,
            reported_new_lines: 0,
            ignorecase: false,
            smartcase: false,
            iskeyword: "_".to_string(),
        }
    }

    // The two search options, from the session owning this pane.
    fn set_search_case(&mut self, shell: &exec::Shell) {
        self.iskeyword = shell.bishopt_str("iskeyword");
        self.ignorecase = shell.bishopt_bool("ignorecase");
        self.smartcase = shell.bishopt_bool("smartcase");
    }

    // How many lines the pane has gained since the excursion started (see
    // `entry_line_count`). Saturating: a pane can lose lines (a full-screen
    // program clearing), and a negative count tells nobody anything.
    fn new_lines(&self) -> usize {
        self.line_count().saturating_sub(self.entry_line_count)
    }

    // Whether `new_lines()` moved since last asked: the idle loop's cue to
    // redraw the mode line.
    fn new_lines_changed(&mut self) -> bool {
        let now = self.new_lines();
        if now == self.reported_new_lines {
            return false;
        }
        self.reported_new_lines = now;
        true
    }

    // `line`'s raw cell count: the grid's width for a grid row, or the
    // width when it scrolled off for a scrollback row, which differs after
    // a resize. `char_at`/`line_len` trim trailing blanks from it.
    fn raw_len(&self, line: usize) -> usize {
        let s = self.screen.borrow();
        let sb_len = addressable_scrollback_len(&s);
        if line < sb_len { s.scrollback[line].len() } else { s.size().1 }
    }

    // Addressed by column, not character: the second column of a
    // double-width character reads as a space. The cursor and a rectangular
    // selection can be there, but a search through the scrollback sees
    // `漢 字` where the pane shows `漢字`.
    fn raw_char_at(&self, line: usize, col: usize) -> Option<char> {
        let s = self.screen.borrow();
        let sb_len = addressable_scrollback_len(&s);
        if line < sb_len {
            s.scrollback[line].get(col).map(|c| c.ch)
        } else {
            let row = line - sb_len;
            let (rows, cols) = s.size();
            if row < rows && col < cols { Some(s.cell(row, col).ch) } else { None }
        }
    }
}

impl BisheditBuffer for ScreenBuffer {
    // Not the scrollback plus the grid's full height: the grid is always
    // allocated full height, and counting every row would let G or Ctrl-D
    // move into empty space. See `addressable_len` for where the end is.
    fn line_count(&self) -> usize {
        addressable_len(&self.screen.borrow())
    }

    fn line_len(&self, line: usize) -> usize {
        let raw = self.raw_len(line);
        (0..raw).rev().find(|&c| self.raw_char_at(line, c) != Some(' ')).map(|c| c + 1).unwrap_or(0)
    }

    fn char_at(&self, line: usize, col: usize) -> Option<char> {
        if col < self.line_len(line) { self.raw_char_at(line, col) } else { None }
    }

    fn word_chars(&self) -> &str {
        &self.iskeyword
    }

    fn search_ignore_case(&self, pattern: &str) -> bool {
        crate::bishedit::ignore_case_for(pattern, self.ignorecase, self.smartcase)
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

    fn set_viewport_height(&mut self, rows: usize) {
        self.vheight = rows.max(1);
    }

    fn set_mark(&mut self, name: char, pos: (usize, usize)) {
        self.marks.insert(name, pos);
    }

    fn get_mark(&self, name: char) -> Option<(usize, usize)> {
        self.marks.get(&name).copied()
    }

    // A terminal grid can autowrap a line that ran out of columns (see
    // `vt100::Grid`'s `wrapped`).
    fn line_wraps(&self, line: usize) -> bool {
        let s = self.screen.borrow();
        let sb_len = addressable_scrollback_len(&s);
        if line < sb_len { s.scrollback_wrapped.get(line).copied().unwrap_or(false) } else { s.row_wraps(line - sb_len) }
    }
}

// What the Normal-mode loop (`run_normal_mode_navigation`) navigates:
// a pane's scrollback and grid (`ReadOnly`), or a file while the pane's
// top frame is a `Frame::Edit` (`Editable`). An editor pane is the same
// Normal mode over a buffer that can also be changed. The `Buffer` impl
// below delegates to the live variant, so the buffer-generic helpers
// (`motion::apply_motion`, `editor::yank_motion`, ...) work on both;
// only mutating outcomes match on this enum, as mutation isn't part of
// `Buffer` (see `TextBuffer`).
enum NavBuffer {
    ReadOnly(ScreenBuffer),
    Editable(TextBuffer),
}

impl NavBuffer {
    // See `ScreenBuffer::new_lines_changed`. Only a shell pane grows under
    // the reader; nothing else writes to an `Editable` file meanwhile.
    fn new_lines_changed(&mut self) -> bool {
        match self {
            NavBuffer::ReadOnly(b) => b.new_lines_changed(),
            NavBuffer::Editable(_) => false,
        }
    }

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

    // Not gated on `TextBuffer::is_readonly`: this feeds
    // `run_command_mode`'s `editing`, and `:dbg` needs the buffer to toggle
    // `set_readonly` and breakpoints whether or not editing is allowed.
    // Edits are gated separately through `as_writable_mut`.
    fn as_editable(&self) -> Option<&TextBuffer> {
        match self {
            NavBuffer::ReadOnly(_) => None,
            NavBuffer::Editable(b) => Some(b),
        }
    }

    fn as_editable_mut(&mut self) -> Option<&mut TextBuffer> {
        match self {
            NavBuffer::ReadOnly(_) => None,
            NavBuffer::Editable(b) => Some(b),
        }
    }

    // Whether the buffer can be edited: false for `ReadOnly`, and for an
    // `Editable` buffer under `:dbg` (`TextBuffer::is_readonly`). Every
    // mutating arm in `run_normal_mode_navigation` checks this (or uses
    // `as_writable_mut`) rather than matching `Editable`, so a debugged file
    // is read-only the same way scrollback is.
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
    // Easy to forget: a defaulted method this wrapper doesn't forward
    // answers with the default for both variants, which is how `/` stayed
    // case-sensitive while `:s` didn't.
    fn search_ignore_case(&self, pattern: &str) -> bool {
        match self {
            NavBuffer::Editable(b) => b.search_ignore_case(pattern),
            NavBuffer::ReadOnly(b) => b.search_ignore_case(pattern),
        }
    }

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

    // Forwarded so `Editable` uses its real value; `ScreenBuffer` keeps the
    // default 0.
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

    fn folds(&self) -> Option<&crate::bishedit::fold::Folds> {
        match self {
            NavBuffer::ReadOnly(b) => b.folds(),
            NavBuffer::Editable(b) => b.folds(),
        }
    }

    fn folds_mut(&mut self) -> Option<&mut crate::bishedit::fold::Folds> {
        match self {
            NavBuffer::ReadOnly(b) => b.folds_mut(),
            NavBuffer::Editable(b) => b.folds_mut(),
        }
    }
}

// Scrolls `buf` as little as possible to keep the cursor's line in
// view, as vim does. `content_cols` is the pane's content width (see
// `nav_content_cols`). Horizontal scrolling is a no-op for a
// `ScreenBuffer` (see `Buffer::viewport_left`).
pub(crate) fn scroll_to_show_cursor(buf: &mut dyn BisheditBuffer, content_cols: usize) {
    let (line, col) = buf.cursor();
    let height = buf.viewport_height();
    if line < buf.viewport_top() {
        buf.set_viewport_top(line);
    } else if line >= buf.viewport_top() + height {
        buf.set_viewport_top(line + 1 - height);
    }
    // `viewport_left` is a display column, not a char index; as in
    // `fileeditor::scroll_to_show_cursor`.
    let cursor_col = col_of(&buf.line_chars(line), col);
    let width = content_cols.max(1);
    if cursor_col < buf.viewport_left() {
        buf.set_viewport_left(cursor_col);
    } else if cursor_col >= buf.viewport_left() + width {
        buf.set_viewport_left(cursor_col + 1 - width);
    }
}

// A file gets the scrolling `fileeditor` gives it everywhere else:
// `scrolloff`, wrap-aware walking over visual rows, and drawn-column
// arithmetic for a tabular file. The generic version suits a
// `ScreenBuffer`, whose content is already wrapped terminal output.
fn nav_scroll_to_show_cursor(buf: &mut NavBuffer, content_cols: usize) {
    match buf {
        NavBuffer::Editable(tb) => fileeditor::scroll_to_show_cursor(tb, content_cols),
        NavBuffer::ReadOnly(sb) => scroll_to_show_cursor(sb, content_cols),
    }
}

// Only a file scrolls sideways; a `ScreenBuffer` is already wrapped
// (see `Buffer::viewport_left`).
fn nav_scroll_horizontally(buf: &mut NavBuffer, columns: isize, content_cols: usize) {
    if let NavBuffer::Editable(tb) = buf {
        fileeditor::scroll_horizontally(tb, columns, content_cols);
    }
}

// The content width `buf` scrolls against: a `ScreenBuffer` has no
// gutter, a file does (see `fileeditor::editor_content_cols`).
fn nav_content_cols(buf: &NavBuffer, rect: Rect) -> usize {
    match buf {
        NavBuffer::ReadOnly(_) => rect.cols,
        NavBuffer::Editable(tb) => fileeditor::editor_content_cols(tb, rect),
    }
}

// The (start, end) char columns `range` covers on `line`, or `None` if
// it doesn't reach it. `Linewise` covers the whole row; `Inclusive`
// (charwise Visual, see `active_visual_range`) is full width between
// the endpoint lines and clamped on them, as `motion::extract_text`
// reads it, with `to.1 + 1` because it includes the cursor's character.
fn selection_columns_in_line(range: &motion::MotionRange, line: usize, cols: usize) -> Option<(usize, usize)> {
    if line < range.from.0 || line > range.to.0 {
        return None;
    }
    if range.shape == motion::MotionShape::Linewise {
        return Some((0, cols));
    }
    // The same columns on every row; see the twin in fileeditor.rs.
    if let Some((left, right)) = motion::block_columns(range) {
        return (right + 1 > left).then(|| (left, (right + 1).min(cols)));
    }
    let start = if line == range.from.0 { range.from.1 } else { 0 };
    let end = if line == range.to.0 { range.to.1 + 1 } else { cols };
    Some((start, end))
}

// Draws one row with `search_matches`, (start, end) char columns, in
// reverse video, as `hlsearch` does. The reverse bit is flipped on the
// cell's own attributes, so coloured output keeps its colours. Visual
// selections arrive as more entries on the same list (see
// `render_normal_mode_frame`).
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
            if row < rows && c < scols { s.cell(row, c) } else { vt100::Cell::default() }
        };
        if cell.continuation {
            continue;
        }
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

// All of the pane's rows: the mode line is in the global status row
// (`render_global_status_row`). At least 1, for a zero-height rect.
fn normal_mode_content_rows(rect: Rect) -> usize {
    rect.rows.max(1)
}

// The mode line's label, as vim shows it.
fn mode_label(vk: &VimKeys) -> &'static str {
    match vk.visual_anchor() {
        Some((RegisterShape::Char, _)) => "-- VISUAL --",
        Some((RegisterShape::Line, _)) => "-- VISUAL LINE --",
        Some((RegisterShape::Block, _)) => "-- VISUAL BLOCK --",
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

// A chip in the mode line, drawn un-reversed inside the reversed row so
// it reads as a label, not more status text. The same pill as
// `render_diagnostics_title`, inverted for the reversed background.
fn status_pill(text: &str) -> String {
    format!("\x1b[27m {text} \x1b[7m")
}

// How to leave. Normal mode over a shell pane is entered with Ctrl+G
// and left with `i` (or another insert command) or `:q`; not Escape,
// which clears a selection, nor `q`, which records a macro. None of that
// is guessable, so the mode line says it while the mode is up. The short
// form is for narrow panes.
const NORMAL_MODE_EXIT_HINT: &str = "i or :q to return to the prompt";
const NORMAL_MODE_EXIT_HINT_SHORT: &str = "i to return";

fn normal_mode_status_text(buf: &ScreenBuffer, vk: &VimKeys, command_line: Option<&str>, cols: usize) -> String {
    let left = normal_mode_status_left(vk, command_line);
    let (line, col) = buf.cursor();
    let total = buf.line_count();
    let right = format!("{},{}  {}/{}", line + 1, col + 1, line + 1, total);

    // While a `:`/`/` line is typed the row is that line; no room for
    // chips, and Escape does what people expect there anyway.
    let news = (command_line.is_none() && buf.new_lines() > 0).then(|| {
        let n = buf.new_lines();
        format!("+{n} new line{}", if n == 1 { "" } else { "s" })
    });
    let chips: Vec<Vec<String>> = if command_line.is_some() {
        vec![Vec::new()]
    } else {
        // Widest first; the first that fits wins. The exit hint goes last
        // and shrinks before it vanishes: a narrow pane is where someone
        // stuck here has least to go on.
        let mut candidates = Vec::new();
        for hint in [NORMAL_MODE_EXIT_HINT, NORMAL_MODE_EXIT_HINT_SHORT] {
            if let Some(news) = &news {
                candidates.push(vec![hint.to_string(), news.clone()]);
            }
        }
        if let Some(news) = &news {
            candidates.push(vec![news.clone()]);
        }
        candidates.push(vec![NORMAL_MODE_EXIT_HINT.to_string()]);
        candidates.push(vec![NORMAL_MODE_EXIT_HINT_SHORT.to_string()]);
        candidates.push(Vec::new());
        candidates
    };

    let right_len = right.chars().count();
    for chip_set in &chips {
        let pills: String = chip_set.iter().map(|c| status_pill(c)).collect();
        let gap = if chip_set.is_empty() { "" } else { "  " };
        let body = format!("{left}{gap}{pills}");
        let body_len = editor::visible_len(&body);
        if body_len + right_len > cols {
            continue;
        }
        return format!("{body}{}{right}", " ".repeat(cols - body_len - right_len));
    }
    // Not even the label and position fit.
    let text: String = left.chars().take(cols).collect();
    let text_len = text.chars().count();
    format!("{text}{}", " ".repeat(cols.saturating_sub(text_len)))
}

// The search pattern to highlight: the `/`/`?` text being typed (live
// feedback), else the last search's pattern. The same rule as
// `editor::normal_mode_prompt_and_matches`, which drives a different
// `Buffer` type.
fn active_search_pattern(vk: &VimKeys, buf: &ScreenBuffer) -> Option<String> {
    let pending = vk.pending_display();
    // A pattern being typed is shown even after `:noh`: that's feedback on
    // the typing, not a leftover highlight.
    if !vk.search_highlight_on() && !pending.starts_with('/') && !pending.starts_with('?') {
        return None;
    }
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

// Draws `buf`'s viewport into `rect` (the focused pane's, see
// `pane_rect`) plus the global mode line (`render_global_status_row`).
// Rows past the end are left blank. The terminal cursor goes to the
// navigation cursor, not the mode line, even while a command line is
// being typed there.
fn render_normal_mode_frame(buf: &ScreenBuffer, rect: Rect, vk: &VimKeys, command_line: Option<&str>, term_rows: usize, term_cols: usize) {
    let content_rows = normal_mode_content_rows(rect);
    let total = buf.line_count();
    let pattern = active_search_pattern(vk, buf);
    // Every selection on screen, committed and active, computed once.
    let active = active_visual_range(vk, buf);
    let mut out = String::new();
    for r in 0..content_rows {
        let line = buf.viewport_top() + r;
        out.push_str(&format!("\x1b[{};{}H", rect.row + r + 1, rect.col + 1));
        if line < total {
            // Only the visible rows are searched: a match elsewhere can't
            // be seen, and scrollback can be large.
            let mut matches = match &pattern {
                Some(p) => motion::find_matches_in_line(buf, line, p),
                None => Vec::new(),
            };
            // Selections draw as reverse video too, as more entries on the
            // same list.
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

// `GotoFirstWindow`/`GotoLastWindow` aren't here: an absolute jump has
// no `WindowAction`, and `dispatch_window_cmd` handles them itself.
fn window_cmd_to_action(cmd: WindowCmd) -> WindowAction {
    match cmd {
        WindowCmd::Next => WindowAction::Next,
        WindowCmd::Previous => WindowAction::Previous,
        WindowCmd::New => WindowAction::New { name: None, command: None },
        WindowCmd::Close => WindowAction::Close,
        WindowCmd::Split => WindowAction::Split { horizontal: false, command: None },
        WindowCmd::VSplit => WindowAction::Split { horizontal: true, command: None },
        WindowCmd::FocusLeft => WindowAction::FocusPane(PaneDirection::Left),
        WindowCmd::FocusDown => WindowAction::FocusPane(PaneDirection::Down),
        WindowCmd::FocusUp => WindowAction::FocusPane(PaneDirection::Up),
        WindowCmd::FocusRight => WindowAction::FocusPane(PaneDirection::Right),
        WindowCmd::Zoom => WindowAction::Zoom,
        WindowCmd::NextLayout => WindowAction::SetLayout(None),
        WindowCmd::SwapPane => WindowAction::SwapPane,
        WindowCmd::BreakPane => WindowAction::BreakPane,
        WindowCmd::Balance => WindowAction::Balance,
        WindowCmd::Minimize => WindowAction::Minimize,
        WindowCmd::GotoFirstWindow | WindowCmd::GotoLastWindow => {
            unreachable!("handled directly in dispatch_window_cmd, never reaches here")
        }
    }
}

// Applies a resolved `<C-w>{cmd}`, from either `NavBuffer` variant (see
// `NavBuffer`). The caller leaves its loop afterwards, as focus moved.
fn dispatch_window_cmd(app: &mut App, cmd: WindowCmd, count: Option<usize>) {
    match cmd {
        // No `WindowAction` for an absolute jump, so `current_window` is
        // set directly; it still needs the freeze `apply_window_action`
        // does first (see `freeze_focused_idle_prompt`).
        WindowCmd::GotoFirstWindow | WindowCmd::GotoLastWindow => {
            freeze_focused_idle_prompt(app);
            let default = if cmd == WindowCmd::GotoFirstWindow { 1 } else { app.windows.len() };
            let target = count.unwrap_or(default);
            app.current_window = target.saturating_sub(1).min(app.windows.len().saturating_sub(1));
            compositor_redraw(app);
        }
        _ => {
            let action = window_cmd_to_action(cmd);
            for _ in 0..count.unwrap_or(1).max(1) {
                let action = action.clone();
                apply_window_action(app, action);
            }
        }
    }
}

// What's covering normal mode's view. Each stays until the next key,
// which `run_normal_mode_navigation` then handles as usual after
// redrawing, so the key that clears an overlay isn't spent on it. From
// `Output`, Ctrl-L switches to `Transcript` instead of clearing, as in
// command mode.
#[derive(Clone, Copy, PartialEq)]
enum PendingView {
    None,
    // A just-run command's result (`CommandModeOutcome::Ran`), drawn by
    // `render_command_output_overlay`. Raised with empty output for a failure
    // that printed nothing, so it still gets a coloured bar.
    Output,
    Transcript,
}

// What `run_normal_mode_navigation` is driving: picks the `NavBuffer`
// variant and decides what its `NavExit` means.
enum NavStart {
    // Ctrl+G at a live prompt, mid-typing `text` with the cursor at
    // `cursor`; resumable (see `NavExit::Resume`).
    //
    // `wheel` is set when a wheel notch opened this view (see
    // `editor::ReadOutcome::NormalMode`), and scrolls once the buffer exists,
    // or the gesture that opened the view would be swallowed.
    Prompt { text: String, cursor: usize, wheel: Option<editor::MouseEvent> },
    // Ctrl+G detaching a running foreground job: read-only like `Prompt`,
    // but the pane's top frame stays `Frame::Job`, so nothing to resume.
    JobDetach,
    // A `Frame::Edit` pane: the buffer and keys are moved in and handed
    // back with the `NavExit`. Boxed so the enum isn't sized by this variant.
    Edit(Box<TextBuffer>, Box<VimKeys>),
}

// How `run_normal_mode_navigation` ended. Only `Prompt` produces
// `Resume` (back to the prompt's editor), only `Edit` produces `Quit`
// (`:q`, `:wq`, `:x`, `:q!`, `ZZ`, popping the `Frame::Edit`). `Detached`
// covers the rest: focus moved, or EOF.
enum NavExit {
    Resume(String, usize),
    Detached,
    Quit,
    /// `gd` found a definition in another file.
    ///
    /// The loop can't open a frame (it has neither `edit_frames` nor the id
    /// counter), so it hands the request to `run_edit_frame`, as `Quit` and
    /// `Detached` do.
    ///
    /// The position stays the server's `(line, character)` in `encoding`:
    /// converting it needs the text of a file not yet opened.
    // `root`: the project root serving the buffer the jump came from; the
    // opened file inherits it (see `TextBuffer::lsp_root`).
    OpenAt {
        path: PathBuf,
        line: usize,
        character: usize,
        encoding: crate::lsp::PositionEncoding,
        root: Option<PathBuf>,
    },
}

// Hands `buf` and `vk` back to `run_edit_frame` when the loop started
// from `NavStart::Edit`; `None` for the read-only starts, which have
// nothing to return.
fn nav_buffer_into_edit_state(buf: NavBuffer, vk: VimKeys) -> Option<(TextBuffer, VimKeys)> {
    match buf {
        NavBuffer::Editable(tb) => Some((tb, vk)),
        NavBuffer::ReadOnly(_) => None,
    }
}

// Gives a shell the opaque view of `table` that `::bish lsp` reports
// through (see `exec::ServiceTable`). The same `RefCell` the caller
// holds, so the two can't diverge.
fn install_service_table(shell: &mut Shell, table: &Rc<RefCell<lspclient::Table>>) {
    shell.lsp = Rc::clone(table) as Rc<RefCell<dyn exec::ServiceTable>>;
}

// The same for the history, which the `history` builtin reaches through
// `exec::HistoryAccess`; see that trait for why a shell can't own one.
fn install_history(shell: &mut Shell, history: &Rc<RefCell<History>>) {
    shell.history = Rc::clone(history) as Rc<RefCell<dyn exec::HistoryAccess>>;
}

/// One place a location list points at.
///
/// Holds the server's `(line, character)` and encoding rather than a
/// cursor position: resolving needs the file's text, which for entries
/// outside the current buffer may never be read.
struct LocationItem {
    path: PathBuf,
    line: usize,
    character: usize,
    encoding: crate::lsp::PositionEncoding,
    /// What the row shows: where, and the source line if read cheaply.
    label: String,
}

struct LocationList {
    /// The pill in the pane's divider: "12 references".
    title: String,
    items: Vec<LocationItem>,
}

// How many entries a list keeps. A `gr` on something common can answer
// with thousands, and a pane shows a few dozen.
const MAX_LOCATIONS: usize = 200;

// Turns a server's answer into a list, reading each distinct file once
// for the source line on each row; `path:12` alone doesn't say which
// use it is. Bounded to `MAX_LOCATIONS` entries and one read per file.
// An unreadable file still gets a row, without its line.
fn location_list(title: &str, locations: &[crate::lsp::Location], encoding: crate::lsp::PositionEncoding, base: Option<&Path>) -> LocationList {
    let mut cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut items = Vec::new();
    for location in locations.iter().take(MAX_LOCATIONS) {
        let Some(path) = crate::url::to_file_path(&location.uri) else { continue };
        let lines = cache
            .entry(path.clone())
            .or_insert_with(|| std::fs::read_to_string(&path).map(|text| text.lines().map(str::to_string).collect()).unwrap_or_default());
        // Relative to the file asked from, so the interesting part shows.
        let shown = base.and_then(|base| path.strip_prefix(base).ok()).unwrap_or(&path).display().to_string();
        let text = lines.get(location.start.0).map(|l| l.trim()).unwrap_or("");
        let label = if text.is_empty() { format!("{shown}:{}", location.start.0 + 1) } else { format!("{shown}:{}  {text}", location.start.0 + 1) };
        items.push(LocationItem { path, line: location.start.0, character: location.start.1, encoding, label });
    }
    LocationList { title: title.to_string(), items }
}

// `gO`'s outline, as a location list. Same pane, different producer --
// which is the whole reason that pane holds `LocationItem`s rather than
// anything reference-shaped.
//
// Indented by nesting depth, so a method reads as belonging to its
// struct, and labelled with the symbol's own kind: the line number is
// far less useful here than in a reference list, where the point *is*
// which line.
fn symbol_list(title: &str, symbols: &[crate::lsp::Symbol], encoding: crate::lsp::PositionEncoding) -> LocationList {
    let items = symbols
        .iter()
        .take(MAX_LOCATIONS)
        .filter_map(|symbol| {
            let path = crate::url::to_file_path(&symbol.uri)?;
            let indent = "  ".repeat(symbol.depth);
            let label =
                if symbol.kind.is_empty() { format!("{indent}{}", symbol.name) } else { format!("{indent}{}  [{}]", symbol.name, symbol.kind) };
            Some(LocationItem { path, line: symbol.start.0, character: symbol.start.1, encoding, label })
        })
        .collect();
    LocationList { title: title.to_string(), items }
}

// `:sym`'s answer, as a location list. Unlike `symbol_list`, the file
// is shown: a workspace symbol is usually in a file the user isn't
// looking at.
fn workspace_symbol_list(title: &str, symbols: &[crate::lsp::Symbol], encoding: crate::lsp::PositionEncoding, base: Option<&Path>) -> LocationList {
    let items = symbols
        .iter()
        .take(MAX_LOCATIONS)
        .filter_map(|symbol| {
            let path = crate::url::to_file_path(&symbol.uri)?;
            let shown = base.and_then(|base| path.strip_prefix(base).ok()).unwrap_or(&path).display().to_string();
            let kind = if symbol.kind.is_empty() { String::new() } else { format!("  [{}]", symbol.kind) };
            let label = format!("{}{kind}  {shown}:{}", symbol.name, symbol.start.0 + 1);
            Some(LocationItem { path, line: symbol.start.0, character: symbol.start.1, encoding, label })
        })
        .collect();
    LocationList { title: title.to_string(), items }
}

// Bish's own chrome colours (see theme.rs), read fresh from `shell`.
fn ui_colors(shell: &exec::Shell) -> crate::theme::UiColors {
    crate::theme::UI_COL_OPTIONS.iter().filter_map(|(element, name)| shell.bishopt_color(name).map(|c| (*element, c))).collect()
}

// A session's `::bish hl` colours (see `highlight::HL_NAMES`), read
// fresh from `shell`. Callers keep a copy and re-take it when they can
// change: command mode runs builtins, so `::bish hl --set` works from
// the editor's colon line.
fn syntax_color_overrides(shell: &exec::Shell) -> highlight::ColorOverrides {
    highlight::HL_NAMES.iter().filter_map(|(kind, name)| shell.hl_color(name).map(|c| (*kind, c))).collect()
}

// How the editor lays out a line wider than the pane, read fresh from
// the shell like `syntax_color_overrides`, so `bishopt --set wrap on`
// shows on the next redraw.
fn wrap_options(shell: &exec::Shell) -> crate::bishedit::wrap::Options {
    crate::bishedit::wrap::Options {
        wrap: shell.bishopt_bool("wrap"),
        linebreak: shell.bishopt_bool("linebreak"),
        breakindent: shell.bishopt_bool("breakindent"),
        showbreak: shell.bishopt_str("showbreak"),
        column: shell.bishopt_int("wrap_column").max(0) as usize,
        scrolloff: shell.bishopt_int("scrolloff").max(0) as usize,
        sidescrolloff: shell.bishopt_int("sidescrolloff").max(0) as usize,
        extends: shell.bishopt_str("extends"),
        precedes: shell.bishopt_str("precedes"),
    }
}

// The settings a buffer gets from the shell alone, before
// `.editorconfig` has a say.
fn apply_shell_options(shell: &exec::Shell, buf: &mut TextBuffer) {
    buf.expandtab = shell.bishopt_bool("expandtab");
    buf.shiftwidth = shell.bishopt_int("shiftwidth").max(1) as usize;
    buf.tabstop = shell.bishopt_int("tabstop").max(1) as usize;
    buf.trim_trailing_whitespace = shell.bishopt_bool("trim_trailing_whitespace");
    buf.final_newline = shell.bishopt_bool("fixendofline") || buf.final_newline;
    match shell.bishopt_str("fileformat").as_str() {
        "unix" => buf.eol = crate::editorconfig::Eol::Lf,
        "dos" => buf.eol = crate::editorconfig::Eol::Crlf,
        "mac" => buf.eol = crate::editorconfig::Eol::Cr,
        // Empty, the default: keep what the file had, so a CRLF file isn't
        // rewritten on first save.
        _ => {}
    }
}

// What the project's `.editorconfig` says, if the `editorconfig`
// bishopt allows it. Applied after the shell's settings and overriding
// them: bishopts are a preference, a project's written conventions are
// specific. Turning the option off takes a buffer back.
fn apply_editorconfig(shell: &exec::Shell, buf: &mut TextBuffer) {
    if !shell.bishopt_bool("editorconfig") {
        return;
    }
    let Some(path) = buf.path().map(|p| p.to_path_buf()) else { return };
    // Against the absolute path: the `.editorconfig` cascade depends on where
    // the file is, and `e src/main.rs` is relative.
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let properties = crate::editorconfig::for_file(&path);
    if let Some(style) = properties.indent_style {
        buf.expandtab = style == crate::editorconfig::IndentStyle::Space;
    }
    if let Some(width) = properties.width() {
        buf.shiftwidth = width.max(1);
    }
    if let Some(width) = properties.tab_width {
        buf.tabstop = width.max(1);
    }
    if let Some(eol) = properties.end_of_line {
        buf.eol = eol;
    }
    if let Some(trim) = properties.trim_trailing_whitespace {
        buf.trim_trailing_whitespace = trim;
    }
    if let Some(final_newline) = properties.insert_final_newline {
        buf.final_newline = final_newline;
    }
    // `max_line_length` is `wrap_column`; `off` is `wrap_column = 0`.
    if let Some(max) = properties.max_line_length {
        buf.wrap.column = max.unwrap_or(0);
    }
    // A charset this shell can't write is left alone: the buffer keeps the
    // encoding it was read as, and the file is written back as it came.
    if let Some((charset, encoding)) = properties.charset.as_deref().and_then(|c| Some((c, crate::encoding::parse(c)?))) {
        buf.encoding = encoding;
        buf.bom = crate::encoding::names_a_bom(charset) || encoding.requires_bom();
    }
}

// Applies the shell's and the project's settings to a buffer. Called as
// `run_edit_frame` starts driving it and after every command-mode
// command, the one way settings change while the editor holds the
// terminal.
fn apply_view_options(shell: &exec::Shell, buf: &mut TextBuffer) {
    buf.wrap = wrap_options(shell);
    buf.tabular = tabular_style(shell, &fileeditor::language_of(buf));
    buf.hyperlinks = shell.bishopt_bool("hyperlinks");
    buf.relativenumber = shell.bishopt_bool("relativenumber");
    buf.colors = Some(ui_colors(shell));
    buf.cursorshape = shell.bishopt_bool("cursorshape");
    buf.mouse = shell.bishopt_bool("mouse");
    buf.inlayhints = shell.bishopt_bool("inlayhints");
    buf.matchbrackets = shell.bishopt_bool("matchbrackets");
    buf.iskeyword = shell.bishopt_str("iskeyword");
    buf.ignorecase = shell.bishopt_bool("ignorecase");
    buf.smartcase = shell.bishopt_bool("smartcase");
    // Shell settings first, the project's over them.
    apply_shell_options(shell, buf);
    apply_editorconfig(shell, buf);
}

// Whether this buffer's columns line up, and how. The `tabular`
// bishopt's glob is the user's choice of languages; `tabular::style` is
// whether the language has a tabular form at all, which is why the glob
// can default to `*`.
fn tabular_style(shell: &exec::Shell, language: &str) -> Option<crate::bishedit::tabular::Style> {
    let languages = shell.bishopt_str("tabular");
    if !crate::glob::matches(&languages, language) {
        return None;
    }
    crate::bishedit::tabular::style(language)
}

// Insert mode's idle tick: services everything, and when a repaint is
// due returns the geometry to repaint into, not what was measured
// before Insert mode began. See `fileeditor::IdleRedraw`.
fn insert_idle(app: &mut App, session_id: SessionId, buf: &mut TextBuffer) -> Option<fileeditor::IdleRedraw> {
    let repaint = service_background_jobs(app);
    // The Insert-mode counterpart of the navigation loop's call (see
    // `App::focus`); typing moves the cursor too.
    if publish_focus_buffer(app, buf) {
        notify_selection(app);
    }
    // Typing is what document sync is for. This runs only while stdin is
    // quiet, which is the settling the debounce measures.
    sync_language_server_document(&mut app.sessions, session_id, buf);
    // Diagnostics are drawn the tick they arrive, which is what makes them
    // feel live.
    let diagnosed = apply_language_server_diagnostics(&mut app.sessions, session_id, buf);
    if diagnosed {
        refresh_diagnostics_title(app, &buf.diagnostics);
    }
    let recoloured = sync_semantic_tokens(&mut app.sessions, session_id, buf);
    let reprogressed = sync_language_server_progress(&mut app.sessions, session_id, buf);
    let rewatched = sync_watched_file(app.file_watcher.as_mut(), buf);
    // Marks follow the cursor, and typing moves it.
    //
    // A debug adapter isn't serviced here: a breakpoint hit while someone
    // types mustn't move their cursor. The stop shows on the next Normal
    // mode tick, where `:dbg` answers appear anyway.
    let remarked = sync_document_highlights(&mut app.sessions, session_id, buf);
    let rehinted = sync_inlay_hints(&mut app.sessions, session_id, buf);
    (repaint || diagnosed || recoloured || reprogressed || rewatched || remarked || rehinted).then(|| fileeditor::IdleRedraw {
        rect: pane_rect(&app.windows[app.current_window], app.windows[app.current_window].focused_pane, app.term_rows, app.term_cols),
        term_rows: app.term_rows,
        term_cols: app.term_cols,
    })
}

// A cursor inside a closed fold sits on the row standing for it, as in
// vim. Motions see to this (`motion::apply_motion`); this covers the
// rest: `:N`, undo, a fold closing around the cursor.
fn rest_on_fold(buf: &mut dyn BisheditBuffer) {
    let (line, col) = buf.cursor();
    let (start, _) = buf.fold_span(line);
    if start != line {
        buf.set_cursor(start, col.min(buf.line_len(start).saturating_sub(1)));
    }
}

// Draws `buf` in Normal mode with the renderer for its kind:
// `render_normal_mode_frame` or `fileeditor::render_editor_frame`.
// Insert and Replace mode draw inside `fileeditor::run_insert_mode`.
//
// Takes `&mut` so the `Editable` arm can `checkpoint_undo`: every
// top-level key outcome ends here, including after Insert mode returns,
// so a whole Insert session becomes one undo step.
fn render_nav_frame(
    buf: &mut NavBuffer,
    vk: &VimKeys,
    rect: Rect,
    term_rows: usize,
    term_cols: usize,
    color_overrides: Option<&highlight::ColorOverrides>,
) {
    // The viewport height is a stored field with no resize hook, so each
    // redraw resyncs it from the rect, or a shrink leaves scrolling working
    // from the old height.
    buf.set_viewport_height(normal_mode_content_rows(rect));
    // Not while selecting: a Visual selection may reach into a closed fold,
    // and takes it whole.
    if !vk.is_visual() {
        rest_on_fold(buf);
    }
    match buf {
        // A `ReadOnly` view replays a rendered grid; there's no syntax
        // highlighting to override.
        NavBuffer::ReadOnly(sb) => render_normal_mode_frame(sb, rect, vk, None, term_rows, term_cols),
        NavBuffer::Editable(tb) => {
            tb.checkpoint_undo();
            fileeditor::render_editor_frame(tb, vk, fileeditor::EditorMode::Normal, rect, term_rows, term_cols, color_overrides);
        }
    }
}

// The first step of Visual mode's `Z`, `y`, `d`, `c`, `p`, `S` and
// Escape: commit the active selection, if any.
fn commit_active_selection(vk: &VimKeys, buf: &mut NavBuffer) {
    if let Some(range) = active_visual_range(vk, &*buf) {
        buf.selections_mut().push(range);
    }
}

// The Normal-mode loop, over a pane's scrollback and grid
// (`NavStart::Prompt`/`JobDetach`) or a file (`NavStart::Edit`); see
// `NavBuffer`. Ctrl+G enters it. `:` opens command mode
// (`handle_command_mode`): cancelling returns here, and a command that
// ran leaves its output up as `PendingView::Output` until the next key.
// `ZZ` and `:q` leave; so does `<C-w>{cmd}` (`apply_window_action`, run
// `count` times) and a `:window` action, landing at the new pane's
// prompt.
//
// Insert commands (`i`, `a`, `A`, ...) return to the prompt and apply to
// the line as it was when Ctrl+G was pressed (`apply_insert_cmd`), not
// to where the navigation cursor went; `:q` restores that line too. A
// focus change drops it, as no per-session slot holds an unsubmitted
// line.
//
// This loop blocks: other panes' jobs keep running via
// `service_background_jobs`, but aren't repainted until it exits.
fn run_normal_mode_navigation(
    app: &mut App,
    session_id: SessionId,
    // `Some` iff `start` is `NavStart::Edit`: the key of `debug_frames`
    // (see `Frame::DebugRun`), for `K`'s value lookup and `:dbg` in
    // `handle_command_mode`. The read-only starts have no file to debug.
    edit_frame_id: Option<EditFrameId>,
    start: NavStart,
    color_overrides: Option<&highlight::ColorOverrides>,
) -> io::Result<(NavExit, Option<(TextBuffer, VimKeys)>)> {
    // Owned, and refreshed after every command: `:bishopt` runs from the
    // colon line, so a copy taken once goes stale. `insert_abbrs` is
    // refreshed for `:abbr` the same way.
    let mut color_overrides: Option<highlight::ColorOverrides> = color_overrides.cloned();
    let mut rect = pane_rect(&app.windows[app.current_window], app.windows[app.current_window].focused_pane, app.term_rows, app.term_cols);

    // Only `Prompt` has text to resume into (see `NavExit::Resume`); the
    // other starts get empty values so the `ReadOnly` handling of insert
    // commands, `ZZ` and `:q` stays shared. `JobDetach`'s caller discards
    // them, and `Edit` never gets there.
    let (initial_text, initial_cursor) = match &start {
        NavStart::Prompt { text, cursor, .. } => (text.clone(), *cursor),
        NavStart::JobDetach | NavStart::Edit(..) => (String::new(), 0),
    };
    let original_chars: Vec<char> = initial_text.chars().collect();

    // The wheel notch that opened this view, if one did; applied once `buf`
    // exists.
    let entered_by_wheel = match &start {
        NavStart::Prompt { wheel, .. } => *wheel,
        NavStart::JobDetach | NavStart::Edit(..) => None,
    };

    let (mut buf, mut vk) = match start {
        NavStart::Prompt { text, cursor, .. } => {
            // `read_line` draws the prompt straight to the terminal, never
            // into the grid, and this is about to read the grid. Ctrl+G
            // doesn't change focus (the other `freeze_idle_prompt` callers
            // do), so without this the first entry into normal mode would
            // show a blank pane.
            let screen = app.sessions[&session_id].screen.clone();
            let mut sb = ScreenBuffer::new(screen, normal_mode_content_rows(rect));
            sb.set_search_case(&app.sessions[&session_id].shell);
            let prompt_str = freeze_input_with_text(app.sessions.get_mut(&session_id).unwrap(), &text);
            // Placed explicitly: the grid's cursor is where `feed()` left
            // it, at the end of `text`, but navigation should start where
            // editing stopped, as vim's Normal mode does after Insert.
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
            let screen = app.sessions[&session_id].screen.clone();
            let mut sb = ScreenBuffer::new(screen, normal_mode_content_rows(rect));
            sb.set_search_case(&app.sessions[&session_id].shell);
            (NavBuffer::ReadOnly(sb), VimKeys::new())
        }
        NavStart::Edit(tb, vk0) => (NavBuffer::Editable(*tb), *vk0),
    };

    // `::bish map`, snapshotted the same way and for the same reason as
    // `color_overrides` above -- the colon line can redefine a mapping
    // mid-session, so this is refreshed after every command-mode command
    // rather than captured once.
    vk.set_mappings(app.sessions.get(&session_id).map(|s| s.shell.mappings.clone()).unwrap_or_default());

    // The `mouse` bishopt: off means reporting is never enabled, so the
    // terminal keeps its own click-and-drag selection.
    let mouse = app.sessions.get(&session_id).is_none_or(|s| s.shell.bishopt_bool("mouse"));
    let _guard = term::RawGuard::enable_maybe_mouse(0, mouse)?;
    // `mouse_hover`: motion reports with no button held, which is the
    // only way a terminal can say where the pointer is *resting*.
    //
    // Not asked for in `RawGuard` with the rest of the mouse, and not
    // just switched on for the length of this call: this same loop also
    // drives a plain prompt, where nothing reads these and they are a
    // report per cell of pure traffic. So it follows the buffer --
    // see the `set` at the top of the loop below.
    let hover_wanted = mouse && app.sessions.get(&session_id).is_none_or(|s| s.shell.bishopt_bool("mouse_hover"));
    // Repaints the whole screen first (needed the first time normal mode
    // promotes, as the alternate screen starts blank), then the pane.
    compositor_redraw(app);
    // The notch that opened this view scrolls it before the first frame,
    // so a wheel flick moves the content at once. As the wheel branch in
    // the loop below does for later notches.
    if let Some(ev) = entered_by_wheel {
        let motion = if ev.is_scroll_down() { motion::Motion::Down } else { motion::Motion::Up };
        editor::apply_motion_or_reselect(&mut vk, &mut buf, motion, Some(fileeditor::MOUSE_WHEEL_LINES));
        let content_cols = nav_content_cols(&buf, rect);
        nav_scroll_to_show_cursor(&mut buf, content_cols);
    }
    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
    let mut pending_view = PendingView::None;
    // The last left press, to tell a double-click from two clicks: the
    // terminal reports neither, so timing and same-cell checks happen here.
    let mut last_press: Option<(std::time::Instant, u16, u16)> = None;
    // Presses in a row on the same cell within the double-click window: 1
    // places the cursor, 2 selects the word, 3 the line.
    let mut click_streak: u32 = 0;
    let mut ranged_selection = false;
    // Where the pointer came to rest and when, for hover, and whether a
    // popup is showing. A dwell, because the terminal reports every cell
    // the pointer crosses, and a sweep would ask the language server once
    // per cell. See `HOVER_DWELL`.
    let mut hover_at: Option<(std::time::Instant, u16, u16)> = None;
    let mut hover_shown = false;
    // What `gd` (or `gy`/`gD`) last found, which one the cursor is on, and
    // which question was asked, so cycling says "type definition 2/3".
    // `None` otherwise, which keeps `n`/`N` repeating the search. Like
    // `PendingView`, the next unrelated key resolves it away.
    let mut definitions: Option<(GotoKind, Vec<crate::lsp::Location>, usize)> = None;

    let result: (NavExit, Option<(TextBuffer, VimKeys)>) = 'nav: loop {
        // Recomputed each iteration: `:diag`, run from this loop's `:`, can
        // put a diagnostics pane below this one and shrink it.
        rect = pane_rect(&app.windows[app.current_window], app.windows[app.current_window].focused_pane, app.term_rows, app.term_cols);
        // Hover reports only while a file is in front: a prompt has nothing
        // to hover over.
        term::set_hover_tracking(hover_wanted && matches!(buf, NavBuffer::Editable(_)));
        // Wait for input here rather than in `vk.next_key`'s idle closure:
        // that call borrows `vk` throughout, so its closure couldn't redraw
        // `buf` and `vk`, and a newly attached client would see nothing until
        // a key arrived. Keys already queued (a macro, a mapping) skip the
        // wait.
        while !vk.has_pending_keys() && !term::stdin_ready(editor::IDLE_POLL_MS) {
            // The pointer has rested past the dwell: show what `K` would
            // say about that cell. Here because this is where the editor
            // waits while nothing is typed, and it can reach `buf` and
            // `vk`.
            if let Some(popup) = hover_popup_due(app, session_id, edit_frame_id, &buf, rect, &mut hover_at, &mut hover_shown) {
                print!("{popup}");
                let _ = io::stdout().flush();
            }
            let mut attached = service_background_jobs(app);
            // The editor at rest in Normal mode; see `pending_reviews`. The
            // buffer in front isn't in `edit_frames`, so `review_proposal`
            // can't reload it; it's followed here instead, unless it has
            // unsaved changes.
            if open_pending_reviews(app) {
                attached = true;
                if let Some(tb) = buf.as_editable_mut()
                    && !tb.is_dirty()
                    && tb.changed_on_disk()
                {
                    let _ = tb.reload();
                }
            }
            // From here because this is one of two places holding both
            // `App` and the buffer (see `App::focus`). A change is also
            // what triggers the notification below.
            if publish_focus(app, &buf) {
                notify_selection(app);
            }
            // Normal-mode edits (`dd`, `p`, what Insert mode left behind)
            // reach the server from here, on the idle path: when the user
            // has stopped typing and is waiting to hear back.
            let mut diagnosed = false;
            if let Some(tb) = buf.as_editable_mut() {
                sync_language_server_document(&mut app.sessions, session_id, tb);
                diagnosed = apply_language_server_diagnostics(&mut app.sessions, session_id, tb);
                if diagnosed {
                    refresh_diagnostics_title(app, &tb.diagnostics);
                }
                diagnosed |= sync_semantic_tokens(&mut app.sessions, session_id, tb);
                diagnosed |= sync_language_server_progress(&mut app.sessions, session_id, tb);
                diagnosed |= sync_watched_file(app.file_watcher.as_mut(), tb);
                if let Some(id) = edit_frame_id {
                    diagnosed |= sync_debug_adapter(app, id, tb);
                }
                diagnosed |= sync_document_highlights(&mut app.sessions, session_id, tb);
                diagnosed |= sync_inlay_hints(&mut app.sessions, session_id, tb);
            }
            // A job this pane stepped away from may still be writing, and
            // the mode line's "+N new lines below" should keep up. The
            // viewport doesn't move, so only the status row changes.
            let grew = buf.new_lines_changed();
            if attached || grew || diagnosed {
                // `service_background_jobs` repainted, which blanks a
                // `Frame::Edit` pane (see `compositor_redraw`), so redraw
                // the buffer. Re-measured first, as a resize is one reason
                // it repainted.
                rect = pane_rect(&app.windows[app.current_window], app.windows[app.current_window].focused_pane, app.term_rows, app.term_cols);
                if let Some(tb) = buf.as_editable_mut() {
                    // A shrink may have left the cursor below the viewport;
                    // scrolling needs the new height to tell, so it's set
                    // first.
                    tb.set_viewport_height(normal_mode_content_rows(rect));
                    fileeditor::scroll_to_show_cursor(tb, fileeditor::editor_content_cols(tb, rect));
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
        }
        // A byte is ready, so `read_key_idle` returns without calling this
        // closure.
        let mut key = match vk.next_command_key(|| editor::read_key_idle(&mut || {}))? {
            Some(k) => k,
            // EOF: nothing to resume into. An `Edit` session is dropped
            // rather than left attached with no way to drive it.
            None => {
                let exit = if matches!(buf, NavBuffer::Editable(_)) { NavExit::Quit } else { NavExit::Detached };
                break 'nav (exit, nav_buffer_into_edit_state(buf, vk));
            }
        };

        // A key acknowledges an echoed server message (see
        // `TextBuffer::echoed_message`), as in vim; whatever the key does
        // ends in a redraw.
        if let Some(tb) = buf.as_editable_mut() {
            tb.echoed_message = None;
        }

        // Resolve whatever `PendingView` covers the screen first. A loop:
        // Ctrl-L on `Output` switches to `Transcript`, which takes another
        // key.
        loop {
            match pending_view {
                PendingView::None => break,
                PendingView::Output if key == Key::CtrlL => {
                    pending_view = PendingView::Transcript;
                    render_command_transcript(&app.sessions[&session_id].command_transcript, app.term_rows, app.term_cols);
                    key = match vk.next_key(|| {
                        editor::read_key_idle(&mut || {
                            service_background_jobs(app);
                        })
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
                    compositor_redraw(app);
                    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                    break;
                }
            }
        }

        // Mouse events are handled here, before `vk.feed`. A click that moves
        // focus leaves normal mode (`NavExit::Detached`), as `<C-w>` does. A
        // wheel notch scrolls the view with the same motions as a key would.
        // A miss, a click on what already has focus, or another event just
        // re-renders, in case an overlay was cleared above.
        if let Key::Mouse(ev) = key {
            // The pointer moving with nothing held: noted, and the dwell
            // check below decides when it has rested. Any movement hides a
            // popup, which was about the cell just left.
            if ev.is_hover() {
                if hover_at.is_none_or(|(_, r, c)| (r, c) != (ev.row, ev.col)) {
                    hover_at = Some((std::time::Instant::now(), ev.row, ev.col));
                    if std::mem::take(&mut hover_shown) {
                        render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                    }
                }
                continue 'nav;
            }
            // Anything else the mouse does overtakes the popup; the arms
            // below redraw.
            hover_at = None;
            hover_shown = false;
            if ev.is_left_click() {
                match hit_test_click(app, ev) {
                    Some(ClickTarget::Window(idx)) if idx != app.current_window => {
                        freeze_focused_idle_prompt(app);
                        app.current_window = idx;
                        compositor_redraw(app);
                        return Ok((NavExit::Detached, nav_buffer_into_edit_state(buf, vk)));
                    }
                    Some(ClickTarget::Pane(pane_id)) if pane_id != app.windows[app.current_window].focused_pane => {
                        if matches!(app.windows[app.current_window].stack().last(), Some(Frame::Session(_))) {
                            let sid = app.windows[app.current_window].owning_session();
                            freeze_idle_prompt(app.sessions.get_mut(&sid).unwrap());
                        }
                        // Going to a pane is how you get a minimized one back.
                        restore_if_minimized(&mut app.windows[app.current_window], pane_id);
                        app.windows[app.current_window].focused_pane = pane_id;
                        compositor_redraw(app);
                        return Ok((NavExit::Detached, nav_buffer_into_edit_state(buf, vk)));
                    }
                    // A click in the focused pane: put the cursor there and
                    // arm a Visual selection, so a drag selects. A plain
                    // click disarms it on release (below), leaving no empty
                    // selection for Escape to clear.
                    Some(ClickTarget::Divider(divider)) => {
                        drag_divider(app, &divider);
                        // The pane may have changed size; `rect` is
                        // recomputed at the top of the next iteration.
                        render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                        continue 'nav;
                    }
                    Some(ClickTarget::Pane(_)) => {
                        let row0 = (ev.row as usize).saturating_sub(1);
                        let col0 = (ev.col as usize).saturating_sub(1);
                        // A gutter click means something per column; see
                        // `gutter_click`.
                        if let Some(tb) = buf.as_editable_mut()
                            && let Some((line, column)) = fileeditor::gutter_at_screen(tb, rect, row0, col0)
                        {
                            let outcome = gutter_click(app, edit_frame_id, tb, line, column);
                            last_press = None;
                            click_streak = 0;
                            render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                            if let Some(message) = outcome {
                                show_command_mode_error(&message, app.term_rows, app.term_cols);
                            }
                            continue 'nav;
                        }
                        let now = std::time::Instant::now();
                        let repeat = last_press.is_some_and(|(at, r, c)| (r, c) == (ev.row, ev.col) && now.duration_since(at) < DOUBLE_CLICK_WINDOW);
                        // A fourth press starts over, so repeated clicking
                        // cycles cursor, word, line.
                        click_streak = if repeat { click_streak % 3 + 1 } else { 1 };
                        last_press = Some((now, ev.row, ev.col));
                        ranged_selection = false;
                        if place_nav_cursor_at(&mut buf, rect, row0, col0) {
                            buf.selections_mut().clear();
                            vk.end_visual(buf.cursor());
                            match click_streak {
                                // A double click selects the word, the same
                                // `iw` that `viw` gives, so the two agree
                                // on where a word ends.
                                2 => match motion::text_object_range(&buf, motion::TextObjectKind::Word, false, None) {
                                    Some(range) => {
                                        vk.begin_visual(RegisterShape::Char, range.from);
                                        buf.set_cursor(range.to.0, range.to.1);
                                        ranged_selection = true;
                                    }
                                    // No word under the click (blank space):
                                    // an ordinary caret.
                                    None => vk.begin_visual(RegisterShape::Char, buf.cursor()),
                                },
                                // A triple click selects the line linewise,
                                // as `V` does, so a yank puts it back as
                                // its own line.
                                3 => {
                                    vk.begin_visual(RegisterShape::Line, buf.cursor());
                                    ranged_selection = true;
                                }
                                _ => vk.begin_visual(RegisterShape::Char, buf.cursor()),
                            }
                        }
                    }
                    _ => {}
                }
            } else if ev.is_left_drag() {
                // Dragging just moves the cursor -- the Visual selection
                // armed by the press is anchored where it started, so it
                // grows and shrinks on its own with no separate
                // selection bookkeeping here at all.
                let row0 = (ev.row as usize).saturating_sub(1);
                let col0 = (ev.col as usize).saturating_sub(1);
                if vk.is_visual() {
                    place_nav_cursor_at(&mut buf, rect, row0, col0);
                    let content_cols = nav_content_cols(&buf, rect);
                    nav_scroll_to_show_cursor(&mut buf, content_cols);
                }
            } else if ev.is_release() {
                // A press with no drag after it: the anchor is still
                // exactly where the cursor is, so there's nothing
                // selected and the armed Visual mode should just go
                // away. A real drag leaves its selection standing, ready
                // for `y`/`d`/`c` like any other.
                if !ranged_selection
                    && let Some((_, anchor)) = vk.visual_anchor()
                    && anchor == buf.cursor()
                {
                    vk.end_visual(buf.cursor());
                }
            } else if ev.is_scroll_down() || ev.is_scroll_up() {
                // A cursor motion, with the view following -- see
                // fileeditor::MOUSE_WHEEL_LINES for the measurement that
                // says why this is not a viewport scroll.
                let motion = if ev.is_scroll_down() { motion::Motion::Down } else { motion::Motion::Up };
                editor::apply_motion_or_reselect(&mut vk, &mut buf, motion, Some(fileeditor::MOUSE_WHEEL_LINES));
                let content_cols = nav_content_cols(&buf, rect);
                nav_scroll_to_show_cursor(&mut buf, content_cols);
            } else if ev.is_scroll_left() || ev.is_scroll_right() {
                let columns = fileeditor::MOUSE_WHEEL_COLUMNS as isize;
                let content_cols = nav_content_cols(&buf, rect);
                // Deliberately *not* followed by scroll_to_show_cursor,
                // which pulls the view back to the cursor -- the exact
                // opposite of what a scroll is asking for. The cursor
                // comes along instead; see fileeditor::scroll_horizontally.
                nav_scroll_horizontally(&mut buf, if ev.is_scroll_right() { columns } else { -columns }, content_cols);
            }
            render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            continue 'nav;
        }

        // Every mouse event has been dealt with above, so this is a
        // keystroke -- and typing puts a hover popup away. Redrawn
        // here rather than left to the arms below, since not all of
        // them repaint and a popup nobody removes stays on screen over
        // whatever comes next.
        if std::mem::take(&mut hover_shown) {
            hover_at = None;
            render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
        }

        // `n`/`N` step through the definitions `gd` just found, and
        // any other key ends that. Scoped rather than bound outright
        // because both keys already mean something (repeat-search
        // forward and backward), and a `gd` that permanently took them
        // over would be a worse trade than the cycling is worth.
        if definitions.is_some() {
            let cycling = vk.is_idle() && matches!(key, Key::Char('n') | Key::Char('N'));
            if !cycling {
                definitions = None;
            } else {
                let (kind, locations, at) = definitions.take().expect("checked just above");
                let step = if key == Key::Char('n') { 1 } else { locations.len() - 1 };
                let next = (at + step) % locations.len();
                match goto_location(&app.sessions, session_id, &mut buf, &locations[next], kind.noun()) {
                    Goto::Moved => {
                        let content_cols = nav_content_cols(&buf, rect);
                        nav_scroll_to_show_cursor(&mut buf, content_cols);
                        render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                        show_command_mode_error(&format!("{} {}/{}", kind.noun(), next + 1, locations.len()), app.term_rows, app.term_cols);
                        definitions = Some((kind, locations, next));
                    }
                    // Stepping onto a definition in another file opens it,
                    // ending this loop and the cycle, whose list belongs to
                    // this buffer.
                    Goto::Elsewhere { path, line, character, encoding } => {
                        break 'nav (
                            NavExit::OpenAt {
                                path,
                                line,
                                character,
                                encoding,
                                // Inherited: the opened file may be a
                                // dependency's source, whose nearest
                                // project marker belongs to something else.
                                root: buf.as_editable().and_then(|tb| server_target(&app.sessions[&session_id], tb)).map(|t| t.root),
                            },
                            nav_buffer_into_edit_state(buf, vk),
                        );
                    }
                    Goto::Nowhere(message) => {
                        render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                        show_command_mode_error(&message, app.term_rows, app.term_cols);
                        definitions = Some((kind, locations, at));
                    }
                }
                continue;
            }
        }

        match key {
            // Visual-mode keys, handled here rather than in `vk.feed`
            // because the committed `selections` belong to `NavBuffer` (see
            // vimkeys.rs's module doc). `vk.is_idle()` guards them, so
            // after a prefix (`f`, a count) they keep their ordinary
            // meaning.
            //
            // `Z`, `y`, Escape and Ctrl-C work on either buffer; Ctrl-C
            // cancels only in a file, where it's an alias for Escape (see
            // `fileeditor::run_insert_mode`). `d`, `c`, `p` and `S` change
            // text, so they're gated to writable buffers and otherwise fall
            // through to `vk.feed`.
            Key::Char('Z') if vk.is_idle() && vk.is_visual() => {
                commit_active_selection(&vk, &mut buf);
                let end_cursor = buf.cursor();
                vk.end_visual(end_cursor);
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                continue;
            }
            Key::Char('y') if vk.is_idle() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let register = vk.take_pending_register();
                let end_cursor = buf.cursor();
                let selections = buf.selections().clone();
                match &mut buf {
                    NavBuffer::ReadOnly(sb) => yank_selections(sb, &selections, &mut app.registers, register),
                    NavBuffer::Editable(tb) => tb.yank_selections(&mut app.registers, register),
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                continue;
            }
            // `x` with a selection is `d`, as in vim.
            Key::Char('d') | Key::Char('x') if vk.is_idle() && buf.is_writable() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let register = vk.take_pending_register();
                let end_cursor = buf.cursor();
                if let Some(tb) = buf.as_writable_mut() {
                    tb.delete_selections(&mut app.registers, register);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                continue;
            }
            // `c`: Insert mode types into every deleted selection's gap at
            // once (`run_insert_mode`'s `extra_cursors`).
            Key::Char('c') if vk.is_idle() && buf.is_writable() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let register = vk.take_pending_register();
                let end_cursor = buf.cursor();
                let mut gaps: Vec<(usize, usize)> = Vec::new();
                if let Some(tb) = buf.as_writable_mut() {
                    gaps = tb.delete_selections(&mut app.registers, register);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                if !gaps.is_empty()
                    && let Some(tb) = buf.as_writable_mut()
                {
                    let (insert_term_rows, insert_term_cols) = (app.term_rows, app.term_cols);
                    // Taken on each entry into Insert mode: `:abbr` runs
                    // from the colon line, and a copy from the top would
                    // miss one just defined.
                    let insert_abbrs = app.sessions[&session_id].shell.abbrs.clone();
                    app.with_registers(|app, registers| {
                        fileeditor::run_insert_mode(
                            tb,
                            &mut vk,
                            rect,
                            registers,
                            &mut EditorServices { app, session_id },
                            false,
                            insert_term_rows,
                            insert_term_cols,
                            color_overrides.as_ref(),
                            &gaps[1..],
                            &insert_abbrs,
                        )
                    })?;
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                continue;
            }
            Key::Char('p') | Key::Char('P') if vk.is_idle() && buf.is_writable() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let register = vk.take_pending_register();
                let end_cursor = buf.cursor();
                if let Some(tb) = buf.as_writable_mut() {
                    tb.put_over_selections(&mut app.registers, register);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                continue;
            }
            // Visual `>`/`<`: shift every selection linewise, then back to
            // Normal mode at the first shifted line, as in vim.
            Key::Char('>') if vk.is_idle() && buf.is_writable() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let end_cursor = buf.cursor();
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::indent_selections(tb);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
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
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                continue;
            }
            Key::Char('S') if vk.is_idle() && buf.is_writable() && (vk.is_visual() || !buf.selections().is_empty()) => {
                commit_active_selection(&vk, &mut buf);
                let end_cursor = buf.cursor();
                if let Some(tb) = buf.as_writable_mut()
                    && let Some(Key::Char(ch)) = vk.next_key(|| {
                        editor::read_key_idle(&mut || {
                            service_background_jobs(app);
                        })
                    })?
                {
                    fileeditor::surround_selections(tb, ch);
                }
                buf.selections_mut().clear();
                vk.end_visual(end_cursor);
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                continue;
            }
            Key::Escape | Key::CtrlC
                if vk.is_idle()
                    && (key == Key::Escape || matches!(buf, NavBuffer::Editable(_)))
                    && (vk.is_visual() || !buf.selections().is_empty()) =>
            {
                let end_cursor = buf.cursor();
                vk.end_visual(end_cursor);
                buf.selections_mut().clear();
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                continue;
            }
            // `q`/`@`, macros. Handled here because replaying has to
            // re-enter this match for each key (see `VimKeys::next_key`).
            // Not gated to files: a navigate-and-yank macro over scrollback
            // is as useful. An `i` from `ReadOnly` leaves the loop and
            // drops a recording in progress, with the rest of `vk`.
            //
            // Bare `q` while recording stops it; checked before the arm
            // that starts one.
            Key::Char('q') if vk.is_idle() && vk.is_recording().is_some() => {
                vk.stop_recording();
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                continue;
            }
            // `q{reg}` starts recording. The register name is read through
            // `vk.next_key`, so a macro containing `q{reg}` replays
            // correctly.
            Key::Char('q') if vk.is_idle() => {
                if let Some(Key::Char(ch)) = vk.next_key(|| {
                    editor::read_key_idle(&mut || {
                        service_background_jobs(app);
                    })
                })? && ch.is_ascii_alphabetic()
                {
                    vk.start_recording(ch);
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                continue;
            }
            // `@{reg}`/`@@` replays. A count in front is a repeat count
            // (`3@a`), hence `is_idle_except_count` and `take_count`.
            Key::Char('@') if vk.is_idle_except_count() => {
                let count = vk.take_count().unwrap_or(1).max(1);
                if let Some(Key::Char(ch)) = vk.next_key(|| {
                    editor::read_key_idle(&mut || {
                        service_background_jobs(app);
                    })
                })? && (ch == '@' || ch.is_ascii_lowercase())
                {
                    vk.queue_macro_replay(ch, count);
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                continue;
            }
            // Needs `is_idle()` like every arm ahead of `feed()`: otherwise
            // a `Z` typed while something is pending (`rZ`) is stolen, and
            // the key read after it is lost too. A count before `ZZ` is
            // discarded, as before `S` and `q`.
            Key::Char('Z') if vk.is_idle() => {
                let k2 = vk.next_key(|| {
                    editor::read_key_idle(&mut || {
                        service_background_jobs(app);
                    })
                })?;
                if k2 != Some(Key::Char('Z')) {
                    continue;
                }
                if matches!(buf, NavBuffer::Editable(_)) {
                    // `ZZ`: `:x`, save and quit.
                    let mut saved = true;
                    if let Some(tb) = buf.as_writable_mut() {
                        match tb.save(None) {
                            Ok(()) => fileeditor::set_last_filename(tb, &mut app.registers),
                            Err(e) => {
                                app.sessions[&session_id].shell.sink_err(&format!("bish: E212: Can't open file for writing: {e}\n"));
                                saved = false;
                            }
                        }
                    }
                    if saved {
                        break 'nav (NavExit::Quit, nav_buffer_into_edit_state(buf, vk));
                    }
                    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                    continue;
                }
                break 'nav (NavExit::Resume(initial_text.clone(), initial_cursor), nav_buffer_into_edit_state(buf, vk));
            }
            // `K`: hover what's under the cursor (`docs::hover_lines_at`).
            // With a `:dbg` session attached to this frame, the
            // identifier's current value is shown too
            // (`Shell::debug_peek_var`). Only in a file; `ReadOnly` falls
            // through to vimkeys. The doc index is built from the buffer's
            // live text, so unsaved edits count.
            Key::Char('K') if vk.is_idle() && matches!(buf, NavBuffer::Editable(_)) => {
                let NavBuffer::Editable(tb) = &buf else { unreachable!("guarded by this arm's own match above") };
                let (row, col) = tb.cursor();
                let chars = tb.line_chars(row);
                let hover_lines = match hover_lines_at(app, session_id, edit_frame_id, tb, row, col) {
                    // A keypress deserves an answer even with nothing under
                    // the cursor (see `NOTHING_UNDER_THE_CURSOR`); an
                    // unasked hover says nothing.
                    lines if lines.is_empty() => vec![docs::NOTHING_UNDER_THE_CURSOR.to_string()],
                    lines => lines,
                };
                let gutter_width = rect.cols.saturating_sub(fileeditor::editor_content_cols(tb, rect));
                let cursor_row = rect.row + row.saturating_sub(tb.viewport_top());
                let cursor_display_col = col_of(&chars, col);
                let cursor_col = rect.col + gutter_width + cursor_display_col.saturating_sub(tb.viewport_left());
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                print!("{}", fileeditor::render_hover_popup(&hover_lines, cursor_row, cursor_col, rect));
                let _ = io::stdout().flush();
                continue;
            }
            // `:` opens command mode, which places its own prompt row.
            // `buf.as_editable_mut()` is what makes `w`, `wq`, `x`, `q` and
            // `q!` mean something there (see `run_command_mode`'s
            // `editing`).
            //
            // Gated like the arms ahead of `feed()`, or `r:` and `f:` lose
            // their `:`. `is_idle_except_count`, because a count before `:`
            // is dropped, not refused.
            Key::Char(':') if vk.is_idle_except_count() => {
                // Read first: the call borrows `buf` mutably and reads `vk`.
                let last_search = vk.last_search_text().to_string();
                let outcome = handle_command_mode(
                    app,
                    session_id,
                    buf.as_editable_mut(),
                    None,
                    // The rest of a mapping's right-hand side when a mapped
                    // `:` opened this line. `read_line` reads stdin and
                    // never sees the queue, so `<Esc>:w<CR>` would
                    // otherwise fire `:` and drop `w<CR>`.
                    vk.take_replay_queue(),
                    &last_search,
                );
                // `:diag` may have put a pane below this one; `Ran`'s
                // redraw below needs the rect now, not at the next
                // iteration.
                rect = pane_rect(&app.windows[app.current_window], app.windows[app.current_window].focused_pane, app.term_rows, app.term_cols);
                // Likewise anything `:bishopt` just changed has to apply
                // before that redraw.
                color_overrides = Some(syntax_color_overrides(&app.sessions[&session_id].shell));
                vk.set_mappings(app.sessions[&session_id].shell.mappings.clone());
                if let Some(tb) = buf.as_editable_mut() {
                    apply_view_options(&app.sessions[&session_id].shell, tb);
                    fileeditor::scroll_to_show_cursor(tb, fileeditor::editor_content_cols(tb, rect));
                }
                match outcome {
                    // A cancelled `:` returns to Normal mode, as in vim.
                    CommandModeOutcome::Cancelled => {
                        render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                        continue;
                    }
                    // `:q`/`:q!`. In a file, `:q`, `:q!`, `:wq` and `:x`
                    // quit it; over scrollback, like `ZZ` and `i`, it
                    // restores the original line.
                    CommandModeOutcome::Quit => {
                        if matches!(buf, NavBuffer::Editable(_)) {
                            break 'nav (NavExit::Quit, nav_buffer_into_edit_state(buf, vk));
                        }
                        break 'nav (NavExit::Resume(initial_text.clone(), initial_cursor), nav_buffer_into_edit_state(buf, vk));
                    }
                    // Focus may have moved (`apply_window_action` did
                    // that); the original line isn't carried over.
                    CommandModeOutcome::Action(_) => {
                        return Ok((NavExit::Detached, nav_buffer_into_edit_state(buf, vk)));
                    }
                    // `:normal {keys}`: queued, so they reach this loop's
                    // reads as typed keys. `:normal` takes literal keys;
                    // there's no `<Esc>` notation.
                    CommandModeOutcome::RunNormal(keys) => {
                        vk.queue_keys(keys.chars().map(Key::Char));
                        render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                        continue;
                    }
                    // `:s/.../.../c`, driven here because each question
                    // needs the pane drawn under it first.
                    CommandModeOutcome::ConfirmSubstitute(cmd) => {
                        let done = confirm_substitute(&mut buf, &vk, &cmd, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                        render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                        let message = match done {
                            Ok(out) => match (cmd.print, out.last_row) {
                                (Some(style), Some(row)) => buf.as_editable().map(|tb| printed_line(tb, row, style)).unwrap_or_default(),
                                _ => format!(
                                    "{} substitution{} on {} line{}",
                                    out.substitutions,
                                    if out.substitutions == 1 { "" } else { "s" },
                                    out.lines,
                                    if out.lines == 1 { "" } else { "s" }
                                ),
                            },
                            Err(e) => format!("bish: {e}"),
                        };
                        show_command_mode_error(&message, app.term_rows, app.term_cols);
                        continue;
                    }
                    // Back to Normal mode with the output up as
                    // `PendingView::Output` until the next key.
                    // `render_nav_frame` runs first in either case: the
                    // compositor redraw reads the session grid, which a
                    // file pane never feeds (see
                    // `fileeditor::freeze_editor_frame`), so without it the
                    // overlay sat over a blank pane. The overlay only draws
                    // its own rows at the bottom.
                    CommandModeOutcome::Ran { output, status } => {
                        render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                        if !output.is_empty() || status != 0 {
                            render_command_output_overlay(&output, status, app.term_rows, app.term_cols);
                            pending_view = PendingView::Output;
                        }
                        continue;
                    }
                    CommandModeOutcome::NoHighlight => {
                        vk.suppress_search_highlight();
                        render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                        continue;
                    }
                    // `:sym QUERY`: the project-wide `gO`, into the same
                    // pane. A workspace symbol already names its file.
                    CommandModeOutcome::Symbols(query) => {
                        if let NavBuffer::Editable(tb) = &buf {
                            let encoding = server_encoding_for(&app.sessions, session_id, tb).unwrap_or(crate::lsp::PositionEncoding::Utf16);
                            let found = ask_server(
                                app,
                                session_id,
                                tb,
                                "workspace/symbol",
                                "workspaceSymbolProvider",
                                vec![("query".to_string(), crate::json::Value::Str(query.clone()))],
                            )
                            .map(|result| crate::lsp::symbols(&result, ""))
                            .unwrap_or_default();
                            let named = if query.is_empty() { String::new() } else { format!(" matching '{query}'") };
                            let title = match found.len() {
                                0 => format!("No symbols{named}"),
                                1 => format!("1 symbol{named}"),
                                n if n > MAX_LOCATIONS => format!("{MAX_LOCATIONS} of {n} symbols{named}"),
                                n => format!("{n} symbols{named}"),
                            };
                            // Relative to the file asked from: a
                            // project-wide answer is mostly other files.
                            let base = tb.path().and_then(|p| p.parent()).map(std::path::Path::to_path_buf);
                            let list = workspace_symbol_list(&title, &found, encoding, base.as_deref());
                            match edit_frame_id {
                                Some(id) => {
                                    sync_locations_pane(app, id, list);
                                    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                                }
                                None => {
                                    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                                    show_command_mode_error(&list.title, app.term_rows, app.term_cols);
                                }
                            }
                        } else {
                            render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                            show_command_mode_error("bish: sym: no file here to ask a language server about", app.term_rows, app.term_cols);
                        }
                        continue;
                    }
                }
            }
            _ => {}
        }

        match vk.feed(key) {
            // `gd`: go to where this is defined. Several answers arm
            // `n`/`N` to step through them. Only in a file, like `K`.
            KeyOutcome::GotoDefinition(kind) => {
                if let NavBuffer::Editable(tb) = &buf {
                    let (row, col) = tb.cursor();
                    let (method, capability) = kind.request();
                    let found = ask_server_at_cursor(app, session_id, tb, row, col, CursorRequest { method, capability, extra: &[] })
                        .map(|result| crate::lsp::locations(&result))
                        .unwrap_or_default();
                    match found.first().cloned() {
                        None => {
                            // No server answer, but in a shell script a
                            // command word has a definition: its manpage,
                            // in bish's own pager (`man rsync` without
                            // leaving the buffer). Only for `gd`: a manpage
                            // isn't the command's type (`gy`) or
                            // declaration.
                            let manpage = (kind == GotoKind::Definition).then(|| man_page_under_cursor(tb)).flatten();
                            match manpage {
                                Some((name, source)) => {
                                    run_pager(
                                        app,
                                        &format!("man {name}"),
                                        PagerSource {
                                            language: "roff".to_string(),
                                            source,
                                            render: RenderOptions {
                                                hyperlinks: app.sessions.get(&session_id).is_none_or(|s| s.shell.bishopt_bool("hyperlinks")),
                                                base_dir: None,
                                                colors: app.sessions.get(&session_id).map(|s| ui_colors(&s.shell)),
                                                wrap_tables: app.sessions.get(&session_id).is_none_or(|s| s.shell.bishopt_bool("table_wrap")),
                                            },
                                        },
                                    );
                                    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                                }
                                None => {
                                    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                                    show_command_mode_error(&format!("no {} found", kind.noun()), app.term_rows, app.term_cols);
                                }
                            }
                        }
                        Some(first) => {
                            // Recorded before moving, so `Ctrl-O` returns
                            // to where `gd` was asked.
                            record_jump(&mut app.windows, app.current_window, &buf);
                            match goto_location(&app.sessions, session_id, &mut buf, &first, kind.noun()) {
                                Goto::Moved => {
                                    let content_cols = nav_content_cols(&buf, rect);
                                    nav_scroll_to_show_cursor(&mut buf, content_cols);
                                    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                                    // Said, and armed, only when there's
                                    // somewhere else to go.
                                    if found.len() > 1 {
                                        show_command_mode_error(
                                            &format!("{} 1/{} (n/N to cycle)", kind.noun(), found.len()),
                                            app.term_rows,
                                            app.term_cols,
                                        );
                                        definitions = Some((kind, found, 0));
                                    }
                                }
                                Goto::Elsewhere { path, line, character, encoding } => {
                                    break 'nav (
                                        NavExit::OpenAt {
                                            path,
                                            line,
                                            character,
                                            encoding,
                                            // Inherited: the opened file may be a
                                            // dependency's source, whose nearest
                                            // project marker belongs to something
                                            // else.
                                            root: buf.as_editable().and_then(|tb| server_target(&app.sessions[&session_id], tb)).map(|t| t.root),
                                        },
                                        nav_buffer_into_edit_state(buf, vk),
                                    );
                                }
                                Goto::Nowhere(message) => {
                                    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                                    show_command_mode_error(&message, app.term_rows, app.term_cols);
                                }
                            }
                        }
                    }
                }
            }
            // `gr`: everywhere this is used. No single answer, so it fills
            // the location-list pane below the editor and, like `:diag`,
            // leaves focus where it was.
            KeyOutcome::GotoReferences => {
                if let NavBuffer::Editable(tb) = &buf {
                    let (row, col) = tb.cursor();
                    let encoding = server_encoding_for(&app.sessions, session_id, tb).unwrap_or(crate::lsp::PositionEncoding::Utf16);
                    let found = ask_server_at_cursor(
                        app,
                        session_id,
                        tb,
                        row,
                        col,
                        CursorRequest {
                            method: "textDocument/references",
                            capability: "referencesProvider",
                            // "Where is this used" includes the declaration.
                            extra: &[(
                                "context",
                                crate::json::Value::Object(vec![("includeDeclaration".to_string(), crate::json::Value::Bool(true))]),
                            )],
                        },
                    )
                    .map(|result| crate::lsp::locations(&result))
                    .unwrap_or_default();
                    // Relative to the project root when there is one.
                    let base = server_target(&app.sessions[&session_id], tb).map(|t| t.root);
                    let title = match found.len() {
                        0 => "No references found".to_string(),
                        1 => "1 reference".to_string(),
                        n if n > MAX_LOCATIONS => format!("{MAX_LOCATIONS} of {n} references"),
                        n => format!("{n} references"),
                    };
                    let list = location_list(&title, &found, encoding, base.as_deref());
                    match edit_frame_id {
                        Some(id) => {
                            sync_locations_pane(app, id, list);
                            render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                        }
                        // No edit frame to hang a pane on: every start
                        // except `Edit`.
                        None => {
                            render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                            show_command_mode_error(&list.title, app.term_rows, app.term_cols);
                        }
                    }
                }
            }
            // `gO`: an outline of this file, in the same pane as `gr`,
            // without moving focus.
            KeyOutcome::DocumentSymbols => {
                if let NavBuffer::Editable(tb) = &buf {
                    let (row, col) = tb.cursor();
                    let encoding = server_encoding_for(&app.sessions, session_id, tb).unwrap_or(crate::lsp::PositionEncoding::Utf16);
                    let uri = server_target(&app.sessions[&session_id], tb).map(|t| t.uri).unwrap_or_default();
                    let found = ask_server_at_cursor(
                        app,
                        session_id,
                        tb,
                        row,
                        col,
                        CursorRequest { method: "textDocument/documentSymbol", capability: "documentSymbolProvider", extra: &[] },
                    )
                    .map(|result| crate::lsp::symbols(&result, &uri))
                    .unwrap_or_default();
                    let title = match found.len() {
                        0 => "No symbols found".to_string(),
                        1 => "1 symbol".to_string(),
                        n if n > MAX_LOCATIONS => format!("{MAX_LOCATIONS} of {n} symbols"),
                        n => format!("{n} symbols"),
                    };
                    let list = symbol_list(&title, &found, encoding);
                    match edit_frame_id {
                        Some(id) => {
                            sync_locations_pane(app, id, list);
                            render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                        }
                        None => {
                            render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                            show_command_mode_error(&list.title, app.term_rows, app.term_cols);
                        }
                    }
                }
            }
            // `ga`: pick an action for the server to do here. The buffer is
            // passed mutably, as applying an action edits it (and maybe
            // other files).
            KeyOutcome::CodeActions => {
                let message = match buf.as_editable_mut() {
                    Some(tb) => code_actions_at_cursor(app, session_id, tb, rect),
                    None => None,
                };
                let content_cols = nav_content_cols(&buf, rect);
                nav_scroll_to_show_cursor(&mut buf, content_cols);
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                if let Some(message) = message {
                    show_command_mode_error(&message, app.term_rows, app.term_cols);
                }
            }
            KeyOutcome::Fold(cmd, count) => {
                use crate::bishedit::vimkeys::FoldCmd;
                let n = count.unwrap_or(1).max(1);
                let line = buf.cursor().0;
                let selection = if cmd == FoldCmd::CreateFromSelection { active_visual_range(&vk, &buf) } else { None };
                let lines = motion::line_span(&buf, n);
                if let Some(folds) = buf.folds_mut() {
                    match cmd {
                        FoldCmd::Open => folds.open(line, n),
                        FoldCmd::OpenAll | FoldCmd::View => folds.open_all(line),
                        FoldCmd::Close => folds.close(line, n),
                        FoldCmd::CloseAll => folds.close_all(line),
                        FoldCmd::Toggle => folds.toggle(line, n),
                        FoldCmd::ToggleAll => folds.toggle_all(line),
                        FoldCmd::OpenEverything => folds.set_all(false),
                        FoldCmd::CloseEverything => folds.set_all(true),
                        FoldCmd::Delete => folds.delete(line),
                        FoldCmd::DeleteAll => folds.delete_all(line),
                        FoldCmd::Eliminate => folds.clear(),
                        FoldCmd::ToggleEnabled => folds.enabled = !folds.enabled,
                        FoldCmd::CreateLines => folds.create(lines.0, lines.1),
                        FoldCmd::CreateFromSelection => {
                            if let Some(range) = selection {
                                folds.create(range.from.0, range.to.0);
                            }
                        }
                    }
                }
                if cmd == FoldCmd::CreateFromSelection {
                    let at = buf.cursor();
                    vk.end_visual(at);
                }
                rest_on_fold(&mut buf);
                let content_cols = nav_content_cols(&buf, rect);
                nav_scroll_to_show_cursor(&mut buf, content_cols);
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::Motion(m, count) => {
                // `apply_motion_or_reselect` records this in `vk` too,
                // which the editor no longer reads (see `JumpList`); only
                // here is the file known.
                if motion::is_jump(&m) {
                    record_jump(&mut app.windows, app.current_window, &buf);
                }
                editor::apply_motion_or_reselect(&mut vk, &mut buf, m, count);
                let content_cols = nav_content_cols(&buf, rect);
                nav_scroll_to_show_cursor(&mut buf, content_cols);
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            // `I`/`A` with a block selection insert the same text at the
            // same column on every row, deleting nothing (commenting a run
            // of lines, adding a comma to each). `I` goes to the left edge,
            // `A` one past the right; short rows are padded so the edit
            // stays a column.
            KeyOutcome::EnterInsert(cmd @ (crate::bishedit::vimkeys::InsertCmd::LineStart | crate::bishedit::vimkeys::InsertCmd::LineEnd))
                if vk.visual_anchor().map(|(shape, _)| shape) == Some(RegisterShape::Block) =>
            {
                let range = active_visual_range(&vk, &buf);
                let end_cursor = buf.cursor();
                vk.end_visual(end_cursor);
                buf.selections_mut().clear();
                if let (Some(range), Some(tb)) = (range, buf.as_writable_mut())
                    && let Some((left, right)) = motion::block_columns(&range)
                {
                    let col = if cmd == crate::bishedit::vimkeys::InsertCmd::LineStart { left } else { right + 1 };
                    let mut cursors: Vec<(usize, usize)> = Vec::new();
                    for line in range.from.0..=range.to.0.min(tb.line_count().saturating_sub(1)) {
                        let len = tb.line_len(line);
                        if len < col {
                            tb.insert_text((line, len), &" ".repeat(col - len));
                        }
                        cursors.push((line, col));
                    }
                    if let Some(&first) = cursors.first() {
                        tb.set_cursor(first.0, first.1);
                        let extra: Vec<(usize, usize)> = cursors[1..].to_vec();
                        let (insert_term_rows, insert_term_cols) = (app.term_rows, app.term_cols);
                        let insert_abbrs = app.sessions[&session_id].shell.abbrs.clone();
                        app.with_registers(|app, registers| {
                            fileeditor::run_insert_mode(
                                tb,
                                &mut vk,
                                rect,
                                registers,
                                &mut EditorServices { app, session_id },
                                false,
                                insert_term_rows,
                                insert_term_cols,
                                color_overrides.as_ref(),
                                &extra,
                                &insert_abbrs,
                            )
                        })?;
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            // Over scrollback, Insert hands typing back to the prompt
            // (`apply_insert_cmd`, `NavExit::Resume`): Ctrl+G is an
            // excursion. In a file, Normal mode is the resting state and
            // Insert mode a nested loop (`fileeditor::run_insert_mode`)
            // that returns here.
            KeyOutcome::EnterInsert(cmd) => {
                if matches!(buf, NavBuffer::Editable(_)) {
                    if let Some(tb) = buf.as_writable_mut() {
                        fileeditor::resolve_insert_start(tb, cmd);
                        let (insert_term_rows, insert_term_cols) = (app.term_rows, app.term_cols);
                        let insert_abbrs = app.sessions[&session_id].shell.abbrs.clone();
                        app.with_registers(|app, registers| {
                            fileeditor::run_insert_mode(
                                tb,
                                &mut vk,
                                rect,
                                registers,
                                &mut EditorServices { app, session_id },
                                false,
                                insert_term_rows,
                                insert_term_cols,
                                color_overrides.as_ref(),
                                &[],
                                &insert_abbrs,
                            )
                        })?;
                    }
                    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                } else {
                    let (new_chars, new_cursor) = crate::bishedit::vimkeys::apply_insert_cmd(&original_chars, initial_cursor, cmd);
                    break 'nav (NavExit::Resume(new_chars.into_iter().collect(), new_cursor), nav_buffer_into_edit_state(buf, vk));
                }
            }
            // `R`: over scrollback, a plain insert at the cursor, as in
            // editor.rs; overtyping would have to live in the prompt's own
            // loop. A file gets real Replace mode (`run_insert_mode` with
            // `replace: true`).
            KeyOutcome::EnterReplace => {
                if matches!(buf, NavBuffer::Editable(_)) {
                    if let Some(tb) = buf.as_writable_mut() {
                        let (insert_term_rows, insert_term_cols) = (app.term_rows, app.term_cols);
                        let insert_abbrs = app.sessions[&session_id].shell.abbrs.clone();
                        app.with_registers(|app, registers| {
                            fileeditor::run_insert_mode(
                                tb,
                                &mut vk,
                                rect,
                                registers,
                                &mut EditorServices { app, session_id },
                                true,
                                insert_term_rows,
                                insert_term_cols,
                                color_overrides.as_ref(),
                                &[],
                                &insert_abbrs,
                            )
                        })?;
                    }
                    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                } else {
                    let (new_chars, new_cursor) =
                        crate::bishedit::vimkeys::apply_insert_cmd(&original_chars, initial_cursor, crate::bishedit::vimkeys::InsertCmd::Before);
                    break 'nav (NavExit::Resume(new_chars.into_iter().collect(), new_cursor), nav_buffer_into_edit_state(buf, vk));
                }
            }
            // `v`/`V` arm Visual mode anchored at the buffer's cursor,
            // which vimkeys can't read (see `EnterVisual`). The guarded
            // arms above handle what follows.
            KeyOutcome::EnterVisual(shape) => {
                vk.begin_visual(shape, buf.cursor());
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::ReselectVisual => {
                if let Some((shape, anchor, cursor)) = vk.last_visual() {
                    buf.set_cursor(cursor.0, cursor.1);
                    vk.begin_visual(shape, anchor);
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::SwapVisualEnds => {
                if let Some((_, anchor)) = vk.visual_anchor() {
                    let cursor = buf.cursor();
                    buf.set_cursor(anchor.0, anchor.1);
                    vk.set_visual_anchor(cursor);
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            // `Ctrl-O`/`Ctrl-I`, across files as in vim: the list lives on
            // the pane (see `JumpList`), so a `gd` into another file is
            // just another entry. A target in this buffer moves the cursor;
            // one elsewhere leaves with `NavExit::OpenAt`, which brings
            // that file's frame forward or opens it. Nothing is closed, so
            // `Ctrl-I` can come back.
            KeyOutcome::Jump { forward } => {
                let here = jump_here(&buf);
                let target = match (here.clone(), buf.as_editable().is_some()) {
                    (Some(here), true) => {
                        let pane_id = app.windows[app.current_window].focused_pane;
                        let jumps = &mut app.windows[app.current_window].pane_mut(pane_id).jumps;
                        if forward { jumps.forward(here) } else { jumps.back(here) }
                    }
                    // No file behind this view: the position-only list in
                    // `VimKeys`.
                    _ => {
                        let current = buf.cursor();
                        let target = if forward { vk.jump_forward(current) } else { vk.jump_back(current) };
                        target.map(|(row, col)| JumpEntry { path: PathBuf::new(), row, col })
                    }
                };
                if let Some(target) = target {
                    let same_file = here.as_ref().is_none_or(|h| h.path == target.path);
                    if same_file {
                        let row = target.row.min(buf.line_count().saturating_sub(1));
                        let col = target.col.min(buf.line_len(row));
                        buf.set_cursor(row, col);
                        let content_cols = nav_content_cols(&buf, rect);
                        nav_scroll_to_show_cursor(&mut buf, content_cols);
                    } else {
                        // `Utf32` is bish's own column counting, so these
                        // coordinates pass through `OpenAt`'s conversion
                        // unchanged.
                        break 'nav (
                            NavExit::OpenAt {
                                path: target.path,
                                line: target.row,
                                character: target.col,
                                encoding: crate::lsp::PositionEncoding::Utf32,
                                root: buf.as_editable().and_then(|tb| server_target(&app.sessions[&session_id], tb)).map(|t| t.root),
                            },
                            nav_buffer_into_edit_state(buf, vk),
                        );
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            // `u`/`Ctrl-R`, in a writable file only. Not with a selection:
            // vim's Visual `u`/`U` change case, which isn't implemented
            // here, so `u` does nothing rather than undo.
            KeyOutcome::Undo(count) => {
                if !vk.is_visual()
                    && buf.selections().is_empty()
                    && let Some(tb) = buf.as_writable_mut()
                {
                    for _ in 0..count.unwrap_or(1).max(1) {
                        if !tb.undo() {
                            break;
                        }
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::Redo(count) => {
                if !vk.is_visual()
                    && buf.selections().is_empty()
                    && let Some(tb) = buf.as_writable_mut()
                {
                    for _ in 0..count.unwrap_or(1).max(1) {
                        if !tb.redo() {
                            break;
                        }
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            // `g-`/`g+`: guarded like `u`.
            KeyOutcome::UndoSeq { forward, count } => {
                if !vk.is_visual()
                    && buf.selections().is_empty()
                    && let Some(tb) = buf.as_writable_mut()
                {
                    for _ in 0..count.unwrap_or(1).max(1) {
                        if !tb.time_travel(forward) {
                            break;
                        }
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            // Yanking works on either buffer; copying out of scrollback is
            // what that view is for. The other operators change text, so
            // they need a writable file.
            KeyOutcome::Operator(op, motion, count, register) => {
                if op == Op::Yank {
                    editor::yank_motion(&mut buf, &mut app.registers, motion, count, register);
                } else if op == Op::Fold {
                    // Before the writable check: a fold changes no text.
                    if let Some(range) = motion::motion_range(&mut buf, motion, count)
                        && let Some(folds) = buf.folds_mut()
                    {
                        folds.create(range.from.0, range.to.0);
                    }
                    rest_on_fold(&mut buf);
                } else if let Some(tb) = buf.as_writable_mut() {
                    match op {
                        Op::Delete => {
                            fileeditor::delete_motion(tb, &mut app.registers, motion, count, register);
                        }
                        Op::Change => {
                            let m = fileeditor::redirect_cw_to_ce(tb, &motion);
                            if fileeditor::delete_motion(tb, &mut app.registers, m, count, register) {
                                let (insert_term_rows, insert_term_cols) = (app.term_rows, app.term_cols);
                                let insert_abbrs = app.sessions[&session_id].shell.abbrs.clone();
                                app.with_registers(|app, registers| {
                                    fileeditor::run_insert_mode(
                                        tb,
                                        &mut vk,
                                        rect,
                                        registers,
                                        &mut EditorServices { app, session_id },
                                        false,
                                        insert_term_rows,
                                        insert_term_cols,
                                        color_overrides.as_ref(),
                                        &[],
                                        &insert_abbrs,
                                    )
                                })?;
                            }
                        }
                        Op::Lowercase | Op::Uppercase | Op::CaseToggle => {
                            fileeditor::case_operator_motion(tb, motion, count, fileeditor::case_kind_for_op(op));
                        }
                        Op::Indent => fileeditor::indent_operator_motion(tb, motion, count),
                        Op::Outdent => fileeditor::outdent_operator_motion(tb, motion, count),
                        Op::Yank | Op::Fold => unreachable!("handled above"),
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::OperatorLines(op, count, register) => {
                if op == Op::Yank {
                    editor::yank_lines(&buf, &mut app.registers, count, register);
                } else if op == Op::Fold {
                    let (first, last) = motion::line_span(&buf, count.unwrap_or(1));
                    if let Some(folds) = buf.folds_mut() {
                        folds.create(first, last);
                    }
                    rest_on_fold(&mut buf);
                } else if let Some(tb) = buf.as_writable_mut() {
                    match op {
                        Op::Delete => fileeditor::delete_lines(tb, &mut app.registers, count, register),
                        Op::Change => {
                            fileeditor::change_lines(tb, &mut app.registers, count, register);
                            let (insert_term_rows, insert_term_cols) = (app.term_rows, app.term_cols);
                            let insert_abbrs = app.sessions[&session_id].shell.abbrs.clone();
                            app.with_registers(|app, registers| {
                                fileeditor::run_insert_mode(
                                    tb,
                                    &mut vk,
                                    rect,
                                    registers,
                                    &mut EditorServices { app, session_id },
                                    false,
                                    insert_term_rows,
                                    insert_term_cols,
                                    color_overrides.as_ref(),
                                    &[],
                                    &insert_abbrs,
                                )
                            })?;
                        }
                        Op::Lowercase | Op::Uppercase | Op::CaseToggle => {
                            fileeditor::case_operator_lines(tb, count, fileeditor::case_kind_for_op(op))
                        }
                        Op::Indent => fileeditor::indent_lines(tb, count),
                        Op::Outdent => fileeditor::outdent_lines(tb, count),
                        Op::Yank | Op::Fold => unreachable!("handled above"),
                    }
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            // These all change text: no-ops over scrollback, a `fileeditor::`
            // helper each in a file.
            KeyOutcome::Put { before, count, register } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::put(tb, &mut app.registers, before, count, register);
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::DeleteCharForward { count, register } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::delete_char_forward(tb, &mut app.registers, count, register);
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::Join { count, with_space } => {
                if let Some(tb) = buf.as_writable_mut() {
                    tb.join_lines(count.unwrap_or(1).max(1), with_space);
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::AddSurround { target, ch } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::add_surround(tb, target, ch);
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::DeleteSurround { ch } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::delete_surround(tb, ch);
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::ChangeSurround { ch, replacement } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::change_surround(tb, ch, replacement);
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::ReplaceChar { ch, count } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::replace_char(tb, ch, count.unwrap_or(1).max(1));
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::ToggleCase { count } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::toggle_case(tb, count.unwrap_or(1).max(1));
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::AdjustNumber { delta } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::adjust_number(tb, delta);
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            KeyOutcome::OpenLine { above } => {
                if let Some(tb) = buf.as_writable_mut() {
                    fileeditor::open_line(tb, above);
                    let (insert_term_rows, insert_term_cols) = (app.term_rows, app.term_cols);
                    let insert_abbrs = app.sessions[&session_id].shell.abbrs.clone();
                    app.with_registers(|app, registers| {
                        fileeditor::run_insert_mode(
                            tb,
                            &mut vk,
                            rect,
                            registers,
                            &mut EditorServices { app, session_id },
                            false,
                            insert_term_rows,
                            insert_term_cols,
                            color_overrides.as_ref(),
                            &[],
                            &insert_abbrs,
                        )
                    })?;
                }
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
            // `dispatch_window_cmd` does the work; focus moved, so leave,
            // handing the buffer back for an `Editable` caller to keep.
            KeyOutcome::Window(cmd, count) => {
                dispatch_window_cmd(app, cmd, count);
                return Ok((NavExit::Detached, nav_buffer_into_edit_state(buf, vk)));
            }
            // Ctrl+T. Picking something moves focus or makes a window, so
            // this leaves as `<C-w>` does. Dismissing it changed nothing,
            // so the view is redrawn over where the opener painted.
            KeyOutcome::Opener => match run_opener(app) {
                Some((target, destination)) => {
                    act_on_opener_choice(app, target, destination, None);
                    return Ok((NavExit::Detached, nav_buffer_into_edit_state(buf, vk)));
                }
                None => {
                    compositor_redraw(app);
                    render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
                }
            },
            // Rendered on every key, so the status bar shows a pending
            // count or prefix (`20g` in `20gg`) and a search being typed.
            KeyOutcome::Pending | KeyOutcome::None => {
                render_nav_frame(&mut buf, &vk, rect, app.term_rows, app.term_cols, color_overrides.as_ref());
            }
        }
    };

    // Whatever is next isn't this buffer. Only some paths return here (see
    // `set_hover_tracking`), hence a switch rather than a guard.
    term::set_hover_tracking(false);
    compositor_redraw(app);
    Ok(result)
}

// Visual mode's active (uncommitted) selection: `vk`'s anchor (see
// `visual_anchor`) ordered against the cursor. `Char` becomes
// `Inclusive`, as charwise Visual includes both ends, and `Line`
// becomes `Linewise`.
pub(crate) fn active_visual_range(vk: &VimKeys, buf: &dyn BisheditBuffer) -> Option<motion::MotionRange> {
    let (shape, anchor) = vk.visual_anchor()?;
    let cursor = buf.cursor();
    let motion_shape = match shape {
        RegisterShape::Line => motion::MotionShape::Linewise,
        RegisterShape::Block => motion::MotionShape::Blockwise,
        RegisterShape::Char => motion::MotionShape::Inclusive,
    };
    // Rows in order; columns kept as corners, as a block dragged up and
    // right has them the other way round (see `motion::block_columns`).
    let (from, to) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
    let (from, to) =
        if motion_shape == motion::MotionShape::Blockwise { ((from.0.min(to.0), anchor.1), (from.0.max(to.0), cursor.1)) } else { (from, to) };
    // A selection reaching into a closed fold takes it whole, as an
    // operator's motion does (see `motion::motion_range`).
    let (from, to) = match buf.folds() {
        Some(folds) if motion_shape != motion::MotionShape::Blockwise => {
            let from = folds.closed_at(from.0).map_or(from, |(first, _)| (first, 0));
            let to = folds.closed_at(to.0).map_or(to, |(_, last)| (last, buf.line_len(last).saturating_sub(1)));
            (from, to)
        }
        _ => (from, to),
    };
    Some(motion::MotionRange { shape: motion_shape, from, to })
}

// `y` in Visual mode: the selections, in commit order, joined with no
// separator; a linewise part already ends in `\n`. The register is
// linewise if any part is, as with `"A` appends
// (`RegisterBackend::write`).
fn yank_selections(buf: &dyn BisheditBuffer, selections: &[motion::MotionRange], registers: &mut Registers, register: Option<char>) {
    if selections.is_empty() {
        return;
    }
    let mut text = String::new();
    let mut shape = RegisterShape::Char;
    for range in selections {
        text.push_str(&motion::extract_text(buf, range));
        // A block stays a block in the register, so putting it rebuilds the
        // rectangle. Linewise still wins if mixed in.
        match range.shape {
            motion::MotionShape::Linewise => shape = RegisterShape::Line,
            motion::MotionShape::Blockwise if shape == RegisterShape::Char => shape = RegisterShape::Block,
            _ => {}
        }
    }
    registers.record_yank(register, RegisterValue { text, shape });
}

// For each window: its id, whether it's current, and its label. Owned,
// so a caller can keep it without borrowing `app`.
fn tab_bar_snapshot(app: &App) -> Vec<(u32, bool, String)> {
    // The path as the prompt shows it (`$HOME` as `~`).
    let home = std::env::var("HOME").unwrap_or_default();
    // A tab's share of the bar, less what `tab_segment_text` spends on the
    // id and spaces; each label is fitted to it, or a bar of full paths
    // would run off the line.
    let label_budget = tab_label_budget(app.term_cols, app.windows.len());
    app.windows
        .iter()
        .enumerate()
        .map(|(i, w)| {
            // A window's name, if it has one, replaces its cwd.
            let label = match &w.name {
                // A name has no structure to give up first, so it's cut at
                // the end.
                Some(name) => prompt::ellipsize_end(name, label_budget),
                None => {
                    let cwd = app.sessions[&w.owning_session()].shell.cwd.to_string_lossy();
                    prompt::fit_path(&prompt::display_path(&cwd, &home), label_budget)
                }
            };
            // A zoomed window looks like an unsplit one; the marker says
            // the other panes are still there.
            let label = if w.zoomed.is_some() { format!("{label} Z") } else { label };
            (w.id, i == app.current_window, label)
        })
        .collect()
}

fn tab_bar_line(app: &App) -> String {
    render_tab_bar(&tab_bar_snapshot(app))
}

/// What the terminal's title should be, given the focused window.
///
/// A title a program set is passed through: `vim README.md` named its
/// window. Otherwise `bish: ` and what the tab bar shows for the window
/// (its name, or its directory), so a terminal tab running bish is
/// recognisable.
fn window_title(pane_title: &str, window_name: Option<&str>, cwd: &str, home: &str) -> String {
    if !pane_title.is_empty() {
        return pane_title.to_string();
    }
    match window_name {
        Some(name) => format!("bish: {name}"),
        None => format!("bish: {}", prompt::display_path(cwd, home)),
    }
}

/// The terminal's title right now.
fn terminal_title(app: &App) -> String {
    let window = &app.windows[app.current_window];
    let pane_title = window
        .panes
        .iter()
        .find(|p| p.id == window.focused_pane)
        .and_then(|p| app.sessions.get(&p.owning_session()))
        .map(|session| session.screen.borrow().title().to_string())
        .unwrap_or_default();
    let cwd = app.sessions.get(&window.owning_session()).map(|s| s.shell.cwd.to_string_lossy().to_string()).unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    window_title(&pane_title, window.name.as_deref(), &cwd, &home)
}

/// What Ctrl+T offers, in the opener's group order: open panes, then
/// files git tracks, then their directories. Gathered fresh each time;
/// `git ls-files` is one process, cheaper than a stale pane list.
fn opener_items(app: &App) -> Vec<crate::opener::Item> {
    use crate::opener::{Item, Target};
    let home = std::env::var("HOME").unwrap_or_default();
    let mut items = Vec::new();
    for window in &app.windows {
        for (at, pane) in window.panes.iter().enumerate() {
            let Some(session) = app.sessions.get(&pane.owning_session()) else { continue };
            // The window's `[id]` from the tab bar, with the pane's place
            // in it only when there's more than one.
            let id = match window.panes.len() > 1 {
                true => format!("[{}.{}]", window.id, at + 1),
                false => format!("[{}]", window.id),
            };
            // Everything a pane is known by, so any finds it: the window's
            // name, the program's title, and its directory.
            let title = session.screen.borrow().title().to_string();
            let cwd = prompt::display_path(&session.shell.cwd.to_string_lossy(), &home);
            let text = match (&window.name, title.is_empty()) {
                (Some(name), true) => format!("{id} {name}  {cwd}"),
                (Some(name), false) => format!("{id} {name}  {title}  {cwd}"),
                (None, true) => format!("{id} {cwd}"),
                (None, false) => format!("{id} {title}  {cwd}"),
            };
            items.push(Item { target: Target::Pane { window: window.id, pane: pane.id }, text });
        }
    }
    // Outside a repository there are no files to add; the panes remain.
    let cwd = app.sessions.get(&app.windows[app.current_window].owning_session()).map(|s| s.shell.cwd.clone()).unwrap_or_default();
    let Ok(files) = crate::git::tracked_files(&cwd) else { return items };
    // With the trailing slash the file browser uses.
    let directories: Vec<Item> = crate::git::tracked_directories(&files)
        .into_iter()
        .map(|d| Item { target: Target::Directory(PathBuf::from(&d)), text: format!("{d}/") })
        .collect();
    items.extend(files.into_iter().map(|f| Item { target: Target::File(PathBuf::from(&f)), text: f }));
    items.extend(directories);
    items
}

/// The destinations Ctrl+T offers, and the one it starts on.
///
/// `opener_targets` is a glob over their names, as `tabular` is over
/// languages: `*` is all four, `*split` the two making a pane, and `tab`
/// pins it to one. A glob matching nothing falls back to all, and an
/// `opener_target` naming nothing left starts on the first remaining
/// destination; both are visible in the dialog's border.
fn opener_destinations(shell: &exec::Shell) -> (Vec<crate::opener::Destination>, crate::opener::Destination) {
    use crate::opener::Destination;
    let glob = shell.bishopt_str("opener_targets");
    let mut enabled: Vec<Destination> = Destination::ALL.into_iter().filter(|d| crate::glob::matches(&glob, d.name())).collect();
    if enabled.is_empty() {
        enabled = Destination::ALL.to_vec();
    }
    let wanted = shell.bishopt_str("opener_target");
    let start = enabled.iter().copied().find(|d| d.name() == wanted).unwrap_or(enabled[0]);
    (enabled, start)
}

/// Where the opener's dialog goes: centred over the content area, sized
/// to what it shows. Not in a pane, as where to go next belongs to no
/// window.
///
/// Three quarters of the width, up to 100 columns. The height is the
/// four rows of border, query and keys plus a row per item, fixed when
/// it opens (counting all items, not matches) so it doesn't resize as
/// you type.
///
/// The content area, not the terminal: below it are the global status
/// row, which `compositor_redraw` doesn't repaint, and the tab bar. A
/// dialog reaching the status row left its border behind on close.
/// Inside the content area, closing needs no cleanup.
///
/// Two rows of margin where there's room, so it floats over the panes.
fn opener_dialog_rect(items: usize, term_rows: usize, term_cols: usize) -> Rect {
    let content = content_rows(term_rows);
    let cols = (term_cols * 3 / 4).clamp(24, 100).min(term_cols);
    let rows = (items + 4).clamp(5, content.saturating_sub(2).max(5)).min(content);
    Rect { row: (content - rows) / 2, col: (term_cols - cols) / 2, rows, cols }
}

/// Ctrl+T's loop: the opener, until something is picked or it's put
/// away. Returns what to do, for the caller to carry out.
fn run_opener(app: &mut App) -> Option<(crate::opener::Target, crate::opener::Destination)> {
    use crate::opener::Outcome;
    let items = opener_items(app);
    let session = app.windows[app.current_window].owning_session();
    let (destinations, start) = match app.sessions.get(&session) {
        Some(state) => opener_destinations(&state.shell),
        None => (crate::opener::Destination::ALL.to_vec(), crate::opener::Destination::Tab),
    };
    let chosen = {
        let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else { return None };
        // What capture reports while this is open; see `OVERLAYS`.
        let _overlay = OverlayOpen::new();
        let item_count = items.len();
        let mut rect = opener_dialog_rect(item_count, app.term_rows, app.term_cols);
        // What's under the dialog is painted once: the dialog fills its
        // rectangle, and closing it is one more `compositor_redraw`.
        compositor_redraw(app);
        let mut view = crate::opener::Opener::new(items, destinations, start, rect.rows, rect.cols);
        let mut last_size = (app.term_rows, app.term_cols);
        loop {
            let frame = view.render(rect);
            record_paint(rect, &frame);
            print!("{frame}");
            let _ = io::stdout().flush();
            let key = match editor::read_key_idle(&mut || {
                service_background_jobs(app);
            }) {
                Ok(Some(k)) => k,
                Ok(None) | Err(_) => break None,
            };
            if (app.term_rows, app.term_cols) != last_size {
                last_size = (app.term_rows, app.term_cols);
                rect = opener_dialog_rect(item_count, app.term_rows, app.term_cols);
                compositor_redraw(app);
                view.resize(rect.rows, rect.cols);
            }
            match view.handle_key(key) {
                Outcome::Continue => {}
                Outcome::Close => break None,
                Outcome::Open(index) => break Some((view.item(index).target.clone(), view.destination())),
            }
        }
    };
    print!("{}", term::MOUSE_REPORTING_ENABLE);
    let _ = io::stdout().flush();
    chosen
}

/// Carries out the opener's choice. A pane exists, so going there is a
/// focus change. A file or directory gets a window of its own, via
/// `window new` with the command that opens it.
///
/// `typed` is the live prompt line when Ctrl+T was pressed, `None` when
/// there wasn't one (Normal mode, the hex view). `read_line` draws that
/// line straight to the screen, never into a grid, so it has to be
/// recorded before leaving or the repaint blanks its row, as with
/// click-to-focus.
fn act_on_opener_choice(app: &mut App, target: crate::opener::Target, destination: crate::opener::Destination, typed: Option<&str>) {
    use crate::opener::{Destination, Target};
    match target {
        Target::Pane { window, pane } => {
            let Some(at) = app.windows.iter().position(|w| w.id == window) else { return };
            match typed {
                Some(text) => {
                    if let Some(session) = app.sessions.get_mut(&app.windows[app.current_window].owning_session()) {
                        freeze_input_with_text(session, text);
                    }
                }
                None => freeze_focused_idle_prompt(app),
            }
            app.current_window = at;
            if app.windows[at].panes.iter().any(|p| p.id == pane) {
                // Going to a pane is how you get a minimized one back.
                restore_if_minimized(&mut app.windows[at], pane);
                app.windows[at].focused_pane = pane;
            }
            compositor_redraw(app);
        }
        // A file opens in the editor, a directory by `cd`. `tab` and the
        // splits are the `window new`/`window split` a person would type.
        //
        // `here` differs between the two: a file goes onto the focused
        // pane's frame stack, opening over whatever it shows, even another
        // editor, where a queued command line would wait for a prompt that
        // isn't coming. A directory is queued as `cd` for the pane's shell,
        // as if typed.
        Target::File(path) => match destination {
            Destination::Here => open_file_frame(app, &path, None),
            // `--` because a tracked file may be named `-x`, which the
            // editor's argument parser would refuse as an unknown option.
            // `shell_quote` stops the name being a second command; this
            // stops it being an option.
            _ => open_with(app, destination, format!("e -- {}", exec::shell_quote(&path.to_string_lossy()))),
        },
        Target::Directory(path) => {
            let command = format!("cd {}", exec::shell_quote(&path.to_string_lossy()));
            match destination {
                Destination::Here => queue_pending_input(app, Some(command)),
                _ => open_with(app, destination, command),
            }
        }
    }
}

/// Runs `command` in a new window or pane, as the destination asks.
/// `Here` never reaches this; it makes nothing.
fn open_with(app: &mut App, destination: crate::opener::Destination, command: String) {
    use crate::opener::Destination;
    let action = match destination {
        Destination::Tab => WindowAction::New { name: None, command: Some(command) },
        // `horizontal` names the dividing line, the destinations name how
        // the panes are arranged (see `WindowAction::Split`). `vsplit`
        // stacks them, as `::bish window vsplit` does, so the two can't
        // drift apart.
        Destination::VSplit => WindowAction::Split { horizontal: true, command: Some(command) },
        Destination::HSplit => WindowAction::Split { horizontal: false, command: Some(command) },
        Destination::Here => return,
    };
    apply_window_action(app, action);
}

/// Keeps the terminal's title in step. Called wherever the tab bar is
/// recomputed, as both describe the same window; `term::set_window_title`
/// skips writes that change nothing.
fn sync_terminal_title(app: &App) {
    term::set_window_title(&terminal_title(app));
}

// Everything for talking to the language server about one buffer,
// resolved in one place so the document events can't disagree about
// which server a file belongs to.
struct ServerTarget {
    id: u64,
    command: Vec<String>,
    display: String,
    root: PathBuf,
    // The document's identity to the protocol. Percent-encoded, as the
    // server matches it against its own URI for the file (see
    // `url::from_file_path`).
    uri: String,
    language_id: String,
    apply_edits: lspclient::ApplyEdits,
    // Already nested from the flat `--setting` pairs, so only the parser
    // knows about dotted keys.
    settings: crate::json::Value,
}

// `None` for an unnamed buffer, a language with no server, `bishopt
// --unset lsp`, or a file with no project root: ordinary states, not
// failures.
fn server_target(session: &SessionState, buf: &TextBuffer) -> Option<ServerTarget> {
    let path = buf.path()?;
    let language = fileeditor::language_of(buf);
    let declared = session.shell.lsp_server_for(&language)?;
    // Absolute, so finding the root doesn't depend on the shell's cwd, and
    // the URI names one file however it was reached.
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    // A `--root-cmd`'s answer, when there is one, beats the markers: only a
    // build tool can know, say, a Cargo workspace root.
    let root = match (&buf.lsp_root, absolute.parent().and_then(|dir| session.lsp_roots.get(dir))) {
        // Reached by a jump from a buffer that had a server: that server
        // knows this file, and a second would start and index a project
        // this file only sits inside. See `TextBuffer::lsp_root`.
        (Some(inherited), _) => inherited.clone(),
        (None, Some(root)) => root.clone(),
        (None, None) => lspclient::root_for(&absolute, &declared.root_markers)?,
    };
    Some(ServerTarget {
        id: declared.id,
        command: declared.command.clone(),
        display: declared.command_line(),
        root,
        uri: crate::url::from_file_path(&absolute),
        language_id: crate::lsp::language_id(&language).to_string(),
        apply_edits: lspclient::ApplyEdits::from_name(&declared.apply_edits),
        settings: lspclient::settings_tree(&declared.settings),
    })
}

// Runs a server's `--root-cmd` in `dir` and takes its first line of
// output as the project root.
fn root_from_command(command: &str, dir: &Path) -> Option<PathBuf> {
    // `bish -c` in a process of its own: `/bin/sh` lacks bish builtins like
    // `json`, which the usual
    // `cargo metadata --format-version 1 | json -r .workspace_root` needs,
    // and running in the session would let a `cd` move its cwd. One spawn
    // per directory, memoised (see `SessionState::lsp_roots`).
    let exe = std::env::current_exe().ok()?;
    let output = std::process::Command::new(exe).arg("-c").arg(command).current_dir(dir).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let root = PathBuf::from(text.lines().next()?.trim());
    // `None` on anything unconvincing (no output, a relative path, a
    // missing directory), and the caller falls back to the `--root`
    // markers: a half-working root command mustn't point a server at the
    // wrong tree.
    (root.is_absolute() && root.is_dir()).then_some(root)
}

// Works out this file's project root with the declared `--root-cmd`,
// if there is one and it hasn't been asked yet, and remembers it.
fn resolve_root_command(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &TextBuffer) {
    let Some(session) = sessions.get(&session_id) else { return };
    let Some(path) = buf.path() else { return };
    let language = fileeditor::language_of(buf);
    let Some(declared) = session.shell.lsp_server_for(&language) else { return };
    if declared.root_cmd.is_empty() {
        return;
    }
    let command = declared.root_cmd.clone();
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let Some(dir) = absolute.parent().map(|d| d.to_path_buf()) else { return };
    if session.lsp_roots.contains_key(&dir) {
        return;
    }
    let Some(root) = root_from_command(&command, &dir) else { return };
    if let Some(session) = sessions.get_mut(&session_id) {
        session.lsp_roots.insert(dir, root);
    }
}

// Starts the language server for this buffer's language if one is
// registered and not running for its project, and tells it about the
// file. Called when `editor:file:open` fires, when the language is
// known. The handshake isn't waited for: `didOpen` queues until
// `initialized` goes out.
//
// Failures are silent apart from `::bish lsp status`: opening a file is
// the wrong moment for a configuration problem, and the editor must
// work as well without a server. `run_hooks_for` is as tolerant of a
// hook that doesn't parse.
fn open_language_server_document(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &TextBuffer) {
    // A `--root-cmd` runs only here, once per file; later `server_target`
    // calls read the memo (`SessionState::lsp_roots`).
    resolve_root_command(sessions, session_id, buf);
    let Some(session) = sessions.get(&session_id) else { return };
    let Some(target) = server_target(session, buf) else { return };
    let lsp = Rc::clone(&session.lsp);
    let mut table = lsp.borrow_mut();
    // `get_or_start` records the error on the table for `::bish lsp
    // status`; there's nowhere to print it mid-open.
    let Ok(server) = table.get_or_start(target.id, &target.command, &target.display, &target.root, target.apply_edits, target.settings.clone())
    else {
        return;
    };
    server.open_document(&target.uri, &target.language_id, buf.version(), &fileeditor::buffer_text(buf));
}

// How long a buffer has to stop changing before the server hears about
// it. Measured from the first unsent change, not the latest, so steady
// typing still sends updates at a bounded rate.
const LSP_CHANGE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

// One tick of document sync for the buffer being edited, from the idle
// path of the loops driving a buffer: the only place with both the
// live text and time to spare. The text is built only when
// `needs_change` says a change has settled; otherwise this costs a
// version comparison.
fn sync_language_server_document(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &TextBuffer) {
    let Some(session) = sessions.get(&session_id) else { return };
    let Some(target) = server_target(session, buf) else { return };
    let lsp = Rc::clone(&session.lsp);
    let mut table = lsp.borrow_mut();
    let Some(server) = table.running(&target.display, &target.root) else { return };
    if !server.needs_change(&target.uri, buf.version(), std::time::Instant::now(), LSP_CHANGE_DEBOUNCE) {
        return;
    }
    server.change_document(&target.uri, buf.version(), &fileeditor::buffer_text(buf));
}

// Sends this buffer's text now, skipping the debounce, for callers
// about to ask something that depends on the character just typed. A
// version comparison when the server is already current.
fn flush_language_server_document(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &TextBuffer) {
    let Some(session) = sessions.get(&session_id) else { return };
    let Some(target) = server_target(session, buf) else { return };
    let lsp = Rc::clone(&session.lsp);
    let mut table = lsp.borrow_mut();
    let Some(server) = table.running(&target.display, &target.root) else { return };
    if !server.is_ready() || server.known_version(&target.uri) == Some(buf.version()) {
        return;
    }
    server.change_document(&target.uri, buf.version(), &fileeditor::buffer_text(buf));
}

// Advances a debug adapter session by whatever has arrived, and moves
// the cursor when the program stops. On the idle tick so the editor
// stays usable between events. A stop asks for the stack, and the next
// tick moves the cursor: only the stack says which line, and waiting
// for it here would block.
//
// Returns true when the screen should change.
fn sync_debug_adapter(app: &mut App, edit_frame_id: EditFrameId, buf: &mut TextBuffer) -> bool {
    if !app.dap_frames.contains_key(&edit_frame_id) {
        return false;
    }
    // The pane the program's own output belongs in, resolved before the
    // frame is borrowed -- both live on `app`.
    let screen = debug_run_sibling(&app.windows[app.current_window], edit_frame_id)
        .map(|pane_id| app.windows[app.current_window].pane(pane_id).owning_session())
        .and_then(|sid| app.sessions.get(&sid))
        .map(|session| Rc::clone(&session.screen));
    let frame = app.dap_frames.get_mut(&edit_frame_id).expect("checked");
    advance_debug_adapter(frame, buf, screen.as_ref())
}

/// The half of `sync_debug_adapter` that is only about the session --
/// separated from the pane arithmetic so it can be driven by a test
/// against a real adapter, which is the only way any of this was ever
/// going to be right.
fn advance_debug_adapter(frame: &mut DapFrame, buf: &mut TextBuffer, screen: Option<&Rc<RefCell<vt100::Screen>>>) -> bool {
    let events = frame.session.service();
    let mut changed = false;

    // The program's output goes to the debug-run pane as terminal bytes,
    // since it may write escape sequences. A bare newline becomes CR LF:
    // a grid moves down but not back.
    let output = frame.session.take_output();
    if !output.is_empty() {
        if let Some(screen) = screen {
            let mut screen = screen.borrow_mut();
            for piece in output.iter().filter(|o| o.category != "console") {
                screen.feed(piece.output.replace('\n', "\r\n").as_bytes());
            }
        }
        changed = true;
    }

    for event in &events {
        let crate::dap::Message::Event { event, .. } = event else { continue };
        match event.as_str() {
            "stopped" => {
                if let crate::dapclient::Execution::Stopped(stop) = frame.session.execution() {
                    frame.thread = stop.thread_id;
                }
                frame.frames.clear();
                if let Some(thread) = frame.thread {
                    frame.awaiting_stack = Some(frame.session.stack_trace(thread));
                    frame.landing = true;
                }
                changed = true;
            }
            "continued" => {
                frame.frames.clear();
                changed = true;
            }
            "terminated" | "exited" => {
                buf.echoed_message = Some(match frame.session.exit_code() {
                    Some(code) => format!("bish: dbg: {} exited with status {code}", frame.program),
                    None => format!("bish: dbg: {} ended", frame.program),
                });
                frame.frames.clear();
                changed = true;
            }
            _ => {}
        }
    }

    // The stack, when it arrives, says where the cursor goes.
    if let Some(seq) = frame.awaiting_stack
        && let Some(answer) = frame.session.take_response(seq)
    {
        frame.awaiting_stack = None;
        if let Ok(body) = answer {
            frame.frames = crate::dap::stack_frames(&body);
        }
        if frame.landing {
            frame.landing = false;
            let notice = frame.describe_stop();
            // Only when the stop is in this file; a line number from
            // another file would be worse than staying put.
            let here = buf.path().map(|p| p.to_path_buf());
            if let (Some(here), Some(top)) = (here, frame.frames.first())
                && top.source_path.as_deref().map(std::path::Path::new).is_some_and(|p| same_file(p, &here))
            {
                let row = top.line.saturating_sub(1).min(buf.line_count().saturating_sub(1));
                buf.set_cursor(row, 0);
            }
            buf.echoed_message = Some(notice);
        }
        changed = true;
    }

    // An answer asked for from the command line, once the adapter has it.
    if let Some((seq, label)) = frame.awaiting_answer.clone()
        && let Some(answer) = frame.session.take_response(seq)
    {
        frame.awaiting_answer = None;
        buf.echoed_message = Some(match answer {
            Ok(body) => format!("{label} = {}", crate::dap::evaluated(&body).result),
            Err(why) => format!("{label}: {why}"),
        });
        changed = true;
    }

    changed
}

/// Whether two paths name the same file, however spelled: an adapter
/// reports the path it was given, not necessarily the one opened.
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

// Watches the file behind `buf` and says when it changed. The check is
// a hash of the contents, as `:w`'s E13 does; the watcher only says
// when to look. A `touch`, a formatter that changed nothing, or our own
// save all raise events, and warning on those would be crying wolf.
//
// Returns true when the status line changed, like every `sync_*` here.
fn sync_watched_file(watcher: Option<&mut crate::watch::Watcher>, buf: &mut TextBuffer) -> bool {
    let Some(watcher) = watcher else { return false };
    let Some(path) = buf.path().map(|p| p.to_path_buf()) else { return false };
    if !watcher.is_watching(&path) {
        // A file whose directory is gone can't be watched; `:w` will find
        // out on its own.
        let _ = watcher.watch(&path);
        return false;
    }
    let events = watcher.events();
    if events.is_empty() {
        return false;
    }
    // An overflow means events were dropped, so all that's known is that
    // something happened: look, as for an event about the file.
    let overflowed = events.iter().any(|e| e.change == crate::watch::Change::Overflowed);
    if !overflowed && !events.iter().any(|e| e.path == path) {
        return false;
    }
    if !buf.changed_on_disk() {
        return false;
    }
    // Said once per change: `changed_on_disk` keeps answering yes until the
    // buffer next reads or writes the file, and re-echoing every tick would
    // hold the status line.
    let notice = "bish: W11: file changed on disk (`:e!` to reload, `:w!` to overwrite)".to_string();
    if buf.echoed_message.as_deref() == Some(notice.as_str()) {
        return false;
    }
    buf.echoed_message = Some(notice);
    true
}

// One tick of upkeep on what the server is doing, for the status line,
// so a cold server says so instead of each `K` silently costing
// `lsp_timeout_ms`. A string comparison on a quiet tick.
//
// Returns true when the line changed, the cue to redraw: a progress line
// updated only on the next key is reliably wrong.
fn sync_language_server_progress(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &mut TextBuffer) -> bool {
    // A message the server asked to show is taken first and holds the slot
    // until a key clears it (see `TextBuffer::echoed_message`). Taken the
    // same tick either way, or a second message would show long after its
    // cause.
    let mut changed = false;
    if let Some(message) = (|| {
        let session = sessions.get(&session_id)?;
        let target = server_target(session, buf)?;
        let lsp = Rc::clone(&session.lsp);
        let mut table = lsp.borrow_mut();
        table.running(&target.display, &target.root)?.take_shown_message()
    })() {
        buf.echoed_message = Some(message);
        changed = true;
    }
    let line = (|| {
        let session = sessions.get(&session_id)?;
        let target = server_target(session, buf)?;
        let lsp = Rc::clone(&session.lsp);
        let mut table = lsp.borrow_mut();
        table.running(&target.display, &target.root)?.progress_line()
    })();
    if buf.lsp_progress == line {
        return changed;
    }
    buf.lsp_progress = line;
    true
}

// One tick of semantic tokens for the buffer being edited. Never waits:
// `ask_server`'s bounded wait suits a key press, not a quiet moment.
// The request's id is parked on the session for a later tick to
// collect.
//
// Returns true when the tokens changed, the cue to redraw.
fn sync_semantic_tokens(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &mut TextBuffer) -> bool {
    let Some(session) = sessions.get(&session_id) else { return false };
    let Some(target) = server_target(session, buf) else { return false };
    let lsp = Rc::clone(&session.lsp);
    let version = buf.version();

    // Collect first: an answer already there beats a fresher question.
    if let Some(&(id, asked_at)) = session.lsp_semantic.get(&target.uri) {
        let mut table = lsp.borrow_mut();
        let Some(server) = table.running(&target.display, &target.root) else {
            drop(table);
            sessions.get_mut(&session_id).expect("checked").lsp_semantic.remove(&target.uri);
            return false;
        };
        if let Some(result) = server.take_response(id) {
            let legend = server.semantic_legend();
            let encoding = server.encoding();
            drop(table);
            let session = sessions.get_mut(&session_id).expect("checked");
            session.lsp_semantic.remove(&target.uri);
            // About a version since typed over, so its offsets describe moved
            // text. Dropped, and the next pass asks again.
            if asked_at != version {
                return false;
            }
            let tokens = result.map(|result| crate::lsp::semantic_tokens(&result, &legend)).unwrap_or_default();
            let spans = semantic_spans(&session.shell, buf, &tokens, encoding);
            session.lsp_semantic_applied.insert(target.uri.clone(), version);
            if spans == buf.semantic_spans {
                return false;
            }
            buf.semantic_spans = spans;
            return true;
        }
        // Still waiting. Asking once per document keeps a slow server from
        // piling up a request per tick.
        return false;
    }

    if session.lsp_semantic_applied.get(&target.uri) == Some(&version) {
        return false;
    }
    let mut table = lsp.borrow_mut();
    let Some(server) = table.running(&target.display, &target.root) else { return false };
    if !server.is_ready() || !server.provides("semanticTokensProvider") || server.semantic_legend().is_empty() {
        return false;
    }
    // The server answers about the text it was told about, so asking
    // before `didChange` caught up gets offsets into a different file. The
    // debounced sync earlier in the tick makes these equal eventually.
    if server.known_version(&target.uri) != Some(version) {
        return false;
    }
    let params = crate::json::Value::Object(vec![(
        "textDocument".to_string(),
        crate::json::Value::Object(vec![("uri".to_string(), crate::json::Value::Str(target.uri.clone()))]),
    )]);
    let id = server.request("textDocument/semanticTokens/full", params);
    drop(table);
    sessions.get_mut(&session_id).expect("checked").lsp_semantic.insert(target.uri, (id, version));
    false
}

// One tick of `textDocument/documentHighlight`: where else in this file
// the thing under the cursor appears. Keyed on the cursor as well as
// the version, since moving makes it stale. A still cursor costs a
// tuple comparison; never waits, like `sync_semantic_tokens`.
//
// Returns true when the marks changed, the cue to redraw.
fn sync_document_highlights(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &mut TextBuffer) -> bool {
    let Some(session) = sessions.get(&session_id) else { return false };
    let Some(target) = server_target(session, buf) else { return false };
    let lsp = Rc::clone(&session.lsp);
    let version = buf.version();
    let (row, col) = buf.cursor();

    // Collect first: an answer already there beats a fresher question.
    if let Some((id, asked_version, asked_row, asked_col)) = session.lsp_highlight {
        let mut table = lsp.borrow_mut();
        let Some(server) = table.running(&target.display, &target.root) else {
            drop(table);
            sessions.get_mut(&session_id).expect("checked").lsp_highlight = None;
            return false;
        };
        let Some(result) = server.take_response(id) else {
            // Still waiting; asking once keeps a slow server from piling up
            // requests.
            return false;
        };
        let encoding = server.encoding();
        drop(table);
        let session = sessions.get_mut(&session_id).expect("checked");
        session.lsp_highlight = None;
        // About a position or revision since left. Dropped, as marks under
        // the wrong words are worse than none, and asked again.
        if (asked_version, asked_row, asked_col) != (version, row, col) {
            return false;
        }
        session.lsp_highlight_applied = Some((version, row, col));
        let found = result.map(|result| crate::lsp::highlights(&result)).unwrap_or_default();
        let starts = fileeditor::line_starts_of(buf);
        let underline = vt100::CellAttrs { underline: true, ..vt100::CellAttrs::default() };
        // A write is where the symbol is assigned, usually the occurrence
        // being hunted for.
        let write = vt100::CellAttrs { underline: true, bold: true, ..vt100::CellAttrs::default() };
        let marks: Vec<highlight::StyledSpan> = found
            .iter()
            .filter_map(|h| {
                let start = fileeditor::diagnostic_offset(buf, &starts, h.start.0, h.start.1, encoding);
                let end = fileeditor::diagnostic_offset(buf, &starts, h.end.0, h.end.1, encoding);
                // `fg` isn't read for this layer (see
                // `highlight::compose_attrs`).
                (end > start).then_some(highlight::StyledSpan {
                    start,
                    end,
                    fg: vt100::Color::Default,
                    attrs: if h.write { write } else { underline },
                })
            })
            .collect();
        if marks == buf.document_highlights {
            return false;
        }
        buf.document_highlights = marks;
        return true;
    }

    if session.lsp_highlight_applied == Some((version, row, col)) {
        return false;
    }
    let mut table = lsp.borrow_mut();
    let Some(server) = table.running(&target.display, &target.root) else { return false };
    if !server.is_ready() || !server.provides("documentHighlightProvider") {
        return false;
    }
    // The server answers about the text it was told about; asking before
    // `didChange` caught up marks offsets into a different file.
    if server.known_version(&target.uri) != Some(version) {
        return false;
    }
    let encoding = server.encoding();
    let params = crate::json::Value::Object(vec![
        ("textDocument".to_string(), crate::json::Value::Object(vec![("uri".to_string(), crate::json::Value::Str(target.uri.clone()))])),
        (
            "position".to_string(),
            crate::json::Value::Object(vec![
                ("line".to_string(), crate::json::Value::Number(row as f64)),
                ("character".to_string(), crate::json::Value::Number(crate::lsp::to_server_column(&buf.line_chars(row), col, encoding) as f64)),
            ]),
        ),
    ]);
    let id = server.request("textDocument/documentHighlight", params);
    drop(table);
    sessions.get_mut(&session_id).expect("checked").lsp_highlight = Some((id, version, row, col));
    false
}

// One tick of `textDocument/inlayHint` for the visible part of the
// buffer. Keyed on version and visible range: typing and scrolling
// both invalidate, as the request takes a range. Otherwise like
// `sync_semantic_tokens`.
//
// Returns true when the hints changed, the cue to redraw.
fn sync_inlay_hints(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &mut TextBuffer) -> bool {
    // Off is off: no request, and nothing left drawn from when it was
    // on.
    if !buf.inlayhints {
        if buf.inlay_hints.is_empty() {
            return false;
        }
        buf.inlay_hints.clear();
        return true;
    }
    let Some(session) = sessions.get(&session_id) else { return false };
    let Some(target) = server_target(session, buf) else { return false };
    let lsp = Rc::clone(&session.lsp);
    let version = buf.version();
    let first = buf.viewport_top();
    let last = (first + buf.viewport_height()).min(buf.line_count()).saturating_sub(1);

    if let Some((id, asked_version, asked_first, asked_last)) = session.lsp_inlay {
        let mut table = lsp.borrow_mut();
        let Some(server) = table.running(&target.display, &target.root) else {
            drop(table);
            sessions.get_mut(&session_id).expect("checked").lsp_inlay = None;
            return false;
        };
        let Some(result) = server.take_response(id) else { return false };
        let encoding = server.encoding();
        drop(table);
        let session = sessions.get_mut(&session_id).expect("checked");
        session.lsp_inlay = None;
        if (asked_version, asked_first, asked_last) != (version, first, last) {
            return false;
        }
        session.lsp_inlay_applied = Some((version, first, last));
        let found = result.map(|result| crate::lsp::inlay_hints(&result)).unwrap_or_default();
        let mut hints: Vec<(usize, usize, String)> = found
            .into_iter()
            .filter(|hint| hint.line < buf.line_count())
            .map(|hint| {
                // The server's own column, in whatever encoding was
                // negotiated, converted to one of this buffer's chars.
                let chars = buf.line_chars(hint.line);
                (hint.line, crate::lsp::from_server_column(&chars, hint.character, encoding), hint.label)
            })
            .collect();
        crate::sort::sort_by_key(&mut hints, |(line, col, _)| (*line, *col));
        if hints == buf.inlay_hints {
            return false;
        }
        buf.inlay_hints = hints;
        return true;
    }

    if session.lsp_inlay_applied == Some((version, first, last)) {
        return false;
    }
    let mut table = lsp.borrow_mut();
    let Some(server) = table.running(&target.display, &target.root) else { return false };
    if !server.is_ready() || !server.provides("inlayHintProvider") {
        return false;
    }
    if server.known_version(&target.uri) != Some(version) {
        return false;
    }
    let encoding = server.encoding();
    let at = |line: usize, col: usize| {
        crate::json::Value::Object(vec![
            ("line".to_string(), crate::json::Value::Number(line as f64)),
            ("character".to_string(), crate::json::Value::Number(crate::lsp::to_server_column(&buf.line_chars(line), col, encoding) as f64)),
        ])
    };
    let params = crate::json::Value::Object(vec![
        ("textDocument".to_string(), crate::json::Value::Object(vec![("uri".to_string(), crate::json::Value::Str(target.uri.clone()))])),
        (
            "range".to_string(),
            crate::json::Value::Object(vec![("start".to_string(), at(first, 0)), ("end".to_string(), at(last, buf.line_len(last)))]),
        ),
    ]);
    let id = server.request("textDocument/inlayHint", params);
    drop(table);
    sessions.get_mut(&session_id).expect("checked").lsp_inlay = Some((id, version, first, last));
    false
}

// A server's tokens as drawable spans, in this buffer's own char
// offsets, keeping only the ones that resolve to a colour.
//
// The colour comes from `::bish hl` by the server's own name for the
// token type -- `::bish hl --set parameter '#d19a66'` -- which is what
// that command's open namespace was for. Failing that, from the
// `HighlightKind` the name means the same thing as, if there is one
// (`highlight::kind_for_semantic_type`). A type with neither is dropped
// here rather than drawn in some default colour: the local highlighter
// already had an opinion about that text, and overwriting it with
// "no opinion" would be a downgrade.
fn semantic_spans(
    shell: &exec::Shell,
    buf: &TextBuffer,
    tokens: &[crate::lsp::SemanticToken],
    encoding: crate::lsp::PositionEncoding,
) -> Vec<highlight::StyledSpan> {
    let starts = fileeditor::line_starts_of(buf);
    let mut out = Vec::with_capacity(tokens.len());
    // One lookup per distinct type name, not per token: a large file is
    // tens of thousands of tokens over a couple of dozen names, and
    // every miss walks the theme table.
    let mut resolved: HashMap<&str, Option<(vt100::Color, vt100::CellAttrs)>> = HashMap::new();
    for token in tokens {
        let style = *resolved.entry(token.kind.as_str()).or_insert_with(|| {
            let fallback = highlight::kind_for_semantic_type(&token.kind).map(highlight::default_style);
            match shell.hl_color(&token.kind) {
                Some(fg) => Some((fg, fallback.map(|(_, attrs)| attrs).unwrap_or_default())),
                None => fallback,
            }
        });
        let Some((fg, attrs)) = style else { continue };
        let start = fileeditor::diagnostic_offset(buf, &starts, token.line, token.start, encoding);
        let end = fileeditor::diagnostic_offset(buf, &starts, token.line, token.start + token.length, encoding);
        if end > start {
            out.push(highlight::StyledSpan { start, end, fg, attrs });
        }
    }
    out
}

// Insert mode's own view of everything repl.rs owns -- see
// `fileeditor::InsertServices` for why this is one object rather than
// two closures.
// What the file editor's Insert mode is allowed to reach back into the
// shell for: an idle tick, a completion list, a signature popup. Six of
// its eight fields used to be the same six values every other function
// here threaded, bundled by hand for exactly this one trait; `App`
// bundles them for everyone.
struct EditorServices<'a> {
    app: &'a mut App,
    session_id: SessionId,
}

impl fileeditor::InsertServices for EditorServices<'_> {
    fn idle(&mut self, buf: &mut TextBuffer) -> Option<fileeditor::IdleRedraw> {
        insert_idle(self.app, self.session_id, buf)
    }

    fn complete(&mut self, buf: &TextBuffer, row: usize, col: usize) -> Vec<crate::bishedit::completion::EditorCompletion> {
        completions_from_server(self.app, self.session_id, buf, row, col)
    }

    fn signature(&mut self, buf: &TextBuffer, row: usize, col: usize, typed: Option<char>) -> Option<Vec<String>> {
        signature_from_server(self.app, self.session_id, buf, row, col, typed)
    }
}

// The signature of the call being typed, if the server has one and this
// is a moment worth asking about.
//
// Two things decide "worth asking": the character just typed is one the
// server named as a trigger (`(` and `,` for most languages), or a
// signature is already showing -- in which case every keystroke
// refreshes it, so the active overload keeps up with what has been
// typed and the popup closes on its own once the call is finished.
// Anything else costs one string search and no request.
//
// Same bounded, servicing wait `K` uses: something *is* blocked on this
// answer, because the popup is drawn in the same frame as the character
// that opened it.
fn signature_from_server(app: &mut App, session_id: SessionId, buf: &TextBuffer, row: usize, col: usize, typed: Option<char>) -> Option<Vec<String>> {
    let triggers = signature_triggers(&app.sessions, session_id, buf)?;
    // `None` means something other than typing moved the cursor, and a
    // refresh is only wanted for a popup already up.
    if let Some(ch) = typed
        && !triggers.contains(ch)
    {
        return None;
    }
    // The one place the debounce is wrong. `sync_language_server_
    // document` deliberately waits for typing to settle, but the
    // character that opens this popup is by definition the one just
    // typed -- asking about a document the server was last told about
    // 150ms ago means asking about a file with no call in it, and
    // getting the correct answer: nothing.
    flush_language_server_document(&mut app.sessions, session_id, buf);
    let result = ask_server_at_cursor(
        app,
        session_id,
        buf,
        row,
        col,
        CursorRequest { method: "textDocument/signatureHelp", capability: "signatureHelpProvider", extra: &[] },
    )?;
    let lines = crate::lsp::signature_help(&result);
    (!lines.is_empty()).then_some(lines)
}

// The characters this buffer's server said should open a signature
// popup, plus the ones that should keep it up to date, as one string to
// search. `None` when there is no server or it does not do signatures
// at all -- which is also the "don't ask" answer.
//
// A server that declares the capability without naming any trigger
// characters gets the conventional pair, rather than never being asked:
// the capability is the claim that matters, and `(`/`,` is what every
// server that bothers to name them names.
fn signature_triggers(sessions: &HashMap<SessionId, SessionState>, session_id: SessionId, buf: &TextBuffer) -> Option<String> {
    let session = sessions.get(&session_id)?;
    let target = server_target(session, buf)?;
    let mut table = session.lsp.borrow_mut();
    let server = table.running(&target.display, &target.root)?;
    if !server.is_ready() || !server.provides("signatureHelpProvider") {
        return None;
    }
    let mut chars = String::new();
    for key in [".signatureHelpProvider.triggerCharacters", ".signatureHelpProvider.retriggerCharacters"] {
        if let Ok(crate::json::Value::Array(items)) = crate::json::query(server.capabilities(), key) {
            for item in items {
                if let crate::json::Value::Str(text) = item {
                    chars.push_str(text);
                }
            }
        }
    }
    Some(if chars.is_empty() { "(,".to_string() } else { chars })
}

// `ga`: what the server offers to do here, picked from a popup and
// applied.
//
// The popup is the completion widget over plain rows -- a labelled list
// anchored at the cursor is the same thing whether the labels are
// identifiers or refactors, and building a second one would have been
// building the same thing twice.
//
// Returns a message to show, or `None` when the picker was cancelled
// and there is nothing to say.
fn code_actions_at_cursor(app: &mut App, session_id: SessionId, buf: &mut TextBuffer, rect: Rect) -> Option<String> {
    let encoding = server_encoding_for(&app.sessions, session_id, buf)?;
    let (row, col) = buf.cursor();
    // The diagnostics on this line are the `context` that makes a
    // server offer quick *fixes* rather than only refactors -- without
    // them rust-analyzer has nothing to fix, since it is being asked
    // "what can you do here" with no mention of what is wrong.
    let starts = fileeditor::line_starts_of(buf);
    let line_start = starts[row.min(starts.len().saturating_sub(1))];
    let line_end = line_start + buf.line_len(row);
    let here: Vec<crate::json::Value> = buf
        .diagnostics
        .iter()
        .filter(|d| d.start < line_end && d.end > line_start && d.source.is_some())
        .map(|d| {
            let to_position = |offset: usize| {
                let (line, column) = fileeditor::diagnostic_position(buf, offset);
                let chars = buf.line_chars(line);
                crate::json::Value::Object(vec![
                    ("line".to_string(), crate::json::Value::Number(line as f64)),
                    ("character".to_string(), crate::json::Value::Number(crate::lsp::to_server_column(&chars, column, encoding) as f64)),
                ])
            };
            crate::json::Value::Object(vec![
                (
                    "range".to_string(),
                    crate::json::Value::Object(vec![("start".to_string(), to_position(d.start)), ("end".to_string(), to_position(d.end))]),
                ),
                ("message".to_string(), crate::json::Value::Str(d.message.clone())),
            ])
        })
        .collect();
    let chars = buf.line_chars(row);
    let character = crate::lsp::to_server_column(&chars, col, encoding);
    let position = crate::json::Value::Object(vec![
        ("line".to_string(), crate::json::Value::Number(row as f64)),
        ("character".to_string(), crate::json::Value::Number(character as f64)),
    ]);
    let result = ask_server(
        app,
        session_id,
        buf,
        "textDocument/codeAction",
        "codeActionProvider",
        vec![
            // An empty range at the cursor: "what applies here".
            ("range".to_string(), crate::json::Value::Object(vec![("start".to_string(), position.clone()), ("end".to_string(), position)])),
            ("context".to_string(), crate::json::Value::Object(vec![("diagnostics".to_string(), crate::json::Value::Array(here))])),
        ],
    )?;
    let actions = crate::lsp::code_actions(&result);
    if actions.is_empty() {
        return Some("no code actions here".to_string());
    }

    let rows: Vec<(String, String)> = actions
        .iter()
        .map(|a| {
            let detail = match (&a.disabled, &a.command) {
                (Some(why), _) => format!("({why})"),
                (None, Some(_)) if a.edit.is_none() => "(command)".to_string(),
                _ => a.kind.clone(),
            };
            (a.title.clone(), detail)
        })
        .collect();
    let chosen = pick_from_popup(&rows, buf, rect, app.term_rows, app.term_cols)?;
    let action = &actions[chosen];
    if let Some(why) = &action.disabled {
        return Some(format!("that action does not apply here: {why}"));
    }

    // A server may send an action without its edit and compute one only
    // for the action actually chosen. rust-analyzer does exactly this,
    // so an action with no edit is not an action with nothing to do.
    //
    // Only asked for when there is nothing to go on yet: an action that
    // already carries an edit, or names a command to run, is complete.
    let action = if action.edit.is_none() && action.invocation.is_none() {
        let resolved = ask_server(
            app,
            session_id,
            buf,
            "codeAction/resolve",
            "codeActionProvider",
            match &action.unresolved {
                crate::json::Value::Object(fields) => fields.clone(),
                _ => Vec::new(),
            },
        );
        match resolved.map(|r| crate::lsp::code_actions(&crate::json::Value::Array(vec![r]))) {
            Some(mut one) if !one.is_empty() => one.remove(0),
            _ => action.clone(),
        }
    } else {
        action.clone()
    };

    // Both halves, in the order the spec gives them: the edit first,
    // then the command. An action is allowed to have either, or both --
    // "apply this rewrite, then re-run the thing it was in the middle
    // of" is one action, not two.
    let mut written = 0;
    let mut unsaved = 0;
    let mut did_something = false;
    if let Some(edit) = &action.edit {
        if !edit.unsupported.is_empty() {
            return Some(format!("that action also needs to {} files, which bish cannot do yet -- nothing changed", edit.unsupported.join("/")));
        }
        if !edit.changes.is_empty() || !edit.operations.is_empty() {
            match apply_workspace_edit(&mut app.edit_frames, buf, edit, encoding, &mut |b: &mut TextBuffer, to: &Path| {
                follow_rename(&mut app.sessions, session_id, b, to)
            }) {
                Ok((w, u)) => {
                    written += w;
                    unsaved += u;
                    did_something = true;
                }
                Err(e) => return Some(e),
            }
        }
    }
    if let Some(invocation) = &action.invocation {
        match run_server_command(app, session_id, buf, invocation) {
            Ok((w, u)) => {
                written += w;
                unsaved += u;
                did_something = true;
            }
            // Reported with the action's own title, since "that command
            // timed out" on its own does not say which one.
            Err(why) => return Some(format!("\"{}\": {why}", action.title)),
        }
    }
    if !did_something {
        return Some(format!("\"{}\" produced no edit", action.title));
    }
    let total = written + unsaved;
    if total == 0 {
        // A command that ran and asked for nothing. Not an error --
        // plenty of server commands only move the server's own state.
        return Some(format!("{} -- nothing to change here", action.title));
    }
    let mut message = format!("{} -- {total} file{}", action.title, if total == 1 { "" } else { "s" });
    if unsaved > 0 {
        message.push_str(&format!(" ({unsaved} open, unsaved -- :w to keep)"));
    }
    Some(message)
}

// Draws a labelled list at the cursor and lets one be chosen. `None` if
// cancelled.
//
// Its own small key loop rather than a pane: this is a menu that
// answers a question and goes away, not a view to navigate between --
// which is exactly the distinction between this and the location list.
fn pick_from_popup(rows: &[(String, String)], buf: &TextBuffer, rect: Rect, term_rows: usize, term_cols: usize) -> Option<usize> {
    let (row, col) = buf.cursor();
    let chars = buf.line_chars(row);
    let gutter = rect.cols.saturating_sub(fileeditor::editor_content_cols(buf, rect));
    let screen_row = rect.row + row.saturating_sub(buf.viewport_top());
    let screen_col = rect.col + gutter + crate::bishedit::unicode_width::col_of(&chars, col).saturating_sub(buf.viewport_left());
    let mut selected = 0usize;
    loop {
        print!("{}", fileeditor::render_popup_list(rows, selected, screen_row, screen_col, rect));
        let _ = io::stdout().flush();
        let key = match editor::read_key_idle(&mut || {}) {
            Ok(Some(k)) => k,
            _ => return None,
        };
        match key {
            editor::Key::Char('j') | editor::Key::CtrlN | editor::Key::Down => selected = (selected + 1) % rows.len(),
            editor::Key::Char('k') | editor::Key::CtrlP | editor::Key::Up => selected = (selected + rows.len() - 1) % rows.len(),
            editor::Key::Enter => return Some(selected),
            editor::Key::Escape | editor::Key::Char('q') => return None,
            _ => {}
        }
        let _ = (term_rows, term_cols);
    }
}

// `:rename NEW` through the language server.
//
// Every refusal happens before anything is written; `apply_workspace_
// edit` below is what actually carries the change out, and its own doc
// comment explains where the edits land.
fn rename_via_server(app: &mut App, session_id: SessionId, buf: &mut TextBuffer, new_name: &str) -> Result<String, String> {
    let encoding = server_encoding_for(&app.sessions, session_id, buf).ok_or("no language server for this file")?;
    let (row, col) = buf.cursor();
    let result = ask_server_at_cursor(
        app,
        session_id,
        buf,
        row,
        col,
        CursorRequest {
            method: "textDocument/rename",
            capability: "renameProvider",
            extra: &[("newName", crate::json::Value::Str(new_name.to_string()))],
        },
    )
    .ok_or("the language server did not answer")?;
    let edit = crate::lsp::workspace_edit(&result);
    if !edit.unsupported.is_empty() {
        // Not something to do partially: see this function's own doc
        // comment, and `lsp::WorkspaceEdit::unsupported`.
        return Err(format!("this rename also needs to {} files, which bish cannot do yet -- nothing changed", edit.unsupported.join("/")));
    }
    if edit.changes.is_empty() && edit.operations.is_empty() {
        return Err("the language server had nothing to rename here".to_string());
    }

    let (written, unsaved) = apply_workspace_edit(&mut app.edit_frames, buf, &edit, encoding, &mut |b: &mut TextBuffer, to: &Path| {
        follow_rename(&mut app.sessions, session_id, b, to)
    })?;
    let mut message = format!("renamed to {new_name} in {} file{}", written + unsaved, if written + unsaved == 1 { "" } else { "s" });
    if unsaved > 0 {
        message.push_str(&format!(" ({unsaved} open, unsaved -- :w to keep)"));
    }
    Ok(message)
}

// Carries out a `WorkspaceEdit`, returning how many files were written
// and how many open buffers were left modified-but-unsaved.
//
// **All or nothing.** A change that lands in some files and not others
// leaves the project broken, which is worse than not changing anything
// -- so files that are not open are read and edited *in memory* first,
// and written only once every one of them has succeeded. A file that
// cannot be opened stops the whole thing with nothing changed.
//
// Where the changes land follows what every mainstream editor does:
// buffers that are **open** get the edits in memory and stay dirty, so
// the user keeps control of their own unsaved work; files that are
// **not** open are written, because there is nowhere else for them to
// go.
//
// Files an edit creates, renames or deletes are dealt with first, and
// only once `fileops::plan` has found nothing to refuse. A text change
// before a rename follows its file to where the rename puts it, and an
// open buffer whose file moves moves with it -- `renamed` is told, so the
// language server can be too. Anything that fails before a file holds
// new text takes the file operations back.
//
// Shared by `:rename` and by code actions, which are the same operation
// wearing different names.
fn apply_workspace_edit(
    edit_frames: &mut HashMap<EditFrameId, fileeditor::EditSession>,
    buf: &mut TextBuffer,
    edit: &crate::lsp::WorkspaceEdit,
    encoding: crate::lsp::PositionEncoding,
    renamed: &mut dyn FnMut(&mut TextBuffer, &Path),
) -> Result<(usize, usize), String> {
    use crate::fileops::Operation;
    use crate::lsp::FileOperation;
    let path_of = |uri: &str| {
        crate::url::to_file_path(uri).ok_or_else(|| format!("the language server named {uri}, which is not a local file -- nothing changed"))
    };
    // Phase one: work everything out, touching no disk.
    let mut operations = Vec::new();
    for (position, operation) in &edit.operations {
        let operation = match operation {
            FileOperation::Create { uri, overwrite, ignore_if_exists } => {
                Operation::Create { path: path_of(uri)?, overwrite: *overwrite, ignore_if_exists: *ignore_if_exists }
            }
            FileOperation::Rename { old_uri, new_uri, overwrite, ignore_if_exists } => {
                Operation::Rename { from: path_of(old_uri)?, to: path_of(new_uri)?, overwrite: *overwrite, ignore_if_exists: *ignore_if_exists }
            }
            FileOperation::Delete { uri, recursive, ignore_if_not_exists } => {
                Operation::Delete { path: path_of(uri)?, recursive: *recursive, ignore_if_not_exists: *ignore_if_not_exists }
            }
        };
        operations.push((*position, operation));
    }
    let unsaved: Vec<std::path::PathBuf> = std::iter::once(&*buf)
        .chain(edit_frames.values().map(|session| &session.buffer))
        .filter(|b| b.is_dirty())
        .filter_map(|b| b.path().map(Path::to_path_buf))
        .collect();
    let plan = crate::fileops::plan(&operations, &unsaved).map_err(|why| format!("{why} -- nothing changed"))?;
    // Every buffer is matched by where its file will be once the plan
    // has run, and every text change by where the file it names will be.
    //
    // A file can be named by more than one entry -- `documentChanges`
    // allows it -- and each entry applies to the text the one before it
    // left, so every entry is kept, in order, rather than the last.
    let here = buf.path().map(|p| plan.follow(p));
    let mut in_this_buffer: Vec<Vec<crate::lsp::TextEdit>> = Vec::new();
    let mut in_open_buffers: Vec<(EditFrameId, Vec<crate::lsp::TextEdit>)> = Vec::new();
    let mut to_load: Vec<(std::path::PathBuf, Vec<Vec<crate::lsp::TextEdit>>)> = Vec::new();
    for (position, (uri, edits)) in edit.changes.iter().enumerate() {
        // A file a later step deletes: whatever this change did to it
        // would be deleted with it.
        let Some(path) = plan.destination(position, &path_of(uri)?) else { continue };
        if here.as_deref() == Some(path.as_path()) {
            in_this_buffer.push(edits.clone());
            continue;
        }
        if let Some((id, _)) = edit_frames.iter().find(|(_, s)| s.buffer.path().map(|p| plan.follow(p)).as_deref() == Some(path.as_path())) {
            in_open_buffers.push((*id, edits.clone()));
            continue;
        }
        match to_load.iter_mut().find(|(loaded, _)| *loaded == path) {
            Some((_, entries)) => entries.push(edits.clone()),
            None => to_load.push((path, vec![edits.clone()])),
        }
    }

    // Phase two: the files themselves -- created, renamed and deleted,
    // then read and edited in memory. A failure in any of that takes
    // back every file operation, and nothing has changed.
    let mut done = crate::fileops::commit(&plan).map_err(|why| format!("{why} -- nothing changed"))?;
    let mut on_disk: Vec<(std::path::PathBuf, TextBuffer)> = Vec::new();
    for (path, entries) in to_load {
        let mut loaded = match TextBuffer::open(&path, 1) {
            Ok(loaded) => loaded,
            Err(e) => {
                done.undo();
                return Err(format!("{}: {e} -- nothing changed", path.display()));
            }
        };
        for edits in &entries {
            fileeditor::apply_text_edits(&mut loaded, edits, encoding);
        }
        on_disk.push((path, loaded));
    }

    // Phase three: commit the text.
    let written = on_disk.len();
    for (n, (path, mut loaded)) in on_disk.into_iter().enumerate() {
        if let Err(e) = loaded.save(None) {
            if n == 0 {
                done.undo();
                return Err(format!("{}: {e} -- nothing changed", path.display()));
            }
            // Files already hold their new text, under the names the
            // operations gave them, so the operations stay.
            done.finish();
            return Err(format!("{}: {e} -- some files were already written", path.display()));
        }
    }
    done.finish();
    // Open buffers follow their files before taking their edits.
    if let Some(from) = buf.path().map(Path::to_path_buf) {
        let to = plan.follow(&from);
        if to != from {
            renamed(buf, &to);
        }
    }
    for session in edit_frames.values_mut() {
        if let Some(from) = session.buffer.path().map(Path::to_path_buf) {
            let to = plan.follow(&from);
            if to != from {
                renamed(&mut session.buffer, &to);
            }
        }
    }
    for (id, edits) in &in_open_buffers {
        if let Some(session) = edit_frames.get_mut(id) {
            fileeditor::apply_text_edits(&mut session.buffer, edits, encoding);
        }
    }
    for edits in &in_this_buffer {
        fileeditor::apply_text_edits(buf, edits, encoding);
    }
    // Buffers, not entries: one buffer given two entries is one buffer
    // left to save.
    let open_buffers = in_open_buffers.iter().map(|(id, _)| id).collect::<std::collections::HashSet<_>>().len();
    Ok((written, open_buffers + usize::from(!in_this_buffer.is_empty())))
}

// An open buffer whose file an edit has just renamed. The language server
// hears the document close under its old name and open under its new
// one, which is what it would have heard had the file been closed and
// opened again.
fn follow_rename(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &mut TextBuffer, to: &Path) {
    close_language_server_document(sessions, session_id, buf);
    buf.set_path(to);
    open_language_server_document(sessions, session_id, buf);
}

// `:fmt` through the language server. `None` when there is nothing to
// ask -- no server, no `documentFormattingProvider`, no answer -- which
// is the caller's cue to fall back to bish's own formatter, so a bash
// file with no server behaves exactly as it always has.
//
// `Some(n)` is how many edits actually changed something, so a server
// that reports "already formatted" (an empty list, or edits that are
// all no-ops) is distinguishable from one that reformatted.
fn format_via_server(app: &mut App, session_id: SessionId, buf: &mut TextBuffer) -> Option<usize> {
    let encoding = server_encoding_for(&app.sessions, session_id, buf)?;
    // The buffer's own indent settings, which is what the file already
    // agrees with -- editorconfig and `bishopt` both feed these, so a
    // server is told the same thing `>>` and Tab already obey rather
    // than whatever its own default happens to be.
    let options = crate::json::Value::Object(vec![
        ("tabSize".to_string(), crate::json::Value::Number(buf.shiftwidth as f64)),
        ("insertSpaces".to_string(), crate::json::Value::Bool(buf.expandtab)),
        ("trimTrailingWhitespace".to_string(), crate::json::Value::Bool(buf.trim_trailing_whitespace)),
        ("insertFinalNewline".to_string(), crate::json::Value::Bool(buf.final_newline)),
    ]);
    let result = ask_server(app, session_id, buf, "textDocument/formatting", "documentFormattingProvider", vec![("options".to_string(), options)])?;
    Some(fileeditor::apply_text_edits(buf, &crate::lsp::text_edits(&result), encoding))
}

// What could go where the cursor is, for the editor's completion
// popup. Empty for every ordinary reason there is nothing to offer --
// no server, not ready, no `completionProvider`, no answer in time --
// which is what makes Ctrl-N a no-op rather than an error.
//
// The server's own replace ranges are resolved to buffer coordinates
// here, where the position encoding is known, so the editor never sees
// anything protocol-shaped (see `EditorCompletion`).
fn completions_from_server(
    app: &mut App,
    session_id: SessionId,
    buf: &TextBuffer,
    row: usize,
    col: usize,
) -> Vec<crate::bishedit::completion::EditorCompletion> {
    let encoding = server_encoding_for(&app.sessions, session_id, buf).unwrap_or(crate::lsp::PositionEncoding::Utf16);
    let Some(result) = ask_server_at_cursor(
        app,
        session_id,
        buf,
        row,
        col,
        CursorRequest { method: "textDocument/completion", capability: "completionProvider", extra: &[] },
    ) else {
        return Vec::new();
    };
    let mut found = crate::lsp::completions(&result);
    // `sortText` is how a server puts the likely answers first, and
    // ignoring it is how a completion list ends up alphabetical and
    // useless.
    crate::sort::sort_by(&mut found, |a, b| a.sort.cmp(&b.sort));
    let starts = fileeditor::line_starts_of(buf);
    found
        .into_iter()
        .map(|item| {
            // Only a range on the cursor's own line can be expressed as
            // a single-line replacement, which is all the editor does
            // here; anything else falls back to the word being typed.
            let replace = item.edit.and_then(|(start, end)| {
                (start.0 == end.0 && start.0 == row).then(|| {
                    let to_col = |position: (usize, usize)| {
                        let offset = fileeditor::diagnostic_offset(buf, &starts, position.0, position.1, encoding);
                        fileeditor::diagnostic_position(buf, offset).1
                    };
                    (row, to_col(start), to_col(end))
                })
            });
            let detail = item.detail_or_kind();
            crate::bishedit::completion::EditorCompletion { label: item.label, detail, insert: item.insert, replace, snippet: item.snippet }
        })
        .collect()
}

// Asks the language server one position-shaped question about whatever
// is under the cursor -- `textDocument/hover` for `K`,
// `textDocument/definition` for `gd` -- and returns its raw result.
// `None`, meaning "fall back to whatever bish knows itself", for every
// ordinary reason there might not be an answer: no server, not ready
// yet, doesn't advertise `capability`, answered with an error, or
// didn't answer in time.
//
// **This one blocks**, unlike everything else in this file's LSP path,
// and that is the deliberate trade. Hover is a question the user just
// asked and is waiting for; making the whole editor loop asynchronous
// to serve it would be a rewrite of every mode. So it waits in place --
// the same call `debugger.rs`'s pause loop already makes for the same
// reason -- but it waits *servicing*: `service_background_jobs` runs on
// every tick of the wait, so other panes' jobs keep draining, a resize
// still lands, and the server's own stream keeps being read (which is
// what delivers the reply at all).
//
// A keystroke abandons the wait. Someone who has started typing again
// is no longer waiting for this answer, and holding their input for the
// rest of a timeout to show them a popup they've moved past is the
// wrong side of the trade.
// What a cursor-position request is, apart from where the cursor is.
// Three fields that always travel together and mean nothing apart --
// which is also what got `ask_server_at_cursor` over clippy's argument
// limit once `App` had taken the other five.
struct CursorRequest<'a> {
    method: &'a str,
    capability: &'a str,
    // Anything the method needs beyond `textDocument`/`position`.
    // `textDocument/references` is the reason this exists: its
    // `context` is *required*, not optional, so omitting it is a
    // malformed request rather than a sparse one.
    extra: &'a [(&'a str, crate::json::Value)],
}

/// The hover popup to print, if the pointer has been resting long
/// enough on something worth describing.
///
/// `None` almost every time it is called -- this runs on every idle
/// poll -- so everything that would cost anything is behind the cheap
/// checks: the option, the dwell, and whether a popup is already up.
///
/// Takes `hover_shown` by reference and sets it, so the caller cannot
/// print a popup without recording that one is on screen; the two going
/// out of step is how a popup gets left behind with nothing to remove
/// it.
#[allow(clippy::too_many_arguments)]
fn hover_popup_due(
    app: &mut App,
    session_id: SessionId,
    edit_frame_id: Option<EditFrameId>,
    buf: &NavBuffer,
    rect: Rect,
    hover_at: &mut Option<(std::time::Instant, u16, u16)>,
    hover_shown: &mut bool,
) -> Option<String> {
    if *hover_shown {
        return None;
    }
    let (since, row1, col1) = (*hover_at)?;
    if since.elapsed() < HOVER_DWELL {
        return None;
    }
    // Only where `K` itself means something: a real file buffer. A
    // pane's scrollback has no symbols to look up.
    let NavBuffer::Editable(tb) = buf else { return None };
    // `is_none_or`, matching the guard that asked the terminal for
    // these reports in the first place: no session is the `bish tool
    // edit` case, and the two disagreeing meant the mode was enabled
    // and then nothing ever read it.
    if !app.sessions.get(&session_id).is_none_or(|s| s.shell.bishopt_bool("mouse_hover")) {
        return None;
    }
    let (row0, col0) = ((row1 as usize).saturating_sub(1), (col1 as usize).saturating_sub(1));
    // The gutter first: it is not text, and asking `position_at_screen`
    // about it gets the line's first character, which is how hovering a
    // line number used to describe whatever word happened to start that
    // line.
    if let Some((line, column)) = fileeditor::gutter_at_screen(tb, rect, row0, col0) {
        *hover_at = None;
        let lines = gutter_hover_lines(tb, line, column);
        if lines.is_empty() {
            return None;
        }
        *hover_shown = true;
        return Some(fileeditor::render_hover_popup(&lines, row0, col0, rect));
    }
    let Some((line, col)) = fileeditor::position_at_screen(tb, rect, row0, col0) else {
        *hover_at = None;
        return None;
    };
    let lines = hover_lines_at(app, session_id, edit_frame_id, tb, line, col);
    // A man page still being fetched in the background. Left pending on
    // purpose -- `hover_at` is not cleared, so the next idle poll asks
    // again and the popup appears when the page lands. `K`'s own answer
    // for this is "press K again in a moment", which is advice about a
    // key nobody pressed.
    if docs::is_still_looking(&lines) {
        return None;
    }
    // Asked and answered, whatever the answer was: without clearing
    // this, a cell with nothing to say is looked up again on every idle
    // poll for as long as the pointer rests on it.
    *hover_at = None;
    // Nothing worth interrupting for. `K` says "no info available"
    // because it was asked; a popup that appears on its own has no
    // business appearing to say that.
    if docs::says_nothing(&lines) {
        return None;
    }
    *hover_shown = true;
    // Anchored to the cell the pointer is on, not to the text cursor --
    // the popup belongs to what is being pointed at.
    Some(fileeditor::render_hover_popup(&lines, row0, col0, rect))
}

/// What resting the pointer on a gutter column says.
///
/// The other half of the division in `gutter_click`: these three
/// columns draw a mark meaning "something is true of this line", and
/// this is where the mark says what. The marker itself has room for one
/// coloured bullet; the answer does not fit in it.
///
/// Empty for a column with nothing to add, and for a cell with no mark
/// in it -- hovering the blank part of a gutter should be as quiet as
/// hovering blank text.
fn gutter_hover_lines(tb: &TextBuffer, line: usize, column: fileeditor::GutterKind) -> Vec<String> {
    match column {
        fileeditor::GutterKind::Diagnostic => {
            fileeditor::diagnostics_on_line(tb, line).into_iter().map(|d| format!("{}: {}", d.label(), d.message)).collect()
        }
        // `git blame`'s three fields, which is the whole of what the
        // column is drawn from -- it shows them abbreviated to fit, and
        // this is the unabbreviated form.
        fileeditor::GutterKind::Blame => tb
            .blame
            .as_ref()
            .and_then(|b| b.get(line))
            .and_then(|b| b.as_ref())
            .map(|b| vec![format!("{} {}", b.short_commit, b.author), b.date.clone()])
            .unwrap_or_default(),
        fileeditor::GutterKind::Diff => tb
            .diff
            .as_ref()
            .and_then(|d| d.get(&line))
            .map(|mark| {
                vec![match mark {
                    crate::git::DiffMark::Added => "added since the version being compared".to_string(),
                    crate::git::DiffMark::Changed => "changed since the version being compared".to_string(),
                    crate::git::DiffMark::Removed => "lines were removed here".to_string(),
                }]
            })
            .unwrap_or_default(),
        // A breakpoint's marker already says everything a breakpoint
        // has to say, and a line number says its own number.
        fileeditor::GutterKind::Breakpoint | fileeditor::GutterKind::LineNumber => Vec::new(),
    }
}

/// What a click on a gutter column *means*, decided without doing it.
///
/// Separated from `gutter_click` so the policy -- which column does
/// what -- can be stated and tested on its own. That matters most for
/// the columns that deliberately do nothing: "nothing" is a decision
/// here rather than the absence of code somewhere, and a test can hold
/// it to that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GutterAction {
    /// 1-based, the way breakpoints are numbered everywhere else.
    ToggleBreakpoint(usize),
    /// 0-based, the way the buffer numbers its lines.
    GoToLine(usize),
    /// Deliberately nothing -- see `gutter_action`.
    Nothing,
}

/// The division is that **clicking acts and hovering informs**: a
/// column whose click would only be "tell me about this line" does not
/// need one, because resting the pointer there already says it.
///
/// Two columns act, one goes where it points, and two do nothing on
/// purpose:
///
///   - **Breakpoint** -- the marker is the control, so clicking it is
///     how you set one and how you take it away again.
///   - **Line number** -- goes to the line, which is what clicking one
///     means everywhere it means anything.
///   - **Diagnostic**, **blame** and **diff** -- nothing at all, and
///     that is the decision rather than an omission. Each has an
///     obvious action waiting for it: clicking a problem marker should
///     offer the fix, clicking a blame line should open that commit,
///     clicking a diff marker should go to the hunk. None is settled,
///     and an incidental "puts the cursor on the line" is exactly the
///     sort of behaviour that becomes load-bearing while nobody is
///     looking -- at which point giving the click its real meaning is
///     a change that takes something away. Reserving them costs
///     nothing now and keeps that a free choice.
fn gutter_action(column: fileeditor::GutterKind, line: usize) -> GutterAction {
    match column {
        fileeditor::GutterKind::Breakpoint => GutterAction::ToggleBreakpoint(line + 1),
        fileeditor::GutterKind::LineNumber => GutterAction::GoToLine(line),
        fileeditor::GutterKind::Diagnostic | fileeditor::GutterKind::Blame | fileeditor::GutterKind::Diff => GutterAction::Nothing,
    }
}

/// Carries out `gutter_action`. `Some` is a line for the status row.
///
/// A reserved column still *consumes* the click: letting it fall
/// through would place the cursor by the text path, which is the
/// behaviour being withheld.
fn gutter_click(
    app: &mut App,
    edit_frame_id: Option<EditFrameId>,
    tb: &mut TextBuffer,
    line: usize,
    column: fileeditor::GutterKind,
) -> Option<String> {
    match gutter_action(column, line) {
        GutterAction::ToggleBreakpoint(at) => {
            let added = match edit_frame_id {
                Some(id) => toggle_breakpoint(app, id, tb, at),
                // No frame means no adapter to tell, but the marker is
                // still the user's to set.
                None => {
                    let added = tb.breakpoints.insert(at);
                    if !added {
                        tb.breakpoints.remove(&at);
                    }
                    added
                }
            };
            Some(format!("bish: breakpoint {} at line {at}", if added { "set" } else { "cleared" }))
        }
        GutterAction::GoToLine(line) => {
            tb.set_cursor(line.min(tb.line_count().saturating_sub(1)), 0);
            None
        }
        GutterAction::Nothing => None,
    }
}

/// Adds or removes a breakpoint on `line` (1-based), and tells a live
/// debug adapter about it.
///
/// Shared by `:dbg break` and by clicking the gutter, so the two cannot
/// drift: a breakpoint set one way has to reach the adapter the same as
/// one set the other, or a click would put a marker on screen that
/// nothing is watching.
fn toggle_breakpoint(app: &mut App, edit_frame_id: EditFrameId, tb: &mut TextBuffer, line: usize) -> bool {
    let added = tb.breakpoints.insert(line);
    if !added {
        tb.breakpoints.remove(&line);
    }
    sync_breakpoints_to_adapter(app, edit_frame_id, tb);
    added
}

/// Tells a live debug adapter what this buffer's breakpoints now are.
///
/// The whole set rather than the change, because that is what
/// `setBreakpoints` takes -- it replaces a file's list outright. Called
/// after every way of changing them, so a marker on screen and a
/// breakpoint in the debugger cannot disagree.
fn sync_breakpoints_to_adapter(app: &mut App, edit_frame_id: EditFrameId, tb: &TextBuffer) {
    if let (Some(frame), Some(path)) = (app.dap_frames.get_mut(&edit_frame_id), tb.path()) {
        let lines: Vec<usize> = tb.breakpoints.iter().copied().collect();
        frame.session.set_breakpoints(path, &lines);
    }
}

/// Everything this editor knows about whatever is at `(row, col)` --
/// what `K` prints, and what resting the pointer there shows.
///
/// One function because they are one question. They were the same code
/// written once, and a hover that disagreed with `K` about the same
/// word would be the sort of difference nobody thinks to test for.
///
/// Empty when there is nothing to say. The two callers differ only in
/// what they do about that: `K` was asked and answers anyway, hover was
/// not and stays quiet.
fn hover_lines_at(app: &mut App, session_id: SessionId, edit_frame_id: Option<EditFrameId>, tb: &TextBuffer, row: usize, col: usize) -> Vec<String> {
    // A language server's answer wins when there is one: it knows the
    // actual language, where `docs.rs` knows bash and this buffer's own
    // comments.
    if let Some(lines) =
        ask_server_at_cursor(app, session_id, tb, row, col, CursorRequest { method: "textDocument/hover", capability: "hoverProvider", extra: &[] })
            .and_then(|result| crate::lsp::hover_lines(&result))
    {
        return lines;
    }
    let chars = tb.line_chars(row);
    let line_text: String = chars.iter().collect();
    let base_path = tb.path().map(|p| p.to_path_buf()).unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join("untitled"));
    // Rebuilt from this buffer's own *live* text every time, not
    // re-read off disk, so an unsaved edit is reflected immediately.
    let index = docs::DocIndex::build_from_source(&tb.text(), &base_path);
    let debug_session = edit_frame_id.and_then(|id| app.debug_frames.get(&id));
    docs::hover_lines_at(&chars, col, &line_text, &index, |name| debug_session.and_then(|s| s.peek_var(name)))
}

fn ask_server_at_cursor(
    app: &mut App,
    session_id: SessionId,
    buf: &TextBuffer,
    row: usize,
    col: usize,
    request: CursorRequest,
) -> Option<crate::json::Value> {
    let CursorRequest { method, capability, extra } = request;
    let chars = buf.line_chars(row);
    let encoding = server_encoding_for(&app.sessions, session_id, buf).unwrap_or(crate::lsp::PositionEncoding::Utf16);
    let character = crate::lsp::to_server_column(&chars, col, encoding);
    let mut params = vec![(
        "position".to_string(),
        crate::json::Value::Object(vec![
            ("line".to_string(), crate::json::Value::Number(row as f64)),
            ("character".to_string(), crate::json::Value::Number(character as f64)),
        ]),
    )];
    params.extend(extra.iter().map(|(k, v)| ((*k).to_string(), v.clone())));
    ask_server(app, session_id, buf, method, capability, params)
}

// The general form: one request about this buffer, with whatever params
// the method takes, waited for the same bounded, servicing way.
//
// `textDocument` is added here rather than by every caller, since every
// method that is about a document needs it and none of them want to
// build the uri themselves.
fn ask_server(
    app: &mut App,
    session_id: SessionId,
    buf: &TextBuffer,
    method: &str,
    capability: &str,
    mut params: Vec<(String, crate::json::Value)>,
) -> Option<crate::json::Value> {
    let session = app.sessions.get(&session_id)?;
    let target = server_target(session, buf)?;
    let timeout = std::time::Duration::from_millis(session.shell.bishopt_int("lsp_timeout_ms").max(0) as u64);
    let lsp = Rc::clone(&session.lsp);

    let id = {
        let mut table = lsp.borrow_mut();
        let server = table.running(&target.display, &target.root)?;
        // Asking a server that hasn't said it answers this would spend
        // the whole timeout to be told so.
        if !server.is_ready() || !server.provides(capability) {
            return None;
        }
        // Only for the methods that are actually *about* a document.
        // `codeAction/resolve` takes the action back verbatim and
        // `workspace/symbol` takes a query, and adding a field of our
        // own to either is at best ignored and at worst confusing to
        // read in a server's log.
        if method.starts_with("textDocument/") {
            params.insert(
                0,
                ("textDocument".to_string(), crate::json::Value::Object(vec![("uri".to_string(), crate::json::Value::Str(target.uri.clone()))])),
            );
        }
        server.request(method, crate::json::Value::Object(std::mem::take(&mut params)))
    };

    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        service_background_jobs(app);
        if let Some(result) = lsp.borrow_mut().running(&target.display, &target.root)?.take_response(id) {
            // A server answering with an error has answered: there is
            // nothing better to wait for, and the caller's own fallback
            // is still a sensible thing to do instead.
            return result.ok();
        }
        // Doubles as the tick: a byte arriving means the user has moved
        // on, so the wait is abandoned rather than held out.
        if term::stdin_ready(editor::IDLE_POLL_MS) {
            return None;
        }
    }
    None
}

// Runs a server-side command and applies whatever it asks for on the
// way.
//
// This is the other half of a command-style code action, and the reason
// it cannot go through `ask_server`: the command's *result* is almost
// always `null`. The work arrives as a separate `workspace/applyEdit`
// request from the server, mid-command, and answering it is what makes
// the command finish at all -- a server waiting on that reply will sit
// there until the timeout otherwise.
//
// So the wait loop here is `ask_server`'s with one thing added: each
// tick, whatever edits the server parked (see `Server::take_apply_edit`
// -- only edits the `--apply-edits` policy already accepted get parked)
// are applied and answered. That is also what defines the scope in
// `--apply-edits=scoped`: the window is open from the request going out
// to its response coming back, and `refuse_pending_applies` closes it
// on every exit from this function, including the timeout.
//
// Returns how many files were changed and how many of those are open
// and unsaved, or an error to show.
fn run_server_command(app: &mut App, session_id: SessionId, buf: &mut TextBuffer, invocation: &crate::json::Value) -> Result<(usize, usize), String> {
    let Some(session) = app.sessions.get(&session_id) else { return Err("no session".to_string()) };
    let Some(target) = server_target(session, buf) else { return Err("no language server for this file".to_string()) };
    let timeout = std::time::Duration::from_millis(session.shell.bishopt_int("lsp_timeout_ms").max(0) as u64);
    let encoding = server_encoding_for(&app.sessions, session_id, buf).unwrap_or(crate::lsp::PositionEncoding::Utf16);
    let lsp = Rc::clone(&app.sessions.get(&session_id).expect("checked").lsp);

    let id = {
        let mut table = lsp.borrow_mut();
        let Some(server) = table.running(&target.display, &target.root) else { return Err("that server is not running".to_string()) };
        if !server.is_ready() {
            return Err("that server is still starting up".to_string());
        }
        if !server.provides("executeCommandProvider") {
            return Err("that server did not say it runs commands".to_string());
        }
        if server.apply_edits() == lspclient::ApplyEdits::Never {
            return Err("this server is configured --apply-edits=never, so its commands cannot change anything".to_string());
        }
        let mut params = match invocation {
            crate::json::Value::Object(fields) => fields.clone(),
            _ => Vec::new(),
        };
        // A `Command`'s own `title` is menu text, not a parameter.
        params.retain(|(name, _)| name == "command" || name == "arguments");
        server.execute_command(crate::json::Value::Object(params))
    };

    let mut written = 0;
    let mut unsaved = 0;
    let mut failure = None;
    let deadline = std::time::Instant::now() + timeout;
    let mut done = false;
    while !done && std::time::Instant::now() < deadline {
        service_background_jobs(app);
        loop {
            let taken = {
                let mut table = lsp.borrow_mut();
                match table.running(&target.display, &target.root) {
                    Some(server) => server.take_apply_edit(),
                    None => None,
                }
            };
            let Some((request_id, params)) = taken else { break };
            // The edit is applied with the table *not* borrowed:
            // writing a file can reach back into the same table (an
            // open buffer's own `didChange`), and holding the borrow
            // across that is how a RefCell panic gets written.
            let edit = crate::lsp::workspace_edit(&crate::json::query(&params, ".edit").cloned().unwrap_or(crate::json::Value::Null));
            let applied = if !edit.unsupported.is_empty() {
                failure = Some(format!("that command wanted to {} files, which bish cannot do -- nothing changed", edit.unsupported.join("/")));
                false
            } else {
                match apply_workspace_edit(&mut app.edit_frames, buf, &edit, encoding, &mut |b: &mut TextBuffer, to: &Path| {
                    follow_rename(&mut app.sessions, session_id, b, to)
                }) {
                    Ok((w, u)) => {
                        written += w;
                        unsaved += u;
                        true
                    }
                    Err(why) => {
                        failure = Some(why);
                        false
                    }
                }
            };
            if let Some(server) = lsp.borrow_mut().running(&target.display, &target.root) {
                server.reply_to_apply(request_id, applied);
            }
        }
        let mut table = lsp.borrow_mut();
        let Some(server) = table.running(&target.display, &target.root) else { break };
        if let Some(result) = server.take_response(id) {
            if let Err(e) = result {
                failure = Some(format!("the server refused to run it: {}", e.message));
            }
            done = true;
        }
    }
    if let Some(server) = lsp.borrow_mut().running(&target.display, &target.root) {
        server.refuse_pending_applies();
    }
    match failure {
        Some(why) => Err(why),
        None if !done => Err("that command timed out".to_string()),
        None => Ok((written, unsaved)),
    }
}

// The position encoding the server for this buffer negotiated, if there
// is one running. `None` when there isn't, in which case any position
// arithmetic is moot anyway.
fn server_encoding_for(sessions: &HashMap<SessionId, SessionState>, session_id: SessionId, buf: &TextBuffer) -> Option<crate::lsp::PositionEncoding> {
    let session = sessions.get(&session_id)?;
    let target = server_target(session, buf)?;
    let lsp = Rc::clone(&session.lsp);
    let mut table = lsp.borrow_mut();
    Some(table.running(&target.display, &target.root)?.encoding())
}

// Where the cursor is, as a jump-list entry -- `None` for anything with
// no file behind it (the prompt's own Normal mode, a job's scrollback),
// which is also every case where a cross-file jump could not mean
// anything.
fn jump_here(buf: &NavBuffer) -> Option<JumpEntry> {
    let tb = buf.as_editable()?;
    let (row, col) = tb.cursor();
    Some(JumpEntry { path: tb.path()?.to_path_buf(), row, col })
}

// Records a jump leaving `buf`, on the pane that owns it.
fn record_jump(windows: &mut [WindowEntry], current_window: usize, buf: &NavBuffer) {
    let Some(entry) = jump_here(buf) else { return };
    let pane_id = windows[current_window].focused_pane;
    windows[current_window].pane_mut(pane_id).jumps.push(entry);
}

// What acting on one `lsp::Location` did.
enum Goto {
    /// The cursor moved within this buffer.
    Moved,
    /// The target is a different file, which this loop cannot open --
    /// see `NavExit::OpenAt`.
    Elsewhere { path: PathBuf, line: usize, character: usize, encoding: crate::lsp::PositionEncoding },
    /// Nothing to go to, with what to say about it.
    Nowhere(String),
}

// Moves the cursor to `location` if it is in this buffer, or reports
// what else it is. Shared by `gd`/`gy`/`gD` and by the `n`/`N` cycling
// that follows one, so they cannot disagree about what "go there"
// means.
//
// Recording the jump is the caller's job (`record_jump`), so that a
// `gd` leaves exactly one entry behind -- at the place the question was
// asked -- while cycling through its results with `n`/`N` leaves none.
fn goto_location(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: SessionId,
    buf: &mut NavBuffer,
    location: &crate::lsp::Location,
    // What the caller asked for, for the two messages this can produce
    // -- "definition", "type definition", "declaration".
    noun: &str,
) -> Goto {
    let Some(tb) = buf.as_editable_mut() else {
        return Goto::Nowhere(format!("no {noun} found"));
    };
    let here = tb.path().map(crate::url::from_file_path);
    let encoding = server_encoding_for(sessions, session_id, tb).unwrap_or(crate::lsp::PositionEncoding::Utf16);
    if Some(&location.uri) == here.as_ref() {
        let starts = fileeditor::line_starts_of(tb);
        let offset = fileeditor::diagnostic_offset(tb, &starts, location.start.0, location.start.1, encoding);
        let (line, column) = fileeditor::diagnostic_position(tb, offset);
        tb.set_cursor(line, column);
        return Goto::Moved;
    }
    match crate::url::to_file_path(&location.uri) {
        Some(path) => Goto::Elsewhere { path, line: location.start.0, character: location.start.1, encoding },
        // A uri naming no local file at all (`jar:`, `zipfile:`, a
        // server's own synthetic scheme) is nothing bish can open.
        None => Goto::Nowhere(format!("{noun} is at {}:{}", location.uri, location.start.0 + 1)),
    }
}

// Asks this buffer's server to hand over its last publication again on
// the next idle tick. See `Server::redeliver_diagnostics`.
fn redeliver_language_server_diagnostics(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &TextBuffer) {
    let Some(session) = sessions.get(&session_id) else { return };
    let Some(target) = server_target(session, buf) else { return };
    let lsp = Rc::clone(&session.lsp);
    let mut table = lsp.borrow_mut();
    if let Some(server) = table.running(&target.display, &target.root) {
        server.redeliver_diagnostics(&target.uri);
    }
}

// Applies whatever the language server has most recently said about
// this buffer. `true` when the buffer's diagnostics actually changed,
// which is the caller's cue to repaint.
//
// Runs from the same idle path as `sync_language_server_document`, and
// for the same reason: a publication arrives whenever the server feels
// like sending one, and the editor loop is the only thing holding the
// buffer it describes.
//
// **Version-gated.** A publication naming a revision the buffer has
// already left is dropped rather than drawn: its offsets address text
// that no longer exists, and putting a squiggle under the wrong word is
// worse than showing none. A server that says nothing about the version
// is taken at its word, since there is nothing else to go on. This is
// what `TextBuffer::version` was added for.
fn apply_language_server_diagnostics(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &mut TextBuffer) -> bool {
    let Some(session) = sessions.get(&session_id) else { return false };
    let Some(target) = server_target(session, buf) else { return false };
    let lsp = Rc::clone(&session.lsp);
    let mut table = lsp.borrow_mut();
    // The server this buffer's findings came from may be gone -- it
    // crashed, or `::bish lsp restart` took it away. Its underlines and
    // gutter marks are then claims nobody stands behind, and nothing
    // will ever come along to correct them, since diagnostics are
    // pushed rather than asked for.
    //
    // `source` is what tells the two apart: a server's findings carry
    // one, and bish's own linter (`:diag`) sets none. So this clears
    // exactly the dead server's half and leaves `:diag`'s alone.
    if table.running(&target.display, &target.root).is_none_or(|server| server.is_dead()) {
        drop(table);
        let kept: Vec<lint::Diagnostic> = buf.diagnostics.iter().filter(|d| d.source.is_none()).cloned().collect();
        if kept.len() == buf.diagnostics.len() {
            return false;
        }
        buf.diagnostics = kept;
        return true;
    }
    let server = table.running(&target.display, &target.root).expect("checked just above");
    let encoding = server.encoding();
    let Some(publication) = server.take_diagnostics(&target.uri) else { return false };
    if publication.version.is_some_and(|v| v != buf.version()) {
        return false;
    }
    let relayed = fileeditor::diagnostics_from_server(buf, &publication.findings, encoding);
    drop(table);
    // A publication replaces this server's findings outright, and
    // leaves bish's own linter output (`:diag`, which sets no `source`)
    // exactly where it was. The two coexist in one list precisely so
    // that the gutter, the underlines and the diagnostics pane need to
    // know nothing about where a finding came from.
    let mut merged: Vec<lint::Diagnostic> = buf.diagnostics.iter().filter(|d| d.source.is_none()).cloned().collect();
    merged.extend(relayed);
    // Document order, so the diagnostics pane reads top to bottom
    // however the two sources interleave.
    crate::sort::sort_by_key(&mut merged, |d| (d.start, d.end));
    if merged == buf.diagnostics {
        return false;
    }
    buf.diagnostics = merged;
    true
}

// `:w` happened. Sent after the write rather than before it, so a
// server that reruns a build on save is looking at what is actually on
// disk.
fn save_language_server_document(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &TextBuffer) {
    let Some(session) = sessions.get(&session_id) else { return };
    let Some(target) = server_target(session, buf) else { return };
    let lsp = Rc::clone(&session.lsp);
    let mut table = lsp.borrow_mut();
    let Some(server) = table.running(&target.display, &target.root) else { return };
    // A save with unsent edits behind it would otherwise tell the server
    // "this is saved" about a version it has never seen.
    if server.has_document(&target.uri) {
        server.change_document(&target.uri, buf.version(), &fileeditor::buffer_text(buf));
        server.save_document(&target.uri, &fileeditor::buffer_text(buf));
    }
}

// The buffer is going away. Fired while it still exists, which is what
// makes the URI resolvable at all.
fn close_language_server_document(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, buf: &TextBuffer) {
    let Some(session) = sessions.get(&session_id) else { return };
    let Some(target) = server_target(session, buf) else { return };
    let lsp = Rc::clone(&session.lsp);
    let mut table = lsp.borrow_mut();
    if let Some(server) = table.running(&target.display, &target.root) {
        server.close_document(&target.uri);
    }
}

// Runs whatever `::bish hook` attached to `event` for this buffer's
// language, with the file's path as the first argument. Output is
// captured and dropped: the editor owns the terminal, and an echo would
// land mid-frame. With nobody listening this costs a `Vec` and a string
// compare.
fn run_hooks(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, event: &str, buf: &TextBuffer) {
    let path = buf.path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    run_hooks_for(sessions, session_id, event, &fileeditor::language_of(buf), &path);
}

// The same for an event with no buffer behind it. A `shell:` event's
// language is bash -- the prompt *is* bash, which is also what
// `abbr --lang` defaults to, so one `--lang=bash` hook means the same
// thing in both places.
fn run_shell_hooks(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, event: &str, argument: &str) {
    run_hooks_for(sessions, session_id, event, crate::bishedit::snippet::DEFAULT_LANG, argument);
}

fn run_hooks_for(sessions: &mut HashMap<SessionId, SessionState>, session_id: SessionId, event: &str, language: &str, argument: &str) {
    let Some(session) = sessions.get(&session_id) else { return };
    let commands = session.shell.hooks_for(event, language);
    if commands.is_empty() {
        return;
    }
    let path = argument.to_string();
    let Some(session) = sessions.get_mut(&session_id) else { return };
    // Anything these do can't fire more hooks -- a `shell:cwd:change`
    // hook that `cd`s would otherwise keep triggering itself.
    session.shell.set_firing_hooks(true);
    // A hook is not a command the user ran, so it must not become the
    // answer to their next `$?`. Kept across the whole run rather than
    // per hook, so `$?` means the same thing to each of several hooks
    // on one event.
    let status = session.shell.last_status();
    let captured = Rc::new(RefCell::new(String::new()));
    // Borrowed and given back, rather than set and assumed: an
    // unpromoted session writes to the real terminal, and "restoring" it
    // to a grid nobody paints makes every later line disappear. Found
    // exactly that way -- `::bish hook help` printed nothing once any
    // hook had fired.
    let saved = session.shell.borrow_sink(captured);
    for command in commands {
        // The path as one quoted word, so a name with a space in it
        // arrives as one argument rather than several.
        let line = format!("{command} {}", crate::exec::shell_quote(&path));
        // A hook that doesn't parse is a config error, and the middle of
        // a redraw is the worst possible place to report one -- it is
        // simply not run, the same tolerance the abbreviation table
        // already has for a bad expansion.
        if let Ok(toks) = Lexer::new(&line).tokenize()
            && let Ok(prog) = Parser::new(toks).parse_program()
        {
            session.shell.run_program(&prog);
        }
    }
    session.shell.set_firing_hooks(false);
    session.shell.set_last_status(status);
    session.shell.return_sink(saved);
}

// The window list as the `window` builtin sees it -- pushed into every
// session's Shell before a command runs, so `window ls` and `window
// select` are ordinary builtins reading a snapshot rather than actions
// the repl applies afterwards. That is the difference between
// `$(window ls)` capturing the listing and capturing nothing.
fn window_snapshot(app: &App) -> Vec<exec::WindowInfo> {
    let home = std::env::var("HOME").unwrap_or_default();
    app.windows
        .iter()
        .enumerate()
        .map(|(i, window)| exec::WindowInfo {
            id: window.id,
            name: window.name.clone(),
            cwd: app.sessions.get(&window.owning_session()).map(|s| prompt::display_path(&s.shell.cwd.to_string_lossy(), &home)).unwrap_or_default(),
            panes: window.panes.len(),
            current: i == app.current_window,
        })
        .collect()
}

// The visible text of one tab's own segment -- shared by render_tab_bar
// and tab_bar_regions so the rendered tab bar and its hit-test column
// ranges (see hit_test_click) can never drift apart from each other.
/// What one tab's label may take: its share of the bar, less what the
/// segment spends on the window id and the spaces around it, and less
/// one more so the bar ends before the line does rather than on it.
///
/// Its own function so the arithmetic can be read and tested without an
/// `App` to hand.
fn tab_label_budget(term_cols: usize, windows: usize) -> usize {
    let share = term_cols / windows.max(1);
    share.saturating_sub(tab_segment_text(0, "").chars().count() + 1)
}

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
    // A press landed on the strip between two panes -- the start of a
    // resize drag, not a focus change. Carries what
    // `resize_split_child_to` needs to act on every motion event that
    // follows without re-deriving the layout each time.
    Divider(Divider),
}

// How close together two presses on the same cell have to be to count as
// a double-click. Terminals report neither the double-click nor the
// timing, so this is bish's own judgement -- the same order most toolkits
// use, and deliberately paired with an exact same-cell test so a slow
// drag across two characters can never read as one.
const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);

// How long the pointer has to sit still before a hover popup appears.
//
// Also bish's own judgement, and the number that decides whether this
// feature is helpful or infuriating. Too short and a popup chases the
// pointer across the screen; too long and it feels broken. This is
// about what an editor with a mouse conventionally waits, and it is
// long enough that a sweep across a line asks a language server
// nothing at all -- the lookup only happens where the pointer stops.
const HOVER_DWELL: std::time::Duration = std::time::Duration::from_millis(350);

// `fileeditor::position_at_screen`'s counterpart for a `ScreenBuffer` --
// simpler because a read-only pane has neither a gutter nor horizontal
// scrolling (see Buffer::viewport_left's own doc comment), so a column
// maps straight through. Mirrors render_normal_mode_frame's own
// placement exactly, the same way that one mirrors build_editor_frame's.
fn screen_position_at(buf: &ScreenBuffer, rect: Rect, row0: usize, col0: usize) -> Option<(usize, usize)> {
    if row0 < rect.row || col0 < rect.col || col0 >= rect.col + rect.cols {
        return None;
    }
    let row = row0 - rect.row;
    if row >= normal_mode_content_rows(rect) {
        return None;
    }
    let line = buf.viewport_top() + row;
    if line >= buf.line_count() {
        return None;
    }
    Some((line, (col0 - rect.col).min(buf.line_len(line).saturating_sub(1))))
}

// Where a click inside the focused pane puts the cursor, for whichever
// kind of buffer normal mode is driving. `true` when it actually moved.
fn place_nav_cursor_at(buf: &mut NavBuffer, rect: Rect, row0: usize, col0: usize) -> bool {
    let pos = match buf {
        NavBuffer::ReadOnly(sb) => screen_position_at(sb, rect, row0, col0),
        NavBuffer::Editable(tb) => fileeditor::position_at_screen(tb, rect, row0, col0),
    };
    match pos {
        Some((line, col)) => {
            buf.set_cursor(line, col);
            true
        }
        None => false,
    }
}

// Hit-tests a click's screen coordinates against the tab bar (if any),
// then the current window's own pane layout -- shared by both places a
// click can be acted on (repl::run's plain-prompt loop, and
// run_normal_mode_navigation's own loop), so the two can't disagree on
// what a given click means. `ev.row`/`ev.col` are 1-indexed (the
// terminal's own convention -- see MouseEvent's own doc comment); `Rect`/
// the tab bar's own row are 0-indexed, hence the -1 conversions below.
fn hit_test_click(app: &App, ev: editor::MouseEvent) -> Option<ClickTarget> {
    let row0 = (ev.row as usize).saturating_sub(1);
    let col0 = (ev.col as usize).saturating_sub(1);
    if row0 == app.term_rows.saturating_sub(1) {
        let regions = tab_bar_regions(&tab_bar_snapshot(app));
        let (id, _, _) = regions.into_iter().find(|(_, start, end)| col0 >= *start && col0 < *end)?;
        return Some(ClickTarget::Window(app.windows.iter().position(|w| w.id == id)?));
    }
    let area = Rect { row: 0, col: 0, rows: content_rows(app.term_rows), cols: app.term_cols };
    let mut regions = Vec::new();
    let mut dividers = Vec::new();
    app.windows[app.current_window].regions(area, &mut regions, &mut dividers);
    // Dividers first: they sit *between* pane rectangles, so nothing here
    // overlaps, but checking them first keeps the intent obvious -- a
    // press on the strip is a resize, never a focus change.
    if let Some(divider) =
        dividers.into_iter().find(|d| row0 >= d.rect.row && row0 < d.rect.row + d.rect.rows && col0 >= d.rect.col && col0 < d.rect.col + d.rect.cols)
    {
        // A folded divider stands for several boundaries at once, so
        // there is no single one for a drag to move. Clicking it focuses
        // the first pane it is hiding instead, which unfolds the run
        // around it -- the "flip" in flipper divider.
        if divider.folded.is_some() {
            return folded_divider_pane(&app.windows[app.current_window], &divider).map(ClickTarget::Pane);
        }
        return Some(ClickTarget::Divider(divider));
    }
    let (pane_id, _) = regions.into_iter().find(|(_, r)| row0 >= r.row && row0 < r.row + r.rows && col0 >= r.col && col0 < r.col + r.cols)?;
    Some(ClickTarget::Pane(pane_id))
}

// Drives a divider drag to completion: reads mouse events directly until
// the button comes up, resizing on every motion that actually moves the
// boundary and repainting as it goes, so the split follows the pointer
// live rather than jumping once at the end.
//
// Runs its own read loop rather than threading drag state back through
// whichever caller started it: a drag is a self-contained gesture with a
// definite end (the release), and both entry points -- the plain prompt
// and normal mode -- would otherwise each need the same three-state
// machine carried across their own unrelated loops. Everything else
// arriving mid-drag is ignored, including keystrokes: the terminal is
// mid-gesture, and a stray key is far more likely to be the user
// fumbling than a command.
fn drag_divider(app: &mut App, divider: &Divider) {
    // Re-derived from the live layout on every motion: the first resize
    // moves the divider itself, so a `Divider` captured at press time is
    // stale from then on -- but its *identity* (path, child, axis) is
    // what's being dragged and doesn't move, so only the position needs
    // refreshing.
    let mut current = divider.clone();
    {
        // Raw mode *and* mouse reporting for the duration. The prompt
        // path arrives here after read_line has already dropped its own
        // guard (it returns the click and exits), so without this the
        // motion events would land in a cooked terminal and be echoed
        // into the next prompt as literal escape text -- which is exactly
        // what happened before this was here. Normal mode's own guard is
        // still live; a nested one just saves and restores the same raw
        // state.
        let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else { return };
        while let Ok(Some(key)) = editor::read_key_idle(&mut || {
            service_background_jobs(app);
        }) {
            let Key::Mouse(ev) = key else { continue };
            if ev.is_release() {
                break;
            }
            if !ev.is_left_drag() {
                continue;
            }
            let to = if current.horizontal { (ev.row as usize).saturating_sub(1) } else { (ev.col as usize).saturating_sub(1) };
            if !resize_split_child_to(&mut app.windows[app.current_window], &current, to) {
                continue;
            }
            // Refresh this divider's own geometry from the layout it just
            // changed, matched by identity rather than position.
            let area = Rect { row: 0, col: 0, rows: content_rows(app.term_rows), cols: app.term_cols };
            let mut regions = Vec::new();
            let mut dividers = Vec::new();
            app.windows[app.current_window].regions(area, &mut regions, &mut dividers);
            if let Some(fresh) = dividers.into_iter().find(|d| d.path == current.path && d.child == current.child) {
                current = fresh;
            }
            if app.sinks_are_grid {
                compositor_redraw(app);
            }
        }
    }
    // The guard above turned mouse reporting off on its way out, and
    // whoever called us was mid-gesture precisely because it was on --
    // put it back rather than leaving them blind to the next click.
    print!("{}", term::MOUSE_REPORTING_ENABLE);
    let _ = io::stdout().flush();
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

// `:help`/`:h`/`:?`'s own content -- written in markdown and rendered
// through crate::markdown (see `render_help`), which is the whole point
// of having that parser: the help is now a *document*, with headings,
// tables and emphasis, rather than hand-aligned columns of plain text
// that had to be re-aligned by hand whenever a line changed.
//
// Kept short enough to fit one ordinary terminal's worth of command-
// output overlay without truncating (see command_mode_content_rows) --
// a genuinely exhaustive reference would need real pagination/topics
// this feature doesn't attempt.
const EDITOR_HELP_MARKDOWN: &str = r#"# bish editor

## Moving around

| Keys | What |
|:--|:--|
| `h j k l` `w b e ge` `0 ^ $` `gg G` | motions |
| `f F t T ; ,` `%` `{ }` | find, match, paragraph |
| `/pat` `?pat` `n` `N` | search |
| `m{a-z}` `` `{mark} `` `'{mark}` | marks |
| `<C-o>` `<C-i>` | jump back / forward |
| `zf{motion}` `zF` `zo zc za` `zR zM` `zd zE` `zj zk` | folds — a closed fold is one line to `j`, `k` and `dd` |

## Changing things

`i I a A o O` enter Insert mode; `<Esc>` returns to Normal.
`v` `V` `<C-v>` start Visual (char, line, block); `o` swaps ends.

Operators take a motion, or double the key for the whole line:
`d` delete, `c` change, `y` yank, `>`/`<` indent, `gu`/`gU`/`g~` case.

- **Surround** — `ys{motion}{char}` add, `cs{old}{new}` change, `ds{char}` delete
- **Registers** — `"{reg}` before an operator or put, e.g. `"ayy` then `"ap`
- **Undo / redo** — `u` and `<C-r>`; put with `p`/`P`; repeat with `.`
- **Hover** — `K` shows a value, a doc comment, or a man page snippet

## Opening files

`e FILE...` opens several at once, the first in front. `e --hex FILE`
opens one as raw bytes instead (with its own `:help`). `e DIR` browses
and opens what you pick, and so does `e ARCHIVE.zip` — members open
read-only, and `:w FILE` extracts one. A gzip'd file opens read-only as
its decompressed text.

## Colon commands

| Command | What |
|:--|:--|
| `:w [FILE]` | write (`:wq`/`:x` write+quit, `:q`, `:q!`) |
| `:s/PAT/REPL/[g]` | substitute (prefix a range, e.g. `:%s/../../`) |
| `:git blame [REV]` | per-line blame gutter (`:git diff [REV]` for +/~/-) |
| `:git log [REV]` | this file's commits; Enter shows one |
| `:git show [REV]` | the commit (`HEAD`, or the cursor line's with blame on), opened at this file |
| `:diff` | +/~/- vs. what's on disk, no git needed |
| `:review [git [REV]]` | go through those changes one by one, keeping or putting back each |
| `:fold` | fold this line, or a range before it (`:foldopen[!]`, `:foldclose[!]`) |
| `:format` | run this file's own formatter |
| `:diag [clear]` | toggle the diagnostics pane |
| `:dbg` | attach a read-only debug session (`:dbg help`) |
| `:preview` | render this markdown buffer |
| `:!CMD` | run a shell command; a space before the `!` keeps it out of history |
| `:help` | this screen (`:help keys`, `:help options`, `:help hooks`) |
"#;

// `:help options`, as markdown: every bishopt with what it accepts, its
// default and its current value.
//
// Generated from the registry rather than written out, so an option
// added tomorrow is documented today -- the same reason `bishopt`'s own
// completion reads the registry instead of a copy. `Shell::describe_
// bishopts` does the formatting, so this page and `bishopt --describe`
// can never disagree about what an option is for.
fn options_help(shell: &exec::Shell) -> String {
    let mut out = String::from(
        "# bish options\n\nSet with `bishopt --set NAME VALUE`, read with `bishopt NAME`, cleared with `bishopt --unset NAME`.\nEverything here also works from the editor's own `:` line.\n\n",
    );
    let described = shell.describe_options(None);
    // describe_bishopts emits three lines per option: the name, the
    // description, then what it accepts.
    for block in described.chunks(3) {
        let [name, description, accepts] = block else { continue };
        out.push_str(&format!("## `{}`\n\n{}\n\n`{}`\n\n", name.trim(), description.trim(), accepts.trim()));
    }
    out
}

// `:help keys`, as markdown -- generated from `keymap::key_index`,
// which asks the editor itself what every key does. The same treatment
// `:help options` already gets from the option registry, and for the
// same reason: a page written out by hand goes stale silently. This one
// did -- the prose above has never mentioned `D`, and finding out that
// `D` deletes to end of line meant mapping a throwaway key to it and
// listing the mappings.
//
// `{motion}`, `{object}`, `{char}` and `{pattern}` in the key column
// are patterns rather than literal keys; see `key_index`.
fn keys_help() -> String {
    let mut out = String::from(
        "# bish editor keys\n\nNormal mode, as the editor itself reports it -- this page is generated from the\nkey table, not written beside it, so it cannot describe a key that isn't there or\nmiss one that is.\n\nA leading count repeats most of these (`3j`, `2dd`). `{motion}` stands for any\nmotion, `{object}` for any text object, `{char}` for one typed character, and\n`{pattern}` for text ending in Enter.\n\n| Keys | What |\n|:--|:--|\n",
    );
    for (keys, action) in crate::keymap::key_index() {
        // A `|` inside a table cell would end it, and `` ` `` inside
        // code spans needs a wider fence.
        let keys = keys.replace('|', "\\|");
        out.push_str(&format!("| ``{keys}`` | {action} |\n"));
    }
    out
}

#[cfg(test)]
mod keys_help_tests {
    // The written help has never mentioned `D`, and nothing could have
    // noticed -- which is the whole argument for generating this page
    // instead. So the test is that the generated page really does carry
    // every row, rather than quietly truncating or mangling some.
    #[test]
    fn the_generated_page_carries_every_row_of_the_index() {
        let page = super::keys_help();
        for (keys, action) in crate::keymap::key_index() {
            assert!(page.contains(&action), "{keys}: `{action}` is missing from :help keys");
            assert!(page.contains(&keys.replace('|', "\\|")), "{keys} is missing from :help keys");
        }
        assert!(page.contains("delete line-end"), "the miss that started this");
    }

    // The prose page is what points at the generated one, so it has to
    // say the name.
    #[test]
    fn the_written_help_points_at_the_generated_one() {
        assert!(super::EDITOR_HELP_MARKDOWN.contains(":help keys"));
    }
}

// `:help hooks`, as markdown -- the same event descriptions
// `::bish hook help` prints, so a page and a command can't disagree
// about when something fires.
fn hooks_help() -> String {
    let mut out = String::from(
        "# bish hooks\n\nAttach a command to something bish does. Registered with `::bish hook add`, listed with\n`::bish hook ls`, removed by the id `add` printed.\n\n```\n::bish hook add --lang=bash editor:file:open __my_bash_stuff\n```\n\nA hook runs its command with the event's own argument appended, and its output goes\nnowhere -- a hook fires while the editor owns the terminal. `--lang` is a glob over the\nfile's language, the same shape `abbr --lang` uses.\n\n## Events\n\n",
    );
    for (event, description) in crate::exec::HOOK_EVENT_HELP {
        out.push_str(&format!("### `{event}`\n\n{description}\n\n"));
    }
    out
}

// Shared by `:help` and `:preview` -- the same pipeline either way,
// which is what makes the help page a real markdown document rather
// than a special case. Rendered on demand rather than once: the width is
// the terminal's, and a table has to be laid out for the width it will
// actually occupy.
fn render_markdown_document(source: &str, term_cols: usize, render: &RenderOptions) -> Vec<String> {
    let doc = crate::markdown::parse(source);
    let opts = crate::markdown::render::Options {
        width: term_cols.saturating_sub(1).max(20),
        highlight_code: true,
        hyperlinks: render.hyperlinks,
        base_dir: render.base_dir.clone(),
        colors: render.colors.clone(),
        wrap_tables: render.wrap_tables,
    };
    crate::markdown::render::to_lines(&doc, &opts)
}

// `:preview`'s own dispatch: which languages have something to render,
// and how. `None` for a language that has no rendered form -- which is
// what the command reports rather than guessing at one.
fn preview_document(language: &str, source: &str, term_cols: usize, render: &RenderOptions) -> Option<Vec<String>> {
    let width = term_cols.saturating_sub(1).max(20);
    match language {
        "markdown" => Some(render_markdown_document(source, term_cols, render)),
        "roff" => {
            let doc = crate::roff::parse(source);
            Some(crate::roff::render::to_lines(&doc, &crate::roff::render::Options { width }))
        }
        _ => None,
    }
}

// `:help`/`:preview`'s own driving loop: a pager over already-rendered
// lines, taking over the terminal for its own duration and handing
// control straight back -- the same "blocks until it exits" shape
// `:dbg`'s own run already has, and for the same reason (a document
// someone is reading wants the whole screen, and wants it gone
// completely afterwards).
//
// The view lives in crate::pager; this is only the loop: read a key,
// hand it over, redraw. `service_background_jobs` runs as the idle
// callback exactly as it does in every other blocking loop here, so a
// background job's output still lands in its own pane's grid while a
// help page is open.
// What the pager is showing, so a resize can re-render it at the new
// width -- which is not optional, since a table's layout and every
// wrapped line depend on it.
// Everything a rendered document needs that it cannot go and read for
// itself: whether links are emitted as real terminal hyperlinks and
// what a relative one is relative to, this session's `ui_col_*`
// colours, and whether a wide table is fitted to the pane. Carried
// rather than read where it's needed, because neither the pager nor the
// markdown renderer has a Shell to ask or a buffer to look at.
#[derive(Debug, Clone)]
struct RenderOptions {
    hyperlinks: bool,
    base_dir: Option<std::path::PathBuf>,
    colors: Option<crate::theme::UiColors>,
    // The `table_wrap` bishopt. `true` in `Default`, matching both the
    // option's own default and the renderer's.
    wrap_tables: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions { hyperlinks: false, base_dir: None, colors: None, wrap_tables: true }
    }
}

struct PagerSource {
    language: String,
    source: String,
    render: RenderOptions,
}

impl PagerSource {
    fn lines(&self, term_cols: usize) -> Vec<String> {
        preview_document(&self.language, &self.source, term_cols, &self.render).unwrap_or_default()
    }
}

// The man page for the command word under the cursor, if this is a
// shell buffer and there is one.
//
// Scoped to bash on purpose: "the word under the cursor names a program
// on this machine" is only a safe reading where a word in command
// position *is* a program. In a Rust file it would be a coincidence.
//
// `manpages::source_for` reads the page synchronously rather than going
// through `query`'s background fetch -- this is answering a key someone
// just pressed, and there is nothing useful to show them a moment later.
fn man_page_under_cursor(buf: &TextBuffer) -> Option<(String, String)> {
    if fileeditor::language_of(buf) != crate::bishedit::snippet::DEFAULT_LANG {
        return None;
    }
    let (row, col) = buf.cursor();
    let line = buf.line_chars(row);
    let start = crate::bishedit::completion::find_word_start(&line, col.min(line.len()));
    let end = line[start..].iter().position(|c| !c.is_alphanumeric() && *c != '-' && *c != '_' && *c != '.').map_or(line.len(), |n| start + n);
    let word: String = line[start..end].iter().collect();
    if word.is_empty() {
        return None;
    }
    crate::bishedit::manpages::source_for(&word).map(|source| (word, source))
}

// Opens every review waiting in `pending_reviews`, one after another,
// and answers each client as soon as its review is decided. Says whether
// anything was opened: the caller's cue to repaint what a review covered,
// the same contract `service_background_jobs`'s own return has.
fn open_pending_reviews(app: &mut App) -> bool {
    if app.pending_reviews.is_empty() {
        return false;
    }
    for (request, caller) in std::mem::take(&mut app.pending_reviews) {
        let outcome = review_proposal(app, &request);
        session::reply_to(caller, &mcp::review_answer(request.id, &outcome));
    }
    compositor_redraw(app);
    true
}

// One proposed version of a file, gone through against the file as it is
// now. What was taken is written, and the answer carries what the file
// then holds; nothing taken, or the review walked away from, leaves the
// file alone and says so.
//
// A change nobody ruled on is not taken. The proposal is not in the file
// yet, so leaving a change undecided leaves it out -- `:review`'s own
// "nothing happens unless you say so", from the other side.
fn review_proposal(app: &mut App, request: &mcp::ReviewRequest) -> mcp::ReviewOutcome {
    let current = match std::fs::read(&request.path) {
        Ok(bytes) => crate::encoding::decode(&bytes).text,
        // Not there yet: empty, so the whole proposal is new.
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(_) => return mcp::ReviewOutcome::Rejected,
    };
    let old: Vec<String> = current.lines().map(str::to_string).collect();
    let new: Vec<String> = request.proposed.lines().map(str::to_string).collect();
    let title = match request.title.as_str() {
        "" => request.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "review".to_string()),
        title => title.to_string(),
    };
    let rect = app.focused_pane_rect();
    let language = fileeditor::language_of_path(&request.path);
    // In the colours of the session the review opens over.
    let colors = syntax_color_overrides(&app.sessions[&app.window().owning_session()].shell);
    let view = crate::review::Review::new(&title, old, new, &language, rect.rows, rect.cols).highlighted(Some(&colors));
    let Some(review) = run_review(app, view) else {
        return mcp::ReviewOutcome::Rejected;
    };
    if !review.accepted_any() {
        return mcp::ReviewOutcome::Rejected;
    }
    let lines = review.resolved(crate::review::Verdict::Rejected);
    let mut text = lines.join("\n");
    if !lines.is_empty() && (request.proposed.ends_with('\n') || (request.proposed.is_empty() && current.ends_with('\n'))) {
        text.push('\n');
    }
    if std::fs::write(&request.path, &text).is_err() {
        return mcp::ReviewOutcome::Rejected;
    }
    // An open buffer on the file with nothing unsaved of its own follows
    // it. One that has unsaved changes is left to say so itself -- see
    // `sync_watched_file` -- rather than having them thrown away here.
    for session in app.edit_frames.values_mut() {
        if session.buffer.path() == Some(request.path.as_path()) && !session.buffer.is_dirty() {
            let _ = session.buffer.reload();
        }
    }
    mcp::ReviewOutcome::Saved(text)
}

// `:git show REV` as a view to read: the commit's header, then every file
// it changed, each as it was in the parent beside what the commit made it.
// Opens at `path`'s own section when the commit touched that file. From
// the directory `path` is in, or the working directory without one.
fn git_show_view(
    path: Option<&Path>,
    rev: &str,
    colors: Option<&highlight::ColorOverrides>,
    rows: usize,
    cols: usize,
) -> Result<crate::review::Review, String> {
    let dir = match path.and_then(Path::parent).filter(|d| !d.as_os_str().is_empty()) {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let commit = crate::git::show(&dir, rev)?;
    let mut header = commit.header.clone();
    if commit.merge {
        header.push(String::new());
        header.push("(a merge: shown against its first parent)".to_string());
    }
    let here = path.and_then(|p| std::fs::canonicalize(p).ok());
    let mut start = None;
    let mut files = Vec::new();
    for changed in &commit.files {
        // A side the file is not on -- before it was added, after it was
        // deleted -- is empty.
        let side = |rev: Option<&str>, path: &Option<String>| match (rev, path) {
            (Some(rev), Some(path)) => crate::git::blob(&commit.root, rev, path),
            _ => Ok(Vec::new()),
        };
        let old = side(commit.parent.as_deref(), &changed.old_path)?;
        let new = side(Some(&commit.hash), &changed.new_path)?;
        let binary = looks_binary(&old) || looks_binary(&new);
        let text = |bytes: &[u8]| crate::encoding::decode(bytes).text.lines().map(str::to_string).collect::<Vec<_>>();
        let label = match (&changed.old_path, &changed.new_path) {
            (Some(old), Some(new)) if old != new => format!("{old} \u{2192} {new}"),
            (None, Some(new)) => format!("{new} (new)"),
            (Some(old), None) => format!("{old} (deleted)"),
            (Some(path), Some(_)) => path.clone(),
            (None, None) => continue,
        };
        let touches_here = [&changed.new_path, &changed.old_path]
            .into_iter()
            .flatten()
            .any(|p| here.is_some() && std::fs::canonicalize(commit.root.join(p)).ok() == here);
        if start.is_none() && touches_here {
            start = Some(files.len());
        }
        let language = changed
            .new_path
            .as_ref()
            .or(changed.old_path.as_ref())
            .map_or_else(|| "text".to_string(), |p| fileeditor::language_of_path(Path::new(p)));
        files.push(crate::review::FileDiff {
            language,
            label,
            old: if binary { Vec::new() } else { text(&old) },
            new: if binary { Vec::new() } else { text(&new) },
            binary,
        });
    }
    let short = &commit.hash[..commit.hash.len().min(12)];
    Ok(crate::review::Review::reading(&format!("git show {short}"), header, files, start, rows, cols).highlighted(colors))
}

// What git itself goes by: a NUL in the first 8000 bytes. A UTF-16 file
// is full of them and is still text, so its byte-order mark says so first.
fn looks_binary(bytes: &[u8]) -> bool {
    !bytes.starts_with(&[0xff, 0xfe]) && !bytes.starts_with(&[0xfe, 0xff]) && bytes[..bytes.len().min(8000)].contains(&0)
}

// `:review`'s loop -- `run_pager`'s arrangement, for a view that decides
// rather than reads. The review comes back when it was finished, so its
// verdicts can be acted on, and not when it was walked away from.
// `view` is made by the caller, sized to `focused_pane_rect`, so the same
// loop serves one that reads, too.
fn run_review(app: &mut App, mut view: crate::review::Review) -> Option<crate::review::Review> {
    let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else { return None };
    // What capture reports while this is open -- see `OVERLAYS`.
    let _overlay = OverlayOpen::new();
    let mut rect = app.focused_pane_rect();
    compositor_redraw(app);
    view.resize(rect.rows, rect.cols);
    let mut last_size = (app.term_rows, app.term_cols);
    let outcome = loop {
        let frame = view.render(rect);
        record_paint(rect, &frame);
        print!("{frame}");
        let _ = io::stdout().flush();
        let key = match editor::read_key_idle(&mut || {
            service_background_jobs(app);
        }) {
            Ok(Some(k)) => k,
            // EOF: there is nobody left to decide anything.
            Ok(None) | Err(_) => break crate::review::Outcome::Cancel,
        };
        if (app.term_rows, app.term_cols) != last_size {
            last_size = (app.term_rows, app.term_cols);
            rect = app.focused_pane_rect();
            compositor_redraw(app);
            view.resize(rect.rows, rect.cols);
        }
        match view.handle_key(key) {
            crate::review::Outcome::Continue => {}
            finished => break finished,
        }
    };
    print!("{}", term::MOUSE_REPORTING_ENABLE);
    let _ = io::stdout().flush();
    (outcome == crate::review::Outcome::Done).then_some(view)
}

// `:git log`'s loop: the list, and the diff view a commit opens in.
// The list lets go of the terminal while a commit is open -- the diff
// view takes it for itself -- and takes it back afterwards, with the
// same commit still picked out. What comes back is why a commit could
// not be opened, if one could not.
fn run_commit_list(
    app: &mut App,
    mut list: crate::commitlist::CommitList,
    open: impl Fn(&str, usize, usize) -> Result<crate::review::Review, String>,
) -> Option<String> {
    use crate::commitlist::Outcome;
    loop {
        let choice = {
            let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else { return None };
            // What capture reports while this is open -- see `OVERLAYS`.
            let _overlay = OverlayOpen::new();
            let mut rect = app.focused_pane_rect();
            compositor_redraw(app);
            list.resize(rect.rows, rect.cols);
            let mut last_size = (app.term_rows, app.term_cols);
            loop {
                let frame = list.render(rect);
                record_paint(rect, &frame);
                print!("{frame}");
                let _ = io::stdout().flush();
                let key = match editor::read_key_idle(&mut || {
                    service_background_jobs(app);
                }) {
                    Ok(Some(k)) => k,
                    Ok(None) | Err(_) => break Outcome::Close,
                };
                if (app.term_rows, app.term_cols) != last_size {
                    last_size = (app.term_rows, app.term_cols);
                    rect = app.focused_pane_rect();
                    compositor_redraw(app);
                    list.resize(rect.rows, rect.cols);
                }
                match list.handle_key(key) {
                    Outcome::Continue => {}
                    chosen => break chosen,
                }
            }
        };
        print!("{}", term::MOUSE_REPORTING_ENABLE);
        let _ = io::stdout().flush();
        let Outcome::Open(index) = choice else { return None };
        let rect = app.focused_pane_rect();
        match open(&list.entry(index).hash, rect.rows, rect.cols) {
            Ok(view) => {
                run_review(app, view);
            }
            Err(e) => return Some(e),
        }
    }
}

fn run_pager(app: &mut App, title: &str, doc: PagerSource) {
    let Ok(_guard) = term::RawGuard::enable_with_mouse(0) else { return };
    // What capture reports while this is open -- see `OVERLAYS`.
    let _overlay = OverlayOpen::new();
    // One pane's rectangle, not the whole screen. The neighbours are
    // painted once here and then left alone -- the pager only ever
    // writes inside its own rect, the same arrangement browser.rs and
    // the hex view already use.
    let mut rect = app.focused_pane_rect();
    compositor_redraw(app);
    let mut view = crate::pager::Pager::new(title, doc.lines(rect.cols), rect.rows, rect.cols);
    let mut last_size = (app.term_rows, app.term_cols);
    loop {
        let frame = view.render(rect);
        record_paint(rect, &frame);
        print!("{frame}");
        let _ = io::stdout().flush();
        let key = match editor::read_key_idle(&mut || {
            service_background_jobs(app);
        }) {
            Ok(Some(k)) => k,
            // EOF/error: nothing to resume, same as every other loop's
            // own EOF arm.
            Ok(None) | Err(_) => break,
        };
        // A resize while reading re-renders the *document*, not just the
        // view: the wrap width changed, and so did every table's layout.
        if (app.term_rows, app.term_cols) != last_size {
            last_size = (app.term_rows, app.term_cols);
            rect = app.focused_pane_rect();
            compositor_redraw(app);
            let (top_line, left_column) = (view.top_line(), view.left_column());
            view = crate::pager::Pager::new(title, doc.lines(rect.cols), rect.rows, rect.cols);
            view.scroll_to(top_line);
            view.scroll_columns_to(left_column);
        }
        match view.handle_key(key) {
            crate::pager::Outcome::Quit => break,
            // A click the pager has no use for. Landing on another tab
            // or pane, going there closes it -- the same rule a `<C-w>`
            // focus change already follows. Anything else (this pane, a
            // divider, a miss) keeps reading.
            crate::pager::Outcome::Click(ev) => match hit_test_click(app, ev) {
                Some(ClickTarget::Window(idx)) if idx != app.current_window => {
                    app.current_window = idx;
                    break;
                }
                Some(ClickTarget::Pane(pane)) if pane != app.windows[app.current_window].focused_pane => {
                    restore_if_minimized(&mut app.windows[app.current_window], pane);
                    app.windows[app.current_window].focused_pane = pane;
                    break;
                }
                _ => {}
            },
            crate::pager::Outcome::Continue => {}
        }
    }
    // The guard's own Drop turns mouse reporting off; whoever called
    // this still wants it (see every other nested-guard site here).
    print!("{}", term::MOUSE_REPORTING_ENABLE);
    let _ = io::stdout().flush();
}

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
pub(crate) fn command_mode_row(term_rows: usize) -> usize {
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
// Tabs, as the terminal will actually draw them: out to the next
// multiple of eight, counting only what is visible (an SGR sequence
// occupies no columns).
//
// A fixed-width bar cannot leave these alone. `visible_len` counts a
// tab as one column because a per-character width is all it can
// return, but a terminal draws it as up to eight -- so a line holding
// one is padded as though it were short, overflows `cols`, and wraps
// onto the row below. That row is the mode line, which is what made
// `::bish lsp status` (tab-separated, like every `ls` this shell has)
// look spliced into the file and spilling over the status bar.
// Expanding here rather than at each call site keeps the "this draws
// exactly `cols` columns" promise in the one place that makes it.
const TAB_STOP: usize = 8;

fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut column = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            out.push(c);
            for c2 in chars.by_ref() {
                out.push(c2);
                if c2 == 'm' {
                    break;
                }
            }
            continue;
        }
        if c == '\t' {
            let width = TAB_STOP - (column % TAB_STOP);
            out.push_str(&" ".repeat(width));
            column += width;
            continue;
        }
        out.push(c);
        column += crate::bishedit::unicode_width::char_width(c);
    }
    out
}

#[cfg(test)]
mod git_show_tests {
    use super::*;

    #[test]
    fn git_show_opens_at_the_file_being_edited_and_says_what_is_not_text() {
        if !crate::git::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-repl-git-show-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| crate::gittest::run(&dir, args);
        crate::gittest::init(&dir);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        std::fs::write(dir.join("c.bin"), b"\0\x01").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "all of it"]);

        let view = git_show_view(Some(&dir.join("b.txt")), "HEAD", None, 30, 60).unwrap();
        let frame = view.render(crate::window::Rect { row: 0, col: 0, rows: 30, cols: 60 });
        assert!(frame.contains("file 2 of 3: b.txt (new)"), "opened at b.txt: {frame:?}");
        assert!(frame.contains("binary file, not shown"), "{frame:?}");

        let from_nowhere = git_show_view(Some(&dir.join("elsewhere.txt")), "HEAD", None, 30, 60).unwrap();
        let frame = from_nowhere.render(crate::window::Rect { row: 0, col: 0, rows: 30, cols: 60 });
        assert!(frame.contains("all of it"), "an untouched file opens at the top, header first: {frame:?}");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod workspace_edit_tests {
    use super::*;
    use crate::lsp::{FileOperation, PositionEncoding, TextEdit, WorkspaceEdit};

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bish-workspace-edit-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uri(path: &Path) -> String {
        crate::url::from_file_path(path)
    }

    fn replace(line: usize, from: usize, to: usize, text: &str) -> TextEdit {
        TextEdit { start: (line, from), end: (line, to), text: text.to_string() }
    }

    // rust-analyzer's own shape for renaming a module that has a file of
    // its own: the file moves, then it and whatever names it are edited.
    #[test]
    fn a_file_is_renamed_then_edited_by_its_new_name_and_its_open_buffer_follows() {
        let dir = scratch("rename");
        std::fs::write(dir.join("lib.rs"), "mod old;\n").unwrap();
        std::fs::write(dir.join("old.rs"), "fn a() {}\n").unwrap();
        let mut buf = TextBuffer::open(&dir.join("old.rs"), 10).unwrap();
        let edit = WorkspaceEdit {
            changes: vec![(uri(&dir.join("new.rs")), vec![replace(0, 3, 4, "b")]), (uri(&dir.join("lib.rs")), vec![replace(0, 4, 7, "new")])],
            operations: vec![(
                0,
                FileOperation::Rename {
                    old_uri: uri(&dir.join("old.rs")),
                    new_uri: uri(&dir.join("new.rs")),
                    overwrite: false,
                    ignore_if_exists: false,
                },
            )],
            unsupported: Vec::new(),
        };
        let mut followed = Vec::new();
        let counts = apply_workspace_edit(&mut HashMap::new(), &mut buf, &edit, PositionEncoding::Utf16, &mut |b: &mut TextBuffer, to: &Path| {
            followed.push(to.to_path_buf());
            b.set_path(to);
        })
        .unwrap();
        assert_eq!(counts, (1, 1), "lib.rs written; the open buffer edited and left for :w");
        assert_eq!(std::fs::read_to_string(dir.join("lib.rs")).unwrap(), "mod new;\n");
        assert!(!dir.join("old.rs").exists());
        assert_eq!(std::fs::read_to_string(dir.join("new.rs")).unwrap(), "fn a() {}\n", "on disk it is what was saved; the edit is in the buffer");
        assert_eq!(followed, [dir.join("new.rs")]);
        assert_eq!(buf.path(), Some(dir.join("new.rs").as_path()));
        assert_eq!(fileeditor::buffer_text(&buf).lines().next(), Some("fn b() {}"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_entry_for_one_file_lands_in_order_whether_it_is_open_or_not() {
        let dir = scratch("entries");
        std::fs::write(dir.join("closed.rs"), "one\n").unwrap();
        std::fs::write(dir.join("open.rs"), "one\n").unwrap();
        let mut buf = TextBuffer::open(&dir.join("open.rs"), 10).unwrap();
        // The second entry's position is in the text the first one left:
        // ` three` goes after the ` two` the first entry put there.
        let entries = |path: &Path| vec![(uri(path), vec![replace(0, 3, 3, " two")]), (uri(path), vec![replace(0, 7, 7, " three")])];
        let edit = WorkspaceEdit {
            changes: [entries(&dir.join("closed.rs")), entries(&dir.join("open.rs"))].concat(),
            operations: Vec::new(),
            unsupported: Vec::new(),
        };
        let counts =
            apply_workspace_edit(&mut HashMap::new(), &mut buf, &edit, PositionEncoding::Utf16, &mut |_: &mut TextBuffer, _: &Path| {}).unwrap();
        assert_eq!(counts, (1, 1), "a file written and a buffer changed, however many entries each had");
        assert_eq!(std::fs::read_to_string(dir.join("closed.rs")).unwrap(), "one two three\n");
        assert_eq!(fileeditor::buffer_text(&buf).lines().next(), Some("one two three"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_cannot_be_read_takes_back_the_rename_before_it() {
        let dir = scratch("undo");
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        std::fs::create_dir(dir.join("not-a-file")).unwrap();
        let edit = WorkspaceEdit {
            changes: vec![(uri(&dir.join("not-a-file")), vec![replace(0, 0, 0, "x")])],
            operations: vec![(
                0,
                FileOperation::Rename {
                    old_uri: uri(&dir.join("a.txt")),
                    new_uri: uri(&dir.join("b.txt")),
                    overwrite: false,
                    ignore_if_exists: false,
                },
            )],
            unsupported: Vec::new(),
        };
        let why = apply_workspace_edit(
            &mut HashMap::new(),
            &mut TextBuffer::new_unnamed(10),
            &edit,
            PositionEncoding::Utf16,
            &mut |_: &mut TextBuffer, _: &Path| {},
        )
        .unwrap_err();
        assert!(why.ends_with("nothing changed"), "{why}");
        assert!(dir.join("a.txt").exists() && !dir.join("b.txt").exists(), "the rename is taken back");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_operation_that_cannot_be_done_is_refused_before_any_text_changes() {
        let dir = scratch("refused");
        std::fs::write(dir.join("lib.rs"), "mod old;\n").unwrap();
        let edit = WorkspaceEdit {
            changes: vec![(uri(&dir.join("lib.rs")), vec![replace(0, 4, 7, "new")])],
            operations: vec![(
                1,
                FileOperation::Rename {
                    old_uri: uri(&dir.join("missing.rs")),
                    new_uri: uri(&dir.join("new.rs")),
                    overwrite: false,
                    ignore_if_exists: false,
                },
            )],
            unsupported: Vec::new(),
        };
        let why = apply_workspace_edit(
            &mut HashMap::new(),
            &mut TextBuffer::new_unnamed(10),
            &edit,
            PositionEncoding::Utf16,
            &mut |_: &mut TextBuffer, _: &Path| {},
        )
        .unwrap_err();
        assert_eq!(why, format!("{} does not exist -- nothing changed", dir.join("missing.rs").display()));
        assert_eq!(std::fs::read_to_string(dir.join("lib.rs")).unwrap(), "mod old;\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod overlay_tab_tests {
    use super::*;

    // The bug this exists for: a tab-separated line (every `ls` in this
    // shell prints one) was padded as though each tab were one column,
    // overflowed the row, and wrapped onto the mode line -- which is
    // what made `::bish lsp status` look spliced into the file.
    #[test]
    fn a_tabbed_line_still_draws_exactly_one_row_wide() {
        let row = "1\tready\tutf-32\t1\t/home/jussi/bish\trust-analyzer";
        let drawn = styled_full_width_line(row, OUTPUT_BG, OUTPUT_FG, 80);
        assert_eq!(editor::visible_len(&drawn), 80, "one row, no more and no less");
        assert!(!drawn.contains('\t'), "a tab left in would be re-expanded by the terminal");
    }

    #[test]
    fn tabs_advance_to_the_next_multiple_of_eight() {
        assert_eq!(expand_tabs("a\tb"), "a       b");
        assert_eq!(expand_tabs("abcdefg\tb"), "abcdefg b");
        // Exactly on a stop still advances a whole one, as a terminal does.
        assert_eq!(expand_tabs("abcdefgh\tb"), "abcdefgh        b");
        // A line with none is handed back untouched.
        assert_eq!(expand_tabs("plain"), "plain");
        // SGR occupies no columns, so it must not shift the stops.
        assert_eq!(expand_tabs("\x1b[1ma\x1b[0m\tb"), "\x1b[1ma\x1b[0m       b");
    }

    // Long output is truncated rather than wrapped, for the same reason.
    #[test]
    fn a_line_wider_than_the_row_is_cut_to_it() {
        let wide = "x".repeat(50) + "\t" + &"y".repeat(50);
        assert_eq!(editor::visible_len(&styled_full_width_line(&wide, OUTPUT_BG, OUTPUT_FG, 40)), 40);
    }
}

fn styled_full_width_line(text: &str, bg: &str, fg: &str, cols: usize) -> String {
    let text = &expand_tabs(text);
    let mut out = String::new();
    out.push_str(bg);
    out.push_str(fg);
    // Measured by what the line *draws*, not by how many chars it holds:
    // command output may contain its own SGR sequences (a rendered
    // markdown document does -- see render_markdown_document), and
    // counting those as text would truncate a styled line at a fraction
    // of its real width and mis-pad the rest.
    let len = editor::visible_len(text);
    if len >= cols {
        out.push_str(&truncate_to_width(text, cols));
    } else {
        out.push_str(text);
        out.push_str(&" ".repeat(cols - len));
    }
    out.push_str("\x1b[0m");
    out
}

// The first `cols` *columns* of `text`, keeping every SGR sequence it
// passes (dropping one would leave the rest of the line in whatever
// style happened to be active).
fn truncate_to_width(text: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            out.push(c);
            out.push(chars.next().expect("just peeked"));
            for c2 in chars.by_ref() {
                out.push(c2);
                if c2 == 'm' {
                    break;
                }
            }
            continue;
        }
        let w = crate::bishedit::unicode_width::char_width(c);
        if width + w > cols {
            break;
        }
        width += w;
        out.push(c);
    }
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
    Ran {
        output: String,
        status: i32,
    },
    // `:noh`/`:nohlsearch` -- stop drawing the current search's matches.
    //
    // Handed back rather than done here for the reason `Symbols` is:
    // the flag lives on `VimKeys`, which command mode does not have and
    // the Normal-mode loop that owns the search does.
    NoHighlight,
    // `:normal {keys}` -- run `keys` as if they had been typed in
    // Normal mode.
    //
    // Handed back for the same reason `NoHighlight` is: the keys go
    // through `VimKeys`, which command mode does not have.
    RunNormal(String),
    /// `:s/.../.../c` -- the range has to be walked one match at a time
    /// with the pane redrawn between each, which needs the caller's own
    /// rect and `VimKeys`. Same reason `RunNormal` is handed back rather
    /// than run here.
    ConfirmSubstitute(Box<SubstituteCmd>),
    // `:sym QUERY` -- ask the language server for matching symbols
    // across the whole project. Handed back rather than done here
    // because the answer goes into the locations pane, and command mode
    // has neither the pane's id nor the map it lives in; the Normal-mode
    // loop that owns `gO`'s pane has both.
    Symbols(String),
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
// The editor's own colon-line vocabulary: the command names the match
// below answers to that are the editor's rather than the shell's.
//
// A copy of what that match spells out, and the only honest way to have
// one -- a `match` arm is not a registry that can be read back, the way
// `bishopt`'s own completion reads the option registry. What keeps the
// copy honest is a test: every `:name` the help screen documents has to
// appear here, which is exactly the gap that let `:preview` be a
// documented command the colon line painted as an error.
//
// Two things need it, and would each have grown a list of their own
// otherwise. The highlighter, so a real command is not shown red for
// being absent from PATH -- it *is* absent from PATH, and so is every
// other command on this line. And completion, so `:prev<Tab>` finishes
// the word, which is the other half of the same question.
pub(crate) const COLON_COMMANDS: &[&str] = &[
    "dbg",
    "debug",
    "diag",
    "diagnose",
    "diff",
    "e!",
    "edit!",
    "fmt",
    "fold",
    "foldclose",
    "foldopen",
    "format",
    "git",
    "h",
    "help",
    "noh",
    "nohl",
    "nohlsearch",
    "norm",
    "normal",
    "prev",
    "preview",
    "q",
    "q!",
    "rename",
    "review",
    "rn",
    "sym",
    "symbols",
    "w",
    "w!",
    "wq",
    "write",
    "write!",
    "x",
];

fn run_command_mode(
    app: &mut App,
    session_id: SessionId,
    // `Some` iff the pane driving this call is a `Frame::Edit` (the
    // unified Normal-mode loop's own `NavBuffer::Editable` -- see that
    // enum's doc comment) -- what makes `w`/`w <path>`/`wq`/`x`/`q`/`q!`/
    // `diag`/`diag clear` mean anything here at all: they're the file's
    // own save/quit/diagnose commands, not general window-management
    // ones, so they only exist when there's an actual buffer for them to
    // act on.
    editing: Option<&mut TextBuffer>,
    // Seeds the very first prompt with already-typed text, cursor at its
    // end -- Ctrl+G mid-typing (below) uses this to carry the
    // in-progress line into command mode's own next prompt rather than
    // losing it. `None` (every other call site) starts with the ordinary
    // empty buffer.
    seed: Option<String>,
    // See editor::read_line's own `queued` parameter: a mapping whose
    // right-hand side opened this colon line has more keys to deliver.
    mut queued: Vec<Key>,
    // See handle_command_mode's own parameter of the same name.
    last_search: &str,
) -> CommandModeOutcome {
    let mut editing = editing;
    let mut buffer = String::new();
    // The colon line's own copy of the prompt's rule (see
    // starts_off_the_record). `: !make` is remembered; `:` space `!make`
    // runs and is forgotten -- and the space costs the bang nothing,
    // since history::expand trims before it looks for one.
    let mut buffer_unrecorded = false;
    let mut transcript_visible = false;
    // Set from `seed` on the very first iteration, or by Ctrl+G
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
        // &mut app.term_rows put it.
        let prompt_row = command_mode_row(app.term_rows) + 1;
        let prompt_str = if buffer.is_empty() { prompt::command_mode_prompt() } else { prompt::continuation() };
        let mouse = app.sessions.get(&session_id).is_none_or(|s| s.shell.bishopt_bool("mouse"));
        let builtin_completion = crate::bishedit::completion::BuiltinCompletionProvider {
            hl_names: app.sessions.get(&session_id).map(|s| s.shell.hl_colors().into_iter().map(|(name, _)| name).collect()).unwrap_or_default(),
            commands: COLON_COMMANDS,
        };
        print!("\x1b[{};1H", prompt_row);
        let _ = io::stdout().flush();

        // esc_cancels: true -- like a vim ':' command line, Esc (and an
        // empty-buffer Backspace) should back out of command mode the
        // same as Ctrl-C, regardless of what's been typed. ctrl_l_
        // reports: true -- command mode gives Ctrl-L its own meaning
        // (toggling the transcript view, below) rather than the
        // ordinary shell prompt's "clear the real screen."
        // The highlight context is `default()` but for one field: no
        // single clearly-current session at this call site, and command
        // mode types window-management subcommands, not shell command
        // lines. The exception is `line_commands` -- this line answers to
        // the editor's own commands as well as the shell's, and without
        // them every one of those shows red for not being on PATH.
        // cwd being None skips file/dir Link detection entirely;
        // known_functions being None doesn't skip command-validity
        // checking (that still runs against builtins/PATH), it just can't
        // recognize a session-specific function as valid there -- a minor,
        // accepted gap given what command mode is actually used for.
        // Flag/subcommand/printf highlighting need neither field, so those
        // work exactly as they do at the ordinary prompt.
        // No meaningful *shell* completion context here, same reasoning
        // as HighlightContext::default() above -- window-management
        // subcommands, not shell command lines. What it does get is
        // BuiltinCompletionProvider: the builtins whose arguments bish
        // itself defines need no session, cwd or PATH to complete, and
        // `:bishopt --set <Tab>` is worth as much here as at the prompt
        // -- more, since changing a view option is something you do
        // *while* looking at the buffer it changes. Which is also why it
        // gets the one snapshot it cannot do without: `::bish hl`'s
        // namespace is open, so what is *set* is knowable only from the
        // session. It also gets `COLON_COMMANDS`, the other half of the
        // same fix the highlight context above gets them for: a command
        // that cannot be completed is one you have to have memorised.
        // menu_capable: false
        // -- this colon line has no row of its own to draw a menu into,
        // so Tab cycles by splicing, which is what a one-line prompt
        // wants anyway.
        // Snapshotted before the call: the `on_idle` closure below takes
        // `app.sessions` mutably for the whole of it, so this cannot be read
        // inline as another argument.
        let command_mappings = app.sessions.get(&session_id).map(|s| s.shell.mappings.clone()).unwrap_or_default();
        // O(1) Rc-clone, for the same reason -- see `History`'s own doc
        // comment, which names this exact situation.
        let history_snapshot = app.cmd_history.clone();
        let outcome = app.with_registers(|app, registers| {
            editor::read_line(
                &prompt_str,
                &prompt_str,
                &history_snapshot,
                true,
                true,
                pending_initial.take(),
                0,
                app.term_cols,
                // Not `default()` outright: this line's commands are the
                // editor's own, and nothing else about the context is
                // knowable here (see the note above on why).
                HighlightContext { line_commands: Some(COLON_COMMANDS), ..HighlightContext::default() },
                Some(&builtin_completion),
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
                // Command mode's own colon line sits on the global status
                // row with an output overlay of its own that no
                // compositor_redraw knows how to put back, so it never asks
                // for one -- whatever arrived shows up once this excursion
                // ends and the caller repaints.
                &mut || {
                    service_background_jobs(app);
                    false
                },
                // The `mouse` bishopt -- off keeps the terminal's own
                // selection working here (see enable_maybe_mouse).
                mouse,
                // This colon line *is* command mode, so `::bish map -m
                // command` applies to it.
                command_mappings,
                // ...and if a mapping's `:` is what opened it, the rest of
                // that right-hand side arrives here.
                std::mem::take(&mut queued),
            )
        });
        match outcome {
            // Command mode discards whatever was typed either way, so
            // `text` is nothing to it -- see that field's own doc comment
            // for the caller that does need it.
            Ok(ReadOutcome::Eof) | Ok(ReadOutcome::Interrupted { .. }) => return CommandModeOutcome::Cancelled,
            // Directory navigation doesn't mean much inside command
            // mode's own restricted context -- just ignore it and keep
            // showing this same prompt, rather than wiring dir_history
            // all the way through here too.
            Ok(ReadOutcome::DirNav(_)) => {}
            // Entering bishedit normal mode from inside command mode
            // isn't a thing (you're already navigating a screen, not a
            // live prompt) -- but unlike DirNav, this can no longer just
            // be ignored outright: Ctrl+G is unconditional now (see
            // its own doc comment), so simply dropping `text`/`cursor`
            // here would silently discard whatever had already been
            // typed the instant this fired mid-line. Feeding it back as
            // the next read_line call's own `initial` keeps that text
            // right where it was -- the same "don't lose in-progress
            // typing" guarantee every other caller of this gets.
            // `wheel` is ignored for the same reason: a colon line is
            // already inside the scrollback view, so a notch has no
            // second view to open.
            Ok(ReadOutcome::NormalMode { text, cursor, wheel: _ }) => {
                pending_initial = Some((text, cursor));
            }
            // And the same for Ctrl+T: the opener's whole vocabulary is
            // "go to this pane" or "open this in a window of its own",
            // and a colon line is in the middle of saying something about
            // the pane it is already in. Whatever was typed stays put.
            Ok(ReadOutcome::Opener { text, cursor }) => {
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
            // Command mode stitches multi-line input the same way the
            // shell prompt does, so a paste's finished lines join the
            // buffer here and the unfinished one goes back to the
            // prompt. Nothing runs until the line that completes it is
            // submitted, which is the whole point.
            Ok(ReadOutcome::Pasted { lines, rest, cursor }) => {
                for line in lines {
                    if !buffer.is_empty() {
                        buffer.push('\n');
                    }
                    buffer.push_str(&line);
                }
                pending_initial = Some((rest, cursor));
            }
            Ok(ReadOutcome::CtrlL) => {
                transcript_visible = !transcript_visible;
                if transcript_visible {
                    render_command_transcript(&app.sessions[&session_id].command_transcript, app.term_rows, app.term_cols);
                } else {
                    compositor_redraw(app);
                }
            }
            Ok(ReadOutcome::Line(line)) => {
                // Same app.cmd_history expansion as the normal shell prompt (see
                // history::expand's own doc comment), scoped the same
                // way: only the first line of a fresh command, against
                // command mode's own separate app.cmd_history. Command mode
                // disallows subshells outright (command_mode_violation
                // below), so an unrecognized leading `!` becomes
                // `command <rest>` here instead of `(<rest>)` -- the
                // same "force it to run as an external" escape hatch
                // command mode already has, not a second one.
                if buffer.is_empty() {
                    buffer_unrecorded = starts_off_the_record(&line);
                }
                let line = if buffer.is_empty() {
                    match history::expand(&line, &app.cmd_history) {
                        Ok(history::Expansion::Substituted(s)) => s,
                        Ok(history::Expansion::UnrecognizedBang(rest)) => format!("command {}", rest),
                        Err(msg) => {
                            show_command_mode_error(&msg, app.term_rows, app.term_cols);
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
                app.registers.set_last_ex_command(trimmed.clone());

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
                    if let Some(parsed) = parse_global_command(&trimmed) {
                        match parsed.and_then(|cmd| run_global(tb, &cmd)) {
                            Ok(lines) => {
                                let output = format!("{lines} line{}", if lines == 1 { "" } else { "s" });
                                app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                    command: trimmed,
                                    output: output.clone(),
                                    status: 0,
                                });
                                return CommandModeOutcome::Ran { output, status: 0 };
                            }
                            Err(e) => {
                                show_command_mode_error(&format!("bish: {e}"), app.term_rows, app.term_cols);
                                buffer.clear();
                                continue;
                            }
                        }
                    }

                    // `:!cmd` -- vim's "run this through the shell".
                    // Nearly free here, since this colon line already
                    // ends in the shell (see the `run_program` call at
                    // the bottom of this function): the `!` says the
                    // rest wants vim's `%` expansion rather than the
                    // shell's own reading of it, and then hands the
                    // remainder straight down. Without this the `!` went
                    // to the shell attached to the command name, and
                    // `:!echo %` looked for a command called `!echo`.
                    match parse_bang_command(&trimmed) {
                        Some(BangCommand::Filter { from, to, cmd }) => match filter_lines_through_command(app, session_id, tb, from, to, cmd) {
                            Ok(output) => {
                                app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                    command: trimmed,
                                    output: output.clone(),
                                    status: 0,
                                });
                                return CommandModeOutcome::Ran { output, status: 0 };
                            }
                            Err(e) => {
                                show_command_mode_error(&format!("bish: {e}"), app.term_rows, app.term_cols);
                                buffer.clear();
                                continue;
                            }
                        },
                        Some(BangCommand::Run(rest)) => {
                            // History gets what was typed, not what it
                            // expanded to: `!ls %` recalled as `ls
                            // sample.txt` would name whichever file
                            // happened to be open when it ran, in a
                            // buffer that has since moved on.
                            if !buffer_unrecorded {
                                app.cmd_history.record(&trimmed, None);
                            }
                            match run_bang_command(app, session_id, tb, rest) {
                                Ok(output) => {
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: output.clone(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                Err(e) => {
                                    show_command_mode_error(&format!("bish: {e}"), app.term_rows, app.term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            }
                        }
                        None => {}
                    }

                    if let Some(cmd) = parse_line_command(&trimmed) {
                        if let LineCommand::Normal(keys) = cmd {
                            return CommandModeOutcome::RunNormal(keys);
                        }
                        match run_line_command(tb, &cmd) {
                            Ok(()) => {
                                app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                    command: trimmed,
                                    output: String::new(),
                                    status: 0,
                                });
                                return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                            }
                            Err(e) => {
                                show_command_mode_error(&format!("bish: {e}"), app.term_rows, app.term_cols);
                                buffer.clear();
                                continue;
                            }
                        }
                    }

                    // `:s`, `:&`, `:&&`, `:~` -- "that again". Checked
                    // first, and deliberately narrow enough not to
                    // shadow a real `:s/a/b/` (see
                    // `parse_repeat_substitute`).
                    let repeat = parse_repeat_substitute(&trimmed).map(|parsed| {
                        parsed.and_then(|(from, to, kind, flags)| repeat_substitute(app.last_substitute.as_ref(), last_search, from, to, kind, flags))
                    });
                    // Either shape arrives here as the same thing: a
                    // `SubstituteCmd`, or the reason there isn't one.
                    if let Some(parsed) = repeat.or_else(|| parse_substitute_command(&trimmed)) {
                        // A no-op for a repeat, which arrives with its
                        // pattern and flags already decided -- the two
                        // reach back for different things and
                        // `repeat_substitute` documents why.
                        let resolved = parsed.and_then(|cmd| resolve_substitute(cmd, app.last_substitute.as_ref(), last_search));
                        // `c` cannot run here -- see the outcome's own
                        // doc comment. Recorded first, so the `:s//x/`
                        // after it still knows the pattern.
                        if let Ok(cmd) = &resolved
                            && cmd.confirm
                        {
                            app.last_substitute = Some(LastSubstitute { cmd: cmd.clone(), search_then: last_search.to_string() });
                            return CommandModeOutcome::ConfirmSubstitute(Box::new(cmd.clone()));
                        }
                        let ran = resolved.and_then(|cmd| run_substitute(tb, &cmd).map(|out| (cmd, out)));
                        match ran {
                            Ok((cmd, out)) => {
                                // Remembered *after* resolving, so a
                                // later `:s//x/` sees the pattern this
                                // one actually used rather than the
                                // empty one it was written with.
                                app.last_substitute = Some(LastSubstitute { cmd: cmd.clone(), search_then: last_search.to_string() });
                                let (subs, lines) = (out.substitutions, out.lines);
                                // `n` changed nothing, so saying it made
                                // substitutions would be a lie.
                                let what = if cmd.count_only { "match" } else { "substitution" };
                                let plural = if subs == 1 {
                                    ""
                                } else if cmd.count_only {
                                    "es"
                                } else {
                                    "s"
                                };
                                let mut output = format!("{subs} {what}{plural} on {lines} line{}", if lines == 1 { "" } else { "s" });
                                // `p`/`#`/`l` show the line the command
                                // finished on, in place of the tally --
                                // that is what was asked for.
                                if let (Some(style), Some(row)) = (cmd.print, out.last_row) {
                                    output = printed_line(tb, row, style);
                                }
                                app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                    command: trimmed,
                                    output: output.clone(),
                                    status: 0,
                                });
                                return CommandModeOutcome::Ran { output, status: 0 };
                            }
                            Err(e) => {
                                show_command_mode_error(&format!("bish: {e}"), app.term_rows, app.term_cols);
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
                        // The file was rewritten under us since it was
                        // read -- by a formatter, a branch switch,
                        // another pane. vim's own E13, and it matters
                        // more here than in a short-lived editor: a
                        // detachable session can hold a buffer open for
                        // days.
                        //
                        // Content, not mtime (see `changed_on_disk`), so
                        // a `touch` or a formatter that changed nothing
                        // does not stand in the way. `!` overrides, and
                        // `:e!` is how you take the other side instead.
                        "w" | "write" | "wq" | "x" if arg.is_none() && tb.changed_on_disk() => {
                            show_command_mode_error(
                                "bish: E13: file changed on disk since it was read (`:w!` to overwrite, `:e!` to reload)",
                                app.term_rows,
                                app.term_cols,
                            );
                            buffer.clear();
                            continue;
                        }
                        // `e!`: throw away what is in the buffer and read
                        // the file again. Bang required, always -- unlike
                        // `:q`, there is no unambiguous non-destructive
                        // reading of "reload", so this never guesses.
                        "e!" | "edit!" => match tb.reload() {
                            Ok(()) => {
                                app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                    command: trimmed,
                                    output: String::new(),
                                    status: 0,
                                });
                                return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                            }
                            Err(e) => {
                                show_command_mode_error(&format!("bish: e!: {e}"), app.term_rows, app.term_cols);
                                buffer.clear();
                                continue;
                            }
                        },
                        "w!" | "write!" => {
                            run_hooks(&mut app.sessions, session_id, "editor:file:write:pre", tb);
                            fileeditor::run_pre_save_hooks(tb);
                            match tb.save(arg.map(std::path::Path::new)) {
                                Ok(()) => {
                                    fileeditor::set_last_filename(tb, &mut app.registers);
                                    run_hooks(&mut app.sessions, session_id, "editor:file:write:post", tb);
                                    save_language_server_document(&mut app.sessions, session_id, tb);
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: String::new(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                                }
                                Err(e) => {
                                    show_command_mode_error(&write_error(tb, &e), app.term_rows, app.term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            }
                        }
                        "w" | "write" => {
                            run_hooks(&mut app.sessions, session_id, "editor:file:write:pre", tb);
                            fileeditor::run_pre_save_hooks(tb);
                            match tb.save(arg.map(std::path::Path::new)) {
                                Ok(()) => {
                                    fileeditor::set_last_filename(tb, &mut app.registers);
                                    // After the bytes are really on
                                    // disk, so a hook that formats or
                                    // reloads is looking at what was
                                    // written rather than what was
                                    // about to be.
                                    run_hooks(&mut app.sessions, session_id, "editor:file:write:post", tb);
                                    save_language_server_document(&mut app.sessions, session_id, tb);
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: String::new(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                                }
                                Err(e) => {
                                    show_command_mode_error(&write_error(tb, &e), app.term_rows, app.term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            }
                        }
                        "wq" | "x" => {
                            fileeditor::run_pre_save_hooks(tb);
                            match tb.save(arg.map(std::path::Path::new)) {
                                Ok(()) => {
                                    fileeditor::set_last_filename(tb, &mut app.registers);
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: String::new(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Quit;
                                }
                                Err(e) => {
                                    show_command_mode_error(&write_error(tb, &e), app.term_rows, app.term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            }
                        }
                        "q" if tb.is_dirty() => {
                            show_command_mode_error("bish: E37: No write since last change (add ! to override)", app.term_rows, app.term_cols);
                            buffer.clear();
                            continue;
                        }
                        "q" | "q!" => {
                            app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
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
                            // Replaces bish's own findings and leaves
                            // a language server's alone -- the mirror
                            // of what a publication does (see
                            // apply_language_server_diagnostics). Both
                            // write to this one list precisely so the
                            // gutter, the underlines and the pane need
                            // to know nothing about where a finding
                            // came from, and that only works if each
                            // source replaces exactly its own.
                            // Diagnostics are pushed, never requested,
                            // so after `:diag clear` the only copy of a
                            // server's findings is the one it holds.
                            // Asking for them again is what makes
                            // `clear` reversible rather than permanent
                            // until the next edit -- which for a file
                            // nobody goes on to touch is never.
                            redeliver_language_server_diagnostics(&mut app.sessions, session_id, tb);
                            let mut merged = fileeditor::diagnose_buffer(tb);
                            merged.extend(tb.diagnostics.iter().filter(|d| d.source.is_some()).cloned());
                            crate::sort::sort_by_key(&mut merged, |d| (d.start, d.end));
                            tb.diagnostics = merged;
                            let n = tb.diagnostics.len();
                            let output = if n == 0 {
                                "No problems found.".to_string()
                            } else {
                                format!("{n} problem{} found.", if n == 1 { "" } else { "s" })
                            };
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
                            if let Some(Frame::Edit(edit_frame_id)) = app.windows[app.current_window].stack().last().copied() {
                                sync_diagnostics_pane(app, edit_frame_id, &tb.diagnostics);
                            }
                            app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                command: trimmed,
                                output,
                                status: 0,
                            });
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
                            // `clear` really does mean all of them: a
                            // server's findings would come back on its
                            // next publication anyway, and a `clear`
                            // that visibly left some behind would read
                            // as broken.
                            tb.diagnostics.clear();
                            if let Some(Frame::Edit(edit_frame_id)) = app.windows[app.current_window].stack().last().copied()
                                && let Some(pane_id) = diagnostics_sibling(&app.windows[app.current_window], edit_frame_id)
                            {
                                close_pane(&mut app.windows[app.current_window], pane_id);
                                close_orphaned_sessions(&mut app.sessions, &app.windows);
                            }
                            app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                command: trimmed,
                                output: String::new(),
                                status: 0,
                            });
                            return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                        }
                        "diag" | "diagnose" => {
                            show_command_mode_error(
                                &format!("bish: diag: unknown subcommand '{}' (expected: clear)", arg.unwrap_or_default()),
                                app.term_rows,
                                app.term_cols,
                            );
                            buffer.clear();
                            continue;
                        }
                        // `noh`/`nohlsearch`: vim's own escape from a
                        // search highlight that has outlived its
                        // usefulness. Any later search brings it back,
                        // `n` and `N` included, so this is never a mode
                        // anyone has to leave.
                        "noh" | "nohl" | "nohlsearch" => {
                            app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                command: trimmed.clone(),
                                output: String::new(),
                                status: 0,
                            });
                            return CommandModeOutcome::NoHighlight;
                        }
                        // `sym QUERY`: what the language server knows by
                        // that name anywhere in the project, listed in
                        // the same pane `gO`'s outline uses.
                        //
                        // A command rather than a key because it takes a
                        // query, which is also why it is `:sym` and not
                        // an extension of `gO`: `gO` answers "what is in
                        // this file", this answers "where is the thing
                        // called X", and the two only look alike because
                        // both end up as a list of places.
                        //
                        // Empty is allowed and means "everything you
                        // have" -- some servers answer that with the
                        // whole index, which the pane's own cap keeps
                        // survivable.
                        "sym" | "symbols" => {
                            let query = arg.unwrap_or_default().to_string();
                            app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                command: trimmed.clone(),
                                output: String::new(),
                                status: 0,
                            });
                            return CommandModeOutcome::Symbols(query);
                        }
                        // `:review [git [REV]]` -- what `:diff` (or `:git
                        // diff`) marks, as changes to decide on one at a
                        // time. What is refused goes back to how the
                        // other version has it; what is accepted, or not
                        // ruled on, stays as it is in the buffer. All of
                        // it one undo step, like any other command.
                        "review" => {
                            let old = match arg {
                                None => fileeditor::disk_text(tb),
                                Some(a) if a == "git" || a.starts_with("git ") => {
                                    let rev = a[3..].trim();
                                    fileeditor::git_text(tb, (!rev.is_empty()).then_some(rev))
                                }
                                Some(a) => Err(format!(
                                    "unexpected argument '{a}' -- `:review` compares with the file on disk, `:review git [REV]` with git"
                                )),
                            };
                            let old = match old {
                                Ok(old) => old,
                                Err(e) => {
                                    show_command_mode_error(&format!("bish: review: {e}"), app.term_rows, app.term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            };
                            let old: Vec<String> = old.lines().map(str::to_string).collect();
                            let new: Vec<String> = tb.text().lines().map(str::to_string).collect();
                            let output = if crate::review::hunks(&old, &new).is_empty() {
                                "No changes to review.".to_string()
                            } else {
                                let name = tb
                                    .path()
                                    .and_then(|p| p.file_name())
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "review".to_string());
                                let rect = app.focused_pane_rect();
                                let colors = syntax_color_overrides(&app.sessions[&session_id].shell);
                                let view = crate::review::Review::new(&name, old, new, &fileeditor::language_of(tb), rect.rows, rect.cols)
                                    .highlighted(Some(&colors));
                                let Some(review) = run_review(app, view) else {
                                    return CommandModeOutcome::Cancelled;
                                };
                                let reverts = review.reverts();
                                let count = reverts.len();
                                for (range, lines) in reverts {
                                    tb.replace_lines(range, lines);
                                }
                                match count {
                                    0 => "Every change kept.".to_string(),
                                    1 => "1 change put back.".to_string(),
                                    n => format!("{n} changes put back."),
                                }
                            };
                            app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                command: trimmed,
                                output: output.clone(),
                                status: 0,
                            });
                            return CommandModeOutcome::Ran { output, status: 0 };
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
                                app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                    command: trimmed,
                                    output: output.clone(),
                                    status: 0,
                                });
                                return CommandModeOutcome::Ran { output, status: 0 };
                            }
                            Err(e) => {
                                show_command_mode_error(&format!("bish: diff: {e}"), app.term_rows, app.term_cols);
                                buffer.clear();
                                continue;
                            }
                        },
                        "diff" => {
                            show_command_mode_error(
                                &format!("bish: diff: unexpected argument '{}'", arg.unwrap_or_default()),
                                app.term_rows,
                                app.term_cols,
                            );
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
                        // `:rename NEW` -- the new name is text, and the
                        // colon line is where text arguments are typed,
                        // so this is a command rather than a key. `:rn`
                        // for the same reason `:fmt` exists beside
                        // `:format`.
                        "rename" | "rn" => {
                            let Some(new_name) = arg.filter(|a| !a.trim().is_empty()) else {
                                show_command_mode_error("bish: rename: usage: :rename NEW-NAME", app.term_rows, app.term_cols);
                                buffer.clear();
                                continue;
                            };
                            match rename_via_server(app, session_id, tb, new_name.trim()) {
                                Ok(output) => {
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: output.clone(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                Err(e) => {
                                    show_command_mode_error(&format!("bish: rename: {e}"), app.term_rows, app.term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            }
                        }
                        "format" | "fmt" if arg.is_none() => {
                            // The server first when there is one: it
                            // knows the actual language, where
                            // `format_buffer` knows bash. Everything
                            // about the fallback is unchanged.
                            let served = format_via_server(app, session_id, tb);
                            let outcome = match served {
                                Some(0) => fileeditor::FormatOutcome::AlreadyFormatted,
                                Some(_) => fileeditor::FormatOutcome::Formatted,
                                None => fileeditor::format_buffer(tb),
                            };
                            let (output, status) = match outcome {
                                fileeditor::FormatOutcome::Formatted => ("Reformatted.".to_string(), 0),
                                fileeditor::FormatOutcome::AlreadyFormatted => ("Already formatted.".to_string(), 0),
                                fileeditor::FormatOutcome::NotSupported => {
                                    show_command_mode_error("bish: format: no formatter for this filetype", app.term_rows, app.term_cols);
                                    buffer.clear();
                                    continue;
                                }
                                fileeditor::FormatOutcome::Error(e) => {
                                    show_command_mode_error(&format!("bish: format: {e}"), app.term_rows, app.term_cols);
                                    buffer.clear();
                                    continue;
                                }
                            };
                            app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                command: trimmed,
                                output: output.clone(),
                                status,
                            });
                            return CommandModeOutcome::Ran { output, status };
                        }
                        "format" | "fmt" => {
                            show_command_mode_error(
                                &format!("bish: format: unexpected argument '{}'", arg.unwrap_or_default()),
                                app.term_rows,
                                app.term_cols,
                            );
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
                            let Some(Frame::Edit(edit_frame_id)) = app.windows[app.current_window].stack().last().copied() else {
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
                                    if app.debug_frames.contains_key(&edit_frame_id) {
                                        let output = "already attached -- :dbg run to start, :dbg quit to detach".to_string();
                                        app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                            command: trimmed,
                                            output: output.clone(),
                                            status: 0,
                                        });
                                        return CommandModeOutcome::Ran { output, status: 0 };
                                    }
                                    if tb.is_dirty() {
                                        show_command_mode_error(
                                            "bish: dbg: E37: no write since last change -- save first with :w",
                                            app.term_rows,
                                            app.term_cols,
                                        );
                                        buffer.clear();
                                        continue;
                                    }
                                    let Some(path) = tb.path().map(|p| p.to_path_buf()) else {
                                        show_command_mode_error("bish: dbg: no file name", app.term_rows, app.term_cols);
                                        buffer.clear();
                                        continue;
                                    };
                                    let session = match debugger::DebugSession::attach(&path) {
                                        Ok(s) => s,
                                        Err(e) => {
                                            show_command_mode_error(&format!("bish: dbg: {}: {e}", path.display()), app.term_rows, app.term_cols);
                                            buffer.clear();
                                            continue;
                                        }
                                    };
                                    app.debug_frames.insert(edit_frame_id, session);
                                    tb.set_readonly(true);
                                    tb.debug_attached = true;
                                    let pane_id = debug_run_sibling(&app.windows[app.current_window], edit_frame_id)
                                        .unwrap_or_else(|| split_debug_run_pane(app, edit_frame_id).0);
                                    let sid = app.windows[app.current_window].pane(pane_id).owning_session();
                                    render_debug_run_title(&app.sessions[&sid].screen, app.term_cols, "attached -- :dbg run to start");
                                    compositor_redraw(app);
                                    let output =
                                        "attached -- read-only until :dbg quit; :dbg run to start, :dbg break [line] to set a breakpoint".to_string();
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: output.clone(),
                                        status: 0,
                                    });
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
                                    // Only checked here; the session
                                    // itself is taken out further down,
                                    // right where it is used. Holding it
                                    // across the pane arithmetic in
                                    // between would hold a borrow of
                                    // `app` across all of it.
                                    if !app.debug_frames.contains_key(&edit_frame_id) {
                                        show_command_mode_error("bish: dbg: not attached -- use :dbg to attach", app.term_rows, app.term_cols);
                                        buffer.clear();
                                        continue;
                                    }
                                    let Some(path) = tb.path().map(|p| p.to_path_buf()) else {
                                        show_command_mode_error("bish: dbg: no file name", app.term_rows, app.term_cols);
                                        buffer.clear();
                                        continue;
                                    };
                                    let pane_id = debug_run_sibling(&app.windows[app.current_window], edit_frame_id)
                                        .unwrap_or_else(|| split_debug_run_pane(app, edit_frame_id).0);
                                    let budget = editor_pane_for(&app.windows[app.current_window], edit_frame_id)
                                        .map(|id| pane_rect(&app.windows[app.current_window], id, app.term_rows, app.term_cols).rows + 1)
                                        .unwrap_or(app.term_rows);
                                    let rows = (budget / 2).max(6).min(budget.saturating_sub(3).max(1));
                                    if let Some((_, children, idx)) = find_parent_split_mut(&mut app.windows[app.current_window].layout, pane_id) {
                                        children[idx].minimized = false;
                                        children[idx].fixed = Some(rows);
                                    }
                                    // Going to a pane is how you get a minimized one back.
                                    restore_if_minimized(&mut app.windows[app.current_window], pane_id);
                                    app.windows[app.current_window].focused_pane = pane_id;
                                    compositor_redraw(app);
                                    let run_rect = pane_rect(&app.windows[app.current_window], pane_id, app.term_rows, app.term_cols);
                                    // The real editor pane's own rect --
                                    // a pause paints straight into this
                                    // (see debugger.rs's own top-of-file
                                    // doc comment), not `run_rect`, which
                                    // stays reserved for the script's own
                                    // live/handed-off output.
                                    let Some(editor_pane) = editor_pane_for(&app.windows[app.current_window], edit_frame_id) else {
                                        unreachable!("this whole match arm only runs while editing a real Frame::Edit pane")
                                    };
                                    let editor_rect = pane_rect(&app.windows[app.current_window], editor_pane, app.term_rows, app.term_cols);

                                    let (quit_requested, nav_cursor) = match term::RawGuard::enable_with_mouse(0) {
                                        Ok(guard) => {
                                            let hook = match debugger::PauseState::new(
                                                &path,
                                                tb.breakpoints.clone(),
                                                run_rect,
                                                editor_rect,
                                                app.term_rows,
                                                app.term_cols,
                                                Rc::new(guard),
                                            ) {
                                                Ok(h) => Rc::new(RefCell::new(h)),
                                                Err(e) => {
                                                    show_command_mode_error(
                                                        &format!("bish: dbg: {}: {e}", path.display()),
                                                        app.term_rows,
                                                        app.term_cols,
                                                    );
                                                    buffer.clear();
                                                    continue;
                                                }
                                            };
                                            let session = app.debug_frames.get_mut(&edit_frame_id).expect("checked before the pane was split");
                                            let src = session.source().to_string();
                                            session.shell.set_debug_hook(Some(hook.clone() as Rc<RefCell<dyn DebugHook>>));
                                            session.shell.run_source_here(&src, &trimmed);
                                            session.shell.set_debug_hook(None);
                                            print!("{}", term::MOUSE_REPORTING_ENABLE);
                                            let _ = io::stdout().flush();
                                            (hook.borrow().quit_requested(), hook.borrow().nav_cursor())
                                        }
                                        Err(_) => {
                                            show_command_mode_error("bish: dbg: not a terminal", app.term_rows, app.term_cols);
                                            (false, None)
                                        }
                                    };

                                    if let Some((_, children, idx)) = find_parent_split_mut(&mut app.windows[app.current_window].layout, pane_id) {
                                        children[idx].minimized = true;
                                        children[idx].fixed = None;
                                    }
                                    app.windows[app.current_window].focused_pane = editor_pane;
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
                                        app.debug_frames.remove(&edit_frame_id);
                                        close_pane(&mut app.windows[app.current_window], pane_id);
                                        close_orphaned_sessions(&mut app.sessions, &app.windows);
                                        "quit"
                                    } else {
                                        let sid = app.windows[app.current_window].pane(pane_id).owning_session();
                                        render_debug_run_title(&app.sessions[&sid].screen, app.term_cols, "run finished -- :dbg run to run again");
                                        "run finished"
                                    };
                                    compositor_redraw(app);
                                    let output = status.to_string();
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: output.clone(),
                                        status: 0,
                                    });
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
                                // `:dbg launch [--adapter=CMD] PROGRAM`
                                // -- the Debug Adapter Protocol half.
                                // A separate verb from bare `:dbg`
                                // because it debugs a *program*, not
                                // this file: the source on screen is
                                // what the adapter reports positions
                                // in, and the thing that runs is
                                // whatever was built from it.
                                "launch" => {
                                    if app.debug_frames.contains_key(&edit_frame_id) {
                                        show_command_mode_error(
                                            "bish: dbg: this buffer already has a script debug session -- :dbg quit first",
                                            app.term_rows,
                                            app.term_cols,
                                        );
                                        buffer.clear();
                                        continue;
                                    }
                                    let (adapter, program) = match subarg.and_then(parse_launch_arguments) {
                                        Some(parsed) => parsed,
                                        None => {
                                            show_command_mode_error(
                                                "bish: dbg: usage: dbg launch [--adapter=COMMAND] PROGRAM",
                                                app.term_rows,
                                                app.term_cols,
                                            );
                                            buffer.clear();
                                            continue;
                                        }
                                    };
                                    // A named adapter is a program this
                                    // shell is about to start, and
                                    // `Session::start` has no `Shell` to
                                    // ask -- so a restricted shell's
                                    // rule about naming paths is applied
                                    // here, where the name was given.
                                    if let Some(named) = adapter.as_ref().and_then(|a: &Vec<String>| a.first()) {
                                        let named = named.clone();
                                        let sid = app.windows[app.current_window].owning_session();
                                        let refused = app
                                            .sessions
                                            .get_mut(&sid)
                                            .is_some_and(|state| state.shell.restricted_program_refused("dbg: --adapter", &named));
                                        if refused {
                                            show_command_mode_error(
                                                &format!("bish: dbg: {named}: restricted: cannot specify `/' in command names"),
                                                app.term_rows,
                                                app.term_cols,
                                            );
                                            buffer.clear();
                                            continue;
                                        }
                                    }
                                    let language = fileeditor::language_of(tb);
                                    let Some(adapter): Option<Vec<String>> = adapter.or_else(|| default_debug_adapter(&language)) else {
                                        show_command_mode_error(
                                            &format!("bish: dbg: no debug adapter known for {language} -- name one with --adapter=COMMAND"),
                                            app.term_rows,
                                            app.term_cols,
                                        );
                                        buffer.clear();
                                        continue;
                                    };
                                    // Relative to the file being
                                    // debugged, not to the shell: `:dbg
                                    // launch ./prog` beside the source
                                    // is what anyone would type, and
                                    // the pane's own cwd may be
                                    // anywhere.
                                    let base = tb.path().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_else(|| PathBuf::from("."));
                                    let program_path = base.join(&program);
                                    let program_path = std::fs::canonicalize(&program_path).unwrap_or(program_path);
                                    let launch = crate::json::Value::Object(vec![
                                        ("program".to_string(), crate::json::Value::Str(program_path.to_string_lossy().into_owned())),
                                        ("cwd".to_string(), crate::json::Value::Str(base.to_string_lossy().into_owned())),
                                    ]);
                                    let mut session = match crate::dapclient::Session::start(&adapter, &base, launch) {
                                        Ok(s) => s,
                                        Err(e) => {
                                            show_command_mode_error(&format!("bish: dbg: {e}"), app.term_rows, app.term_cols);
                                            buffer.clear();
                                            continue;
                                        }
                                    };
                                    // Whatever is already marked in the
                                    // gutter. Recorded now and sent by
                                    // the session when the adapter says
                                    // it is ready for them -- which is
                                    // why they can be set before there
                                    // is anything to set them on.
                                    if let Some(path) = tb.path() {
                                        let lines: Vec<usize> = tb.breakpoints.iter().copied().collect();
                                        session.set_breakpoints(path, &lines);
                                    }
                                    let pane_id = debug_run_sibling(&app.windows[app.current_window], edit_frame_id)
                                        .unwrap_or_else(|| split_debug_run_pane(app, edit_frame_id).0);
                                    let sid = app.windows[app.current_window].pane(pane_id).owning_session();
                                    render_debug_run_title(&app.sessions[&sid].screen, app.term_cols, &format!("{} -- running", program));
                                    tb.debug_attached = true;
                                    app.dap_frames.insert(
                                        edit_frame_id,
                                        DapFrame {
                                            session,
                                            program: program.clone(),
                                            thread: None,
                                            frames: Vec::new(),
                                            awaiting_stack: None,
                                            awaiting_answer: None,
                                            landing: false,
                                        },
                                    );
                                    compositor_redraw(app);
                                    let output = format!("{}: launched under {}", program, adapter.join(" "));
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: output.clone(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                // The step commands, when the session is
                                // an adapter's. The bish-script debugger
                                // answers these from inside its own
                                // paused run instead -- see the arm
                                // below, which is what is left when
                                // neither applies.
                                "continue" | "next" | "step" | "out" | "finish"
                                    if subarg.is_none() && app.dap_frames.contains_key(&edit_frame_id) =>
                                {
                                    let frame = app.dap_frames.get_mut(&edit_frame_id).expect("checked");
                                    let Some(thread) = frame.thread else {
                                        show_command_mode_error("bish: dbg: not stopped yet", app.term_rows, app.term_cols);
                                        buffer.clear();
                                        continue;
                                    };
                                    match subcmd {
                                        "continue" => frame.session.resume(thread),
                                        "next" => frame.session.step_over(thread),
                                        "step" => frame.session.step_into(thread),
                                        _ => frame.session.step_out(thread),
                                    };
                                    frame.frames.clear();
                                    let output = format!("{subcmd}...");
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: output.clone(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                // `:dbg print EXPR` against an adapter
                                // is `evaluate`, and the answer arrives
                                // later -- so the request goes out here
                                // and `sync_debug_adapter` shows what
                                // comes back.
                                "print" | "p" if app.dap_frames.contains_key(&edit_frame_id) => {
                                    let Some(expression) = subarg else {
                                        show_command_mode_error("bish: dbg: usage: dbg print EXPRESSION", app.term_rows, app.term_cols);
                                        buffer.clear();
                                        continue;
                                    };
                                    let frame = app.dap_frames.get_mut(&edit_frame_id).expect("checked");
                                    let at = frame.current_frame();
                                    // `watch`, not `repl`. The context
                                    // is not decoration: gdb reads a
                                    // `repl` evaluation as a *debugger
                                    // command*, so `a + b` comes back
                                    // as `Ambiguous command "a + b":
                                    // actions, add-auto-load-...`
                                    // rather than as 1. Every other
                                    // context evaluates it as an
                                    // expression, which is what
                                    // `:dbg print` means.
                                    let seq = frame.session.evaluate(expression, at, "watch");
                                    frame.awaiting_answer = Some((seq, expression.to_string()));
                                    let output = format!("{expression}...");
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: output.clone(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                // Where the program is, and how it got
                                // there. Answered from what the last
                                // stop already fetched, so it costs
                                // nothing and is always current.
                                "where" | "bt" if app.dap_frames.contains_key(&edit_frame_id) => {
                                    let frame = &app.dap_frames[&edit_frame_id];
                                    let output = match frame.frames.is_empty() {
                                        true => "running -- not stopped anywhere".to_string(),
                                        false => frame
                                            .frames
                                            .iter()
                                            .map(|f| match &f.source_path {
                                                Some(path) => format!("{} at {}:{}", f.name, path.rsplit('/').next().unwrap_or(path), f.line),
                                                None => f.name.clone(),
                                            })
                                            .collect::<Vec<_>>()
                                            .join("\n"),
                                    };
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: output.clone(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                "quit" | "q" if subarg.is_none() && app.dap_frames.contains_key(&edit_frame_id) => {
                                    tb.debug_attached = false;
                                    if let Some(mut frame) = app.dap_frames.remove(&edit_frame_id) {
                                        frame.session.terminate();
                                        // One tick so the request
                                        // actually reaches the adapter
                                        // before the session is dropped
                                        // -- and dropping it kills the
                                        // adapter, so a terminate it
                                        // never saw would be a
                                        // discourtesy rather than a
                                        // shutdown.
                                        frame.session.service();
                                    }
                                    if let Some(pane_id) = debug_run_sibling(&app.windows[app.current_window], edit_frame_id) {
                                        close_pane(&mut app.windows[app.current_window], pane_id);
                                        close_orphaned_sessions(&mut app.sessions, &app.windows);
                                    }
                                    compositor_redraw(app);
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: String::new(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                                }
                                "continue" | "next" | "step" if subarg.is_none() => {
                                    show_command_mode_error(
                                        "bish: dbg: not paused -- these only work while stopped at a breakpoint",
                                        app.term_rows,
                                        app.term_cols,
                                    );
                                    buffer.clear();
                                    continue;
                                }
                                // `:dbg break [line]` / `:dbg break add|remove N`.
                                //
                                // Works with no session at all, which
                                // it did not used to. A breakpoint is a
                                // mark on a line meaning "stop here
                                // when something runs this", and the
                                // gutter already draws it; requiring an
                                // attachment first was an artificial
                                // order to do things in. `:dbg launch`
                                // made that visible -- the marks have
                                // to be settable *before* the program
                                // runs, because the first stop is the
                                // one worth having.
                                "break" => {
                                    let output = match subarg {
                                        None => {
                                            let line = tb.cursor().0 + 1;
                                            let added = toggle_breakpoint(app, edit_frame_id, tb, line);
                                            format!("breakpoint {} at line {line}", if added { "set" } else { "cleared" })
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
                                                        show_command_mode_error(
                                                            &format!("bish: dbg: {n}: invalid line number"),
                                                            app.term_rows,
                                                            app.term_cols,
                                                        );
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
                                                        show_command_mode_error(
                                                            &format!("bish: dbg: {n}: invalid line number"),
                                                            app.term_rows,
                                                            app.term_cols,
                                                        );
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
                                                        show_command_mode_error(
                                                            &format!("bish: dbg: {rest}: invalid line number (expected: [line], add N, remove N)"),
                                                            app.term_rows,
                                                            app.term_cols,
                                                        );
                                                        buffer.clear();
                                                        continue;
                                                    }
                                                },
                                            }
                                        }
                                    };
                                    // `break add`/`remove N` reach the
                                    // adapter here; the bare toggle
                                    // above has already done it.
                                    sync_breakpoints_to_adapter(app, edit_frame_id, tb);
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: output.clone(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                // `:dbg print NAME`.
                                "print" | "p" => {
                                    let Some(session) = app.debug_frames.get(&edit_frame_id) else {
                                        show_command_mode_error("bish: dbg: not attached -- use :dbg to attach", app.term_rows, app.term_cols);
                                        buffer.clear();
                                        continue;
                                    };
                                    let Some(name) = subarg else {
                                        show_command_mode_error("bish: dbg: usage: dbg print NAME", app.term_rows, app.term_cols);
                                        buffer.clear();
                                        continue;
                                    };
                                    let output = match session.peek_var(name) {
                                        Some(v) => format!("{name} = {v}"),
                                        None => format!("{name}: unset or not inspectable"),
                                    };
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: output.clone(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output, status: 0 };
                                }
                                // `:dbg quit`: detach -- writable again,
                                // drops the app.debug_frames entry, closes
                                // the DebugRun sibling if one exists.
                                "quit" | "q" if subarg.is_none() => {
                                    if app.debug_frames.remove(&edit_frame_id).is_some() {
                                        tb.set_readonly(false);
                                        tb.debug_attached = false;
                                        if let Some(pane_id) = debug_run_sibling(&app.windows[app.current_window], edit_frame_id) {
                                            close_pane(&mut app.windows[app.current_window], pane_id);
                                            close_orphaned_sessions(&mut app.sessions, &app.windows);
                                        }
                                        compositor_redraw(app);
                                    }
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: String::new(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output: String::new(), status: 0 };
                                }
                                "help" | "h" | "?" if subarg.is_none() => {
                                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                        command: trimmed,
                                        output: DBG_HELP_TEXT.to_string(),
                                        status: 0,
                                    });
                                    return CommandModeOutcome::Ran { output: DBG_HELP_TEXT.to_string(), status: 0 };
                                }
                                _ => {
                                    show_command_mode_error(
                                        &format!(
                                            "bish: dbg: unknown subcommand '{subcmd}' (expected: launch, run, break, continue, next, step, out, print, where, quit, help)"
                                        ),
                                        app.term_rows,
                                        app.term_cols,
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
                        // `:help` and `:help options` -- the second
                        // generated from the option registry itself
                        // rather than written out, so a new bishopt is
                        // documented by existing.
                        "help" | "h" | "?" if arg.is_none() || arg == Some("options") || arg == Some("hooks") || arg == Some("keys") => {
                            let title = match arg {
                                Some("options") => "bish options  (q to close)",
                                Some("hooks") => "bish hooks  (q to close)",
                                Some("keys") => "bish keys  (q to close)",
                                _ => "bish help  (q to close)",
                            };
                            let source = match arg {
                                Some("options") => app.sessions.get(&session_id).map(|s| options_help(&s.shell)).unwrap_or_default(),
                                Some("hooks") => hooks_help(),
                                Some("keys") => keys_help(),
                                _ => EDITOR_HELP_MARKDOWN.to_string(),
                            };
                            run_pager(
                                app,
                                title,
                                PagerSource {
                                    language: "markdown".to_string(),
                                    source,
                                    // No file behind this one, so a
                                    // relative target has nothing to
                                    // resolve against -- which is fine,
                                    // since the help page's own links
                                    // are all absolute or none.
                                    render: RenderOptions {
                                        hyperlinks: app.sessions.get(&session_id).is_none_or(|s| s.shell.bishopt_bool("hyperlinks")),
                                        base_dir: None,
                                        colors: app.sessions.get(&session_id).map(|s| ui_colors(&s.shell)),
                                        wrap_tables: app.sessions.get(&session_id).is_none_or(|s| s.shell.bishopt_bool("table_wrap")),
                                    },
                                },
                            );
                            // Cancelled, not Ran: the pager has already
                            // shown everything there was to show, and an
                            // output overlay on top of the repaint would
                            // be a second, emptier copy of it.
                            return CommandModeOutcome::Cancelled;
                        }
                        // `:preview` -- this buffer's own markdown,
                        // rendered. The same pipeline `:help` goes
                        // through, pointed at the file being edited
                        // instead of at the built-in document.
                        "preview" | "prev" if arg.is_none() => {
                            let language = fileeditor::language_of(tb);
                            let source = fileeditor::buffer_source(tb);
                            // The buffer's own directory, so a link to a
                            // sibling document in a README resolves the
                            // same way it does in the editor itself.
                            let render = RenderOptions {
                                hyperlinks: tb.hyperlinks,
                                base_dir: tb.path().and_then(|p| p.parent()).map(|p| p.to_path_buf()),
                                colors: app.sessions.get(&session_id).map(|s| ui_colors(&s.shell)),
                                wrap_tables: app.sessions.get(&session_id).is_none_or(|s| s.shell.bishopt_bool("table_wrap")),
                            };
                            if preview_document(&language, &source, app.term_cols, &render).is_none() {
                                show_command_mode_error(
                                    &format!("bish: preview: nothing to render for a {language} file (markdown and roff have previews)"),
                                    app.term_rows,
                                    app.term_cols,
                                );
                                buffer.clear();
                                continue;
                            }
                            let name = tb
                                .path()
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "preview".to_string());
                            run_pager(app, &format!("{name}  (q to close)"), PagerSource { language, source, render });
                            return CommandModeOutcome::Cancelled;
                        }
                        "help" | "h" | "?" => {
                            show_command_mode_error(
                                &format!("bish: help: unexpected argument '{}' (no help topics yet -- just `:help`)", arg.unwrap_or_default()),
                                app.term_rows,
                                app.term_cols,
                            );
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
                        // subcommand was actually typed. `blame [REV]`/
                        // `diff [REV]` each toggle their own gutter
                        // column via fileeditor::toggle_git_blame/
                        // toggle_git_diff, whose own Ok(bool)/Err(String)
                        // already say exactly what happened and why not;
                        // the optional REV is passed through untouched
                        // for git itself to resolve (or reject, in which
                        // case its own message is what gets shown --
                        // bish has no business second-guessing what
                        // counts as a valid revision).
                        "git" => {
                            if !crate::git::available() {
                                show_command_mode_error("bish: git: git executable not found", app.term_rows, app.term_cols);
                                buffer.clear();
                                continue;
                            }
                            let (subcmd, subarg) = match arg {
                                Some(a) => match a.split_once(' ') {
                                    Some((c, r)) => (c, Some(r.trim()).filter(|r| !r.is_empty())),
                                    None => (a, None),
                                },
                                None => {
                                    show_command_mode_error(
                                        "bish: git: missing subcommand (expected: blame, diff, log, show)",
                                        app.term_rows,
                                        app.term_cols,
                                    );
                                    buffer.clear();
                                    continue;
                                }
                            };
                            // A REV is one word: `git blame HEAD~2`, not
                            // `git blame HEAD~2 something`. Caught here
                            // rather than handed to git, whose own error
                            // for a stray second argument would be about
                            // its command line rather than about `:git`'s.
                            if subarg.is_some_and(|a| a.split_whitespace().count() > 1) {
                                show_command_mode_error(
                                    &format!("bish: git: {subcmd}: expected at most one revision, got '{}'", subarg.unwrap_or_default()),
                                    app.term_rows,
                                    app.term_cols,
                                );
                                buffer.clear();
                                continue;
                            }
                            // How an enabled column names what it's
                            // showing: bare (the working tree for blame,
                            // the index for diff -- each subcommand's own
                            // git default) or the revision that was asked
                            // for, so "on" against two different revisions
                            // doesn't read identically.
                            let against = match subarg {
                                Some(rev) => format!(" ({rev})"),
                                None => String::new(),
                            };
                            match subcmd {
                                "blame" => {
                                    // The `git` capability, from the file's own
                                    // directory: an untrusted repo gets no blame
                                    // gutter, because blame runs its filters.
                                    let owned_default = app.sessions.get(&session_id).map(|s| s.shell.bishopt_str("trust_owned")).unwrap_or_default();
                                    let trusted = tb
                                        .path()
                                        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                                        .is_some_and(|d| crate::trust::is_trusted(&d, "git", &owned_default));
                                    match fileeditor::toggle_git_blame(tb, subarg, trusted) {
                                        Ok(on) => {
                                            let output = if on { format!("Blame on{against}.") } else { "Blame off.".to_string() };
                                            app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                                command: trimmed,
                                                output: output.clone(),
                                                status: 0,
                                            });
                                            return CommandModeOutcome::Ran { output, status: 0 };
                                        }
                                        Err(e) => {
                                            show_command_mode_error(&format!("bish: git: blame: {e}"), app.term_rows, app.term_cols);
                                            buffer.clear();
                                            continue;
                                        }
                                    }
                                }
                                // `diff [REV]`: same toggle shape as
                                // `blame` above, just against
                                // fileeditor::toggle_git_diff -- gutter
                                // +/~/- markers for lines added/changed/
                                // removed relative to this file's tracked
                                // state instead of per-line authorship.
                                "diff" => match fileeditor::toggle_git_diff(tb, subarg) {
                                    Ok(on) => {
                                        let output = if on { format!("Diff markers on{against}.") } else { "Diff markers off.".to_string() };
                                        app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                            command: trimmed,
                                            output: output.clone(),
                                            status: 0,
                                        });
                                        return CommandModeOutcome::Ran { output, status: 0 };
                                    }
                                    Err(e) => {
                                        show_command_mode_error(&format!("bish: git: diff: {e}"), app.term_rows, app.term_cols);
                                        buffer.clear();
                                        continue;
                                    }
                                },
                                // `show [REV]`: the whole commit in the
                                // diff view, read-only, opened at this
                                // file when the commit touched it. With
                                // no REV and blame on, the commit the
                                // gutter names beside the cursor -- see
                                // fileeditor::blamed_commit_at_cursor.
                                "show" => {
                                    let rev = match (subarg, fileeditor::blamed_commit_at_cursor(tb)) {
                                        (Some(rev), _) => rev.to_string(),
                                        (None, Some(Ok(commit))) => commit,
                                        (None, Some(Err(e))) => {
                                            show_command_mode_error(&format!("bish: git: show: {e}"), app.term_rows, app.term_cols);
                                            buffer.clear();
                                            continue;
                                        }
                                        (None, None) => "HEAD".to_string(),
                                    };
                                    let rect = app.focused_pane_rect();
                                    let colors = syntax_color_overrides(&app.sessions[&session_id].shell);
                                    match git_show_view(tb.path(), &rev, Some(&colors), rect.rows, rect.cols) {
                                        Ok(view) => {
                                            run_review(app, view);
                                            return CommandModeOutcome::Cancelled;
                                        }
                                        Err(e) => {
                                            show_command_mode_error(&format!("bish: git: show: {e}"), app.term_rows, app.term_cols);
                                            buffer.clear();
                                            continue;
                                        }
                                    }
                                }
                                // `log [REV]`: this file's commits,
                                // newest first and followed through
                                // renames -- every commit, for a buffer
                                // with no file. Enter opens one in the
                                // diff view; closing that comes back to
                                // the list.
                                "log" => {
                                    let path = tb.path().map(Path::to_path_buf);
                                    let dir = match path.as_deref().and_then(Path::parent).filter(|d| !d.as_os_str().is_empty()) {
                                        Some(dir) => dir.to_path_buf(),
                                        None => std::env::current_dir().unwrap_or_default(),
                                    };
                                    let entries = match crate::git::log(&dir, path.as_deref(), subarg) {
                                        Ok(entries) if entries.is_empty() => {
                                            show_command_mode_error("bish: git: log: no commit has touched this file", app.term_rows, app.term_cols);
                                            buffer.clear();
                                            continue;
                                        }
                                        Ok(entries) => entries,
                                        Err(e) => {
                                            show_command_mode_error(&format!("bish: git: log: {e}"), app.term_rows, app.term_cols);
                                            buffer.clear();
                                            continue;
                                        }
                                    };
                                    let title = match path.as_deref().and_then(Path::file_name) {
                                        Some(name) => format!("git log {}", name.to_string_lossy()),
                                        None => "git log".to_string(),
                                    };
                                    let rect = app.focused_pane_rect();
                                    let colors = syntax_color_overrides(&app.sessions[&session_id].shell);
                                    let list = crate::commitlist::CommitList::new(&title, entries, rect.rows, rect.cols);
                                    let failed = run_commit_list(app, list, |hash, rows, cols| {
                                        git_show_view(path.as_deref(), hash, Some(&colors), rows, cols)
                                    });
                                    if let Some(e) = failed {
                                        show_command_mode_error(&format!("bish: git: log: {e}"), app.term_rows, app.term_cols);
                                        buffer.clear();
                                        continue;
                                    }
                                    return CommandModeOutcome::Cancelled;
                                }
                                other => {
                                    show_command_mode_error(
                                        &format!("bish: git: unknown subcommand '{}' (expected: blame, diff, log, show)", other),
                                        app.term_rows,
                                        app.term_cols,
                                    );
                                    buffer.clear();
                                    continue;
                                }
                            }
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
                    app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                        command: trimmed,
                        output: String::new(),
                        status: 0,
                    });
                    return CommandModeOutcome::Quit;
                }

                match Lexer::new(&buffer).tokenize() {
                    Ok(toks) => match Parser::new(app.sessions.get(&session_id).map(|s| s.shell.expand_aliases(toks.clone())).unwrap_or(toks))
                        .parse_program()
                    {
                        Ok(prog) => {
                            if let Some(msg) = command_mode_violation(&prog) {
                                show_command_mode_error(&format!("bish: {}", msg), app.term_rows, app.term_cols);
                                buffer.clear();
                            } else {
                                // No meaningful shell context here -- this
                                // is command mode's own, entirely
                                // separate app.cmd_history of window-management
                                // commands, not shell command lines, so
                                // there's no cwd worth tagging it with
                                // (same reasoning its own read_line call
                                // already uses for HighlightContext::default()
                                // and a None completion provider).
                                if !buffer_unrecorded {
                                    app.cmd_history.record(&buffer, None);
                                }
                                let (result, captured_text) = {
                                    let session = app.sessions.get_mut(&session_id).unwrap();
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
                                        app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
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
                                        app.sessions.get_mut(&session_id).unwrap().shell.discard_pending_fg();
                                        let output = "bish: fg: not supported in command mode -- use it from the normal shell prompt".to_string();
                                        app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                            command: trimmed,
                                            output: output.clone(),
                                            status: 1,
                                        });
                                        return CommandModeOutcome::Ran { output, status: 1 };
                                    }
                                    // Same reasoning as Fg just above --
                                    // this restricted read-eval loop has
                                    // no way to drive an interactive
                                    // editor session either.
                                    ExecResult::Edit => {
                                        app.sessions.get_mut(&session_id).unwrap().shell.take_pending_edit();
                                        let output = "bish: e: not supported in command mode -- use it from the normal shell prompt".to_string();
                                        app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
                                            command: trimmed,
                                            output: output.clone(),
                                            status: 1,
                                        });
                                        return CommandModeOutcome::Ran { output, status: 1 };
                                    }
                                    // Matches this codebase's prior behavior
                                    // for `exit`/`set -e`/`set -u` (see the
                                    // other run_program call site's own
                                    // ExecResult::Exit arm above) -- the
                                    // exit trap already ran wherever this
                                    // was produced.
                                    ExecResult::Exit(code) => {
                                        session::stop_listening();
                                        // The terminal's own title, given back:
                                        // this path never reaches the end of
                                        // `run`.
                                        term::pop_window_title();
                                        std::process::exit(code)
                                    }
                                    _ => {
                                        let status = app.sessions[&session_id].shell.last_status;
                                        app.sessions.get_mut(&session_id).unwrap().command_transcript.push(TranscriptEntry {
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
                                show_command_mode_error(&format!("bish: syntax error: {}", e), app.term_rows, app.term_cols);
                                buffer.clear();
                            }
                        }
                    },
                    Err(e) => {
                        if !is_incomplete(&e) {
                            show_command_mode_error(&format!("bish: syntax error: {}", e), app.term_rows, app.term_cols);
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

#[derive(Debug, Clone)]
struct SubstituteCmd {
    from: LineRef,
    to: LineRef,
    pattern: String,
    replacement: String,
    /// `g` -- every match on a line, not just the first.
    global: bool,
    /// `i` / `I`. `None` is "neither was given", which leaves the choice
    /// to `'ignorecase'`/`'smartcase'` as it was before either flag
    /// existed here.
    ignore_case: Option<bool>,
    /// The count vim allows after the flags, which *replaces* the range
    /// rather than narrowing it: `:1,2s/x/y/ 3` acts on three lines
    /// starting at line 2 -- the range's own last line -- so it can name
    /// lines the range never covered. Confirmed against real vim.
    count: Option<usize>,
    /// `n` -- report how many matches there are and change nothing.
    count_only: bool,
    /// `e` -- a pattern that matches nothing is not an error.
    no_error: bool,
    /// `&` -- take the previous `:s`'s flags. Must be written first, as
    /// it is in vim, and inherits the flags only: a count is not one,
    /// confirmed by running vim.
    keep_flags: bool,
    /// `p` / `#` / `l` -- print the last line a substitution touched.
    print: Option<PrintStyle>,
    /// `c` -- ask about each match before making it.
    confirm: bool,
}

/// How `p`, `#` and `l` each spell the line they print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrintStyle {
    /// `p` -- the line's own text.
    Plain,
    /// `#` -- with its line number in front, right-aligned the way vim
    /// aligns it.
    Numbered,
    /// `l` -- `:list` style, which marks the end of the line with `$`
    /// and shows a tab as `^I`, so trailing space is visible.
    List,
}

/// Everything after `:s/pat/repl/`, which is the same set of answers
/// `SubstituteCmd` carries -- see its fields for what each one means.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SubstituteFlags {
    global: bool,
    ignore_case: Option<bool>,
    count: Option<usize>,
    count_only: bool,
    no_error: bool,
    keep_flags: bool,
    /// `r` -- reach for the last *search* pattern rather than the last
    /// substitute's. It means something on `:&r`, which is how vim
    /// spells `:~` (see `repeat_substitute`); on a `:s//repl/` it
    /// changes nothing, and `resolve_substitute` has the vim runs that
    /// show why.
    use_search_pattern: bool,
    print: Option<PrintStyle>,
    confirm: bool,
}

/// Reads them.
///
/// Unknown flags are refused rather than ignored. Silently dropping them
/// is what made `:%s/a/Z/gi` quietly do a case-*sensitive* substitution
/// and report success -- a wrong answer, where an error would at least
/// have been true.
fn parse_substitute_flags(text: &str) -> Result<SubstituteFlags, String> {
    let mut f = SubstituteFlags::default();
    let mut digits: Option<usize> = None;
    for (at, c) in text.chars().enumerate() {
        // A count ends the flags: everything from the first digit on is
        // the number, and a letter after it is trailing junk.
        if let Some(n) = digits {
            match c.to_digit(10) {
                Some(d) => {
                    digits = Some(n.saturating_mul(10).saturating_add(d as usize));
                    continue;
                }
                None => return Err(format!("E488: Trailing characters: {c}")),
            }
        }
        match c {
            'g' => f.global = true,
            'i' => f.ignore_case = Some(true),
            'I' => f.ignore_case = Some(false),
            'n' => f.count_only = true,
            'e' => f.no_error = true,
            'c' => f.confirm = true,
            'r' => f.use_search_pattern = true,
            'p' => f.print = Some(PrintStyle::Plain),
            '#' => f.print = Some(PrintStyle::Numbered),
            'l' => f.print = Some(PrintStyle::List),
            // `&` means "the previous command's flags" and vim accepts
            // it only in first position -- written anywhere else it
            // rejects the command rather than reading it out of order.
            '&' if at == 0 => f.keep_flags = true,
            '&' => return Err("E488: Trailing characters: & (it only means anything written first)".to_string()),
            ' ' | '\t' => {}
            '0'..='9' => digits = Some(c.to_digit(10).expect("matched a digit") as usize),
            _ => return Err(format!("E488: Trailing characters: {c}")),
        }
    }
    // `:s/a/b/0` is not "zero lines", it is a typo.
    if digits == Some(0) {
        return Err("E939: Positive count required".to_string());
    }
    f.count = digits;
    Ok(f)
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
/// The ex commands that are a line range and a letter: `:{n}` on its
/// own, `:{range}d`, `:{range}m{dest}`, plus `:normal`, which is not one
/// of those but is parsed here because it is the other command whose
/// name runs straight into its argument.
#[derive(Debug, Clone, PartialEq)]
enum LineCommand {
    /// `:5` -- a bare address is "go there", vim's oldest command.
    Goto(LineRef),
    Delete {
        from: LineRef,
        to: LineRef,
    },
    /// `dest` is a 1-indexed line number to move *after*, so 0 means
    /// "to the very top" -- vim's `:m0`. Deliberately not a row: `:m0`
    /// and `:m1` are different destinations, and every 0-indexed row
    /// would have to represent both.
    Move {
        from: LineRef,
        to: LineRef,
        dest: LineRef,
    },
    /// `:1t$` -- `:m` that leaves the original where it is. Its other
    /// spellings are `:copy` and `:co`.
    Copy {
        from: LineRef,
        to: LineRef,
        dest: LineRef,
    },
    /// `:>` and `:<`, one shift per repeated character: `:>>` shifts
    /// twice.
    Shift {
        from: LineRef,
        to: LineRef,
        right: bool,
        times: usize,
    },
    Normal(String),
    /// `:fold`, `:foldopen[!]` and `:foldclose[!]` over a range, the
    /// current line when there is none.
    Fold {
        from: LineRef,
        to: LineRef,
        action: FoldAction,
    },
}

/// Which of the fold commands. `all` is the `!`: every level, rather
/// than one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FoldAction {
    Create,
    Open { all: bool },
    Close { all: bool },
}

/// What a `!` at the head of a colon line turned out to be. `None` from
/// `parse_bang_command` means there was no `!` there at all, and every
/// other command in this dispatch is untouched.
enum BangCommand<'a> {
    /// `:!cmd` -- run it through the shell. The `&str` is everything
    /// after the `!`, still unexpanded.
    Run(&'a str),
    /// `:[range]!cmd` -- vim's filter: those lines go in as the
    /// command's stdin and whatever it writes out replaces them.
    Filter { from: LineRef, to: LineRef, cmd: &'a str },
}

/// A leading `!`, with or without a range in front of it. `:5!x` and
/// `:%!x` are the filter; a bare `:!x` is the plain shell command.
/// `:'<,'>!x` is *not* recognized as either, because `parse_range_prefix`
/// does not read visual-mode marks -- see its own comment. It reaches
/// the shell as a command named `'<,'>!x`, which is what it did before
/// this existed.
///
/// Deliberately not folded into `parse_line_command`: that one answers
/// with a `LineCommand` the buffer can carry out on its own, and both of
/// these need a shell, which lives a long way up in the session.
fn parse_bang_command(trimmed: &str) -> Option<BangCommand<'_>> {
    if let Some(rest) = trimmed.strip_prefix('!') {
        return Some(BangCommand::Run(rest));
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let mut i = 0;
    let (from, to) = parse_range_prefix(&chars, &mut i)?;
    if chars.get(i) != Some(&'!') {
        return None;
    }
    // Byte offset, not `i`: the range that precedes the `!` is ASCII by
    // construction (digits, `%`, `.`, `$`, `,`), so the two agree here --
    // but slicing `trimmed` wants the byte one regardless.
    let at = trimmed.char_indices().nth(i).map(|(at, _)| at)?;
    Some(BangCommand::Filter { from, to, cmd: &trimmed[at + 1..] })
}

/// Which of the three "do that again" commands this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepeatSubstitute {
    /// `:s` on its own and `:&` -- the last substitute's pattern and
    /// replacement, and *none* of its flags.
    Bare,
    /// `:&&` -- the same, keeping the flags.
    KeepFlags,
    /// `:~` -- the last substitute's replacement, but the last *search*
    /// pattern. `:&r` is spelled differently and means this.
    LastSearch,
}

/// `:[range]s`, `:[range]&`, `:[range]&&`, `:[range]~`, each with its
/// own optional flags and count after it.
///
/// Checked before `parse_substitute_command`, and careful not to shadow
/// it: an `s` here has to be followed by the end of the line or a space,
/// so `:s/a/b/` stays a substitute and `:set`/`:sort` stay themselves.
fn parse_repeat_substitute(trimmed: &str) -> Option<Result<(LineRef, LineRef, RepeatSubstitute, SubstituteFlags), String>> {
    let chars: Vec<char> = trimmed.chars().collect();
    let mut i = 0;
    let (from, to) = parse_range_prefix(&chars, &mut i).unwrap_or((LineRef::Current, LineRef::Current));
    let kind = match (chars.get(i), chars.get(i + 1)) {
        (Some('&'), Some('&')) => {
            i += 2;
            RepeatSubstitute::KeepFlags
        }
        (Some('&'), _) => {
            i += 1;
            RepeatSubstitute::Bare
        }
        (Some('~'), _) => {
            i += 1;
            RepeatSubstitute::LastSearch
        }
        // A bare `s`, and only a bare one.
        (Some('s'), None) | (Some('s'), Some(' ')) => {
            i += 1;
            RepeatSubstitute::Bare
        }
        _ => return None,
    };
    let flags: String = chars[i..].iter().collect();
    Some(parse_substitute_flags(&flags).map(|f| (from, to, kind, f)))
}

/// Turns one of those into the `:s` it stands for.
///
/// The pattern is taken deliberately rather than through
/// `resolve_substitute`'s "whichever was more recent" rule: `:&` means
/// the last *substitute*'s pattern even when a `/` search has happened
/// since, which is exactly where that rule and this one part company.
/// Confirmed by running vim.
fn repeat_substitute(
    last: Option<&LastSubstitute>,
    last_search: &str,
    from: LineRef,
    to: LineRef,
    kind: RepeatSubstitute,
    flags: SubstituteFlags,
) -> Result<SubstituteCmd, String> {
    let Some(previous) = last else { return Err("E33: No previous substitute regular expression".to_string()) };
    let search_pattern = kind == RepeatSubstitute::LastSearch || flags.use_search_pattern;
    let pattern = match search_pattern {
        true if !last_search.is_empty() => last_search.to_string(),
        _ => previous.cmd.pattern.clone(),
    };
    // `:&&` starts from the previous flags and `:s`/`:&`/`:~` start from
    // none, and either way the ones written here go on top.
    let base = match kind {
        RepeatSubstitute::KeepFlags => &previous.cmd,
        _ => &SubstituteCmd {
            global: false,
            ignore_case: None,
            count_only: false,
            no_error: false,
            confirm: false,
            print: None,
            ..previous.cmd.clone()
        },
    };
    Ok(SubstituteCmd {
        from,
        to,
        pattern,
        replacement: previous.cmd.replacement.clone(),
        global: base.global || flags.global,
        ignore_case: flags.ignore_case.or(base.ignore_case),
        // A count belongs to the command that writes it and is never
        // inherited -- see `SubstituteCmd::count`.
        count: flags.count,
        count_only: base.count_only || flags.count_only,
        no_error: base.no_error || flags.no_error,
        keep_flags: false,
        print: flags.print.or(base.print),
        confirm: base.confirm || flags.confirm,
    })
}

/// `None` when `trimmed` is not one of these at all, so the ordinary
/// dispatch still sees `w`, `set`, `diag` and the rest unchanged. The
/// same shape `parse_substitute_command` uses, and for the same reason:
/// these commands run their name into their argument with no space, so
/// the `split_once(' ')` dispatch cannot see them.
fn parse_line_command(trimmed: &str) -> Option<LineCommand> {
    if let Some(rest) = trimmed.strip_prefix("normal ").or_else(|| trimmed.strip_prefix("norm ")) {
        return Some(LineCommand::Normal(rest.to_string()));
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let mut i = 0;
    let range = parse_range_prefix(&chars, &mut i);
    match (chars.get(i), range) {
        // A range and nothing after it: go to its last line. `:` alone
        // is not this -- `parse_range_prefix` found no address at all.
        (None, Some((_, to))) => Some(LineCommand::Goto(to)),
        // A missing range means the current line, which is what makes
        // a bare `:d` and a bare `:>` mean anything at all.
        (Some('d'), range) if i + 1 == chars.len() => {
            let (from, to) = range.unwrap_or((LineRef::Current, LineRef::Current));
            Some(LineCommand::Delete { from, to })
        }
        (Some('m'), Some((from, to))) => {
            let mut j = i + 1;
            let dest = parse_one_line_ref(&chars, &mut j)?;
            (j == chars.len()).then_some(LineCommand::Move { from, to, dest })
        }
        (Some('t'), Some((from, to))) => {
            let mut j = i + 1;
            let dest = parse_one_line_ref(&chars, &mut j)?;
            (j == chars.len()).then_some(LineCommand::Copy { from, to, dest })
        }
        (Some(&c @ ('>' | '<')), range) => {
            let (from, to) = range.unwrap_or((LineRef::Current, LineRef::Current));
            let right = c == '>';
            let times = chars[i..].iter().take_while(|ch| **ch == c).count();
            (i + times == chars.len()).then_some(LineCommand::Shift { from, to, right, times })
        }
        // `:fo[ld]`, `:foldo[pen][!]`, `:foldc[lose][!]` -- vim's own
        // abbreviations, and like `:d` a missing range is this line.
        // Any other word is not a line command, and goes on to the
        // commands that are not.
        (Some(c), range) if c.is_ascii_alphabetic() => {
            let word: String = chars[i..].iter().collect();
            let (word, bang) = match word.strip_suffix('!') {
                Some(word) => (word.to_string(), true),
                None => (word, false),
            };
            let action = match word.as_str() {
                "fo" | "fol" | "fold" if !bang => FoldAction::Create,
                w if w.len() >= 5 && "foldopen".starts_with(w) => FoldAction::Open { all: bang },
                w if w.len() >= 5 && "foldclose".starts_with(w) => FoldAction::Close { all: bang },
                _ => return None,
            };
            let (from, to) = range.unwrap_or((LineRef::Current, LineRef::Current));
            Some(LineCommand::Fold { from, to, action })
        }
        _ => None,
    }
}

/// Runs everything `parse_line_command` produces except `Normal`, which
/// needs the Normal-mode loop's own `VimKeys` and is handed back to it.
fn run_line_command(tb: &mut TextBuffer, cmd: &LineCommand) -> Result<(), String> {
    let last = tb.line_count().saturating_sub(1);
    let current = tb.cursor().0;
    match *cmd {
        LineCommand::Goto(to) => {
            let row = to.resolve(current, last);
            tb.set_cursor(row, 0);
            let col = crate::bishedit::motion::first_non_blank(tb, row);
            tb.set_cursor(row, col);
            Ok(())
        }
        LineCommand::Delete { from, to } => {
            let (a, b) = ordered(from, to, current, last);
            tb.set_cursor(a, 0);
            tb.delete_range(&crate::bishedit::motion::MotionRange {
                shape: crate::bishedit::motion::MotionShape::Linewise,
                from: (a, 0),
                to: (b, 0),
            });
            Ok(())
        }
        LineCommand::Move { from, to, dest } => {
            let (a, b) = ordered(from, to, current, last);
            // 1-indexed, and 0 is a real answer meaning "the top".
            let dest = match dest {
                LineRef::Current => current + 1,
                LineRef::Last => last + 1,
                LineRef::Number(n) => n.min(last + 1),
            };
            if dest > a && dest <= b + 1 {
                return Err("E134: Cannot move a range of lines into itself".to_string());
            }
            let text = crate::bishedit::motion::extract_text(
                &*tb,
                &crate::bishedit::motion::MotionRange { shape: crate::bishedit::motion::MotionShape::Linewise, from: (a, 0), to: (b, 0) },
            );
            tb.delete_range(&crate::bishedit::motion::MotionRange {
                shape: crate::bishedit::motion::MotionShape::Linewise,
                from: (a, 0),
                to: (b, 0),
            });
            // The destination counts lines that are no longer there once
            // the block has been lifted out, so a destination below the
            // block shifts up by its height.
            let height = b - a + 1;
            let row = if dest > b + 1 { dest - height } else { dest };
            let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
            tb.set_cursor(row.min(tb.line_count()), 0);
            insert_lines_at(tb, row, &text);
            Ok(())
        }
        LineCommand::Copy { from, to, dest } => {
            let (a, b) = ordered(from, to, current, last);
            let dest = match dest {
                LineRef::Current => current + 1,
                LineRef::Last => last + 1,
                LineRef::Number(n) => n.min(last + 1),
            };
            // Nothing is lifted out, so unlike `:m` the destination
            // needs no adjusting -- and copying a range into itself is
            // perfectly well defined.
            let text = crate::bishedit::motion::extract_text(
                &*tb,
                &crate::bishedit::motion::MotionRange { shape: crate::bishedit::motion::MotionShape::Linewise, from: (a, 0), to: (b, 0) },
            );
            let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
            tb.set_cursor(dest.min(tb.line_count()), 0);
            insert_lines_at(tb, dest, &text);
            Ok(())
        }
        LineCommand::Shift { from, to, right, times } => {
            let (a, b) = ordered(from, to, current, last);
            for row in a..=b.min(tb.line_count().saturating_sub(1)) {
                for _ in 0..times {
                    match right {
                        true => crate::fileeditor::indent_rows(tb, row, row),
                        false => crate::fileeditor::outdent_rows(tb, row, row),
                    }
                }
            }
            tb.set_cursor(b.min(tb.line_count().saturating_sub(1)), 0);
            Ok(())
        }
        LineCommand::Fold { from, to, action } => {
            let (a, b) = ordered(from, to, current, last);
            match action {
                FoldAction::Create => tb.folds.create(a, b),
                FoldAction::Open { all } => tb.folds.open_range(a, b, all),
                FoldAction::Close { all } => tb.folds.close_range(a, b, all),
            }
            Ok(())
        }
        LineCommand::Normal(_) => Ok(()),
    }
}

/// `:!cmd` -- vim's "run this through the shell". The buffer is not
/// touched; what the command wrote is shown in the same overlay every
/// other colon command's output uses.
///
/// A child shell rather than the ordinary colon line, and for two
/// reasons that point the same way. `:!make`, `:!git status`, `:!ls`
/// are the whole point of `!`, and the colon line runs only builtins
/// (its own escape hatch is `command NAME` -- an explicit "the external
/// one", which is exactly what `!` says in vim's spelling). And an
/// external's output cannot be seen by the colon line's capture sink
/// anyway: it goes to a real descriptor, which under the editor's
/// alternate screen means it flashes and is painted over by the next
/// redraw. `filter_through_command` is the mechanism that catches it.
///
/// Its stdin is empty rather than the terminal, so `:!cat` ends at once
/// instead of swallowing the keystrokes meant for the editor.
fn run_bang_command(app: &mut App, session_id: SessionId, tb: &TextBuffer, cmd: &str) -> Result<String, String> {
    let cmd = crate::fileeditor::expand_filename_macros(cmd, tb)?;
    if cmd.trim().is_empty() {
        // vim repeats the last `:!` here. bish keeps no such register
        // yet, so it says so rather than running an empty line.
        return Err("E34: No previous command".to_string());
    }
    let session = app.sessions.get_mut(&session_id).expect("command mode always has a live session");
    session.shell.sync_real_state_in();
    let ran = session.shell.filter_through_command(&cmd, "");
    session.shell.sync_real_state_out();
    let (output, errors, _status) = ran.map_err(|e| format!("{cmd}: {e}"))?;
    // Both streams, in the order a terminal would have interleaved them
    // as well as this can be reconstructed after the fact -- what the
    // command *did* first, then what it complained about.
    Ok(format!("{output}{errors}").trim_end().to_string())
}

/// `:[range]!cmd` -- vim's filter. The lines go in as the command's
/// stdin, and whatever it writes to stdout replaces them.
///
/// vim replaces the range whatever the command's exit status was, so
/// `:%!false` empties the buffer -- and so does this. Only replacing on
/// success would quietly do nothing for a command that legitimately
/// exits non-zero, and `u` is right there for the mistake.
fn filter_lines_through_command(
    app: &mut App,
    session_id: SessionId,
    tb: &mut TextBuffer,
    from: LineRef,
    to: LineRef,
    cmd: &str,
) -> Result<String, String> {
    use crate::bishedit::motion::{MotionRange, MotionShape};

    let cmd = crate::fileeditor::expand_filename_macros(cmd, tb)?;
    if cmd.trim().is_empty() {
        return Err("E34: No previous command".to_string());
    }
    let last = tb.line_count().saturating_sub(1);
    let (a, b) = ordered(from, to, tb.cursor().0, last);
    let input = crate::bishedit::motion::extract_text(&*tb, &MotionRange { shape: MotionShape::Linewise, from: (a, 0), to: (b, 0) });

    // Not `restrict_to_builtins` and not the capture sink -- see
    // `Shell::filter_through_command`, which explains why an external's
    // output can only be caught the way `$( )` catches it. `sort` and
    // `column -t` are the entire point of a filter, so this is the one
    // colon-line command that must reach past both.
    let session = app.sessions.get_mut(&session_id).expect("command mode always has a live session");
    session.shell.sync_real_state_in();
    let ran = session.shell.filter_through_command(&cmd, &input);
    session.shell.sync_real_state_out();
    let (output, errors, _status) = ran.map_err(|e| format!("{cmd}: {e}"))?;

    replace_lines(tb, a, b, &output);
    // stdout went into the buffer, so what is left to show is whatever
    // the command complained about -- and a filter that worked
    // complained about nothing, which is the empty overlay.
    Ok(errors.trim_end().to_string())
}

/// Puts `output` in place of rows `a..=b`, the way a filter's result
/// lands back in the buffer.
///
/// Its own function because the two edges here are the whole difficulty
/// and neither needs a shell to exercise: the trailing newline every
/// command writes is what *ends* the last line rather than starting an
/// empty one after it, and a buffer always keeps at least one line -- so
/// deleting every row leaves an empty one that has to be written into
/// rather than inserted above, which is what made `:%!sort` come back a
/// line longer than it went in.
fn replace_lines(tb: &mut TextBuffer, a: usize, b: usize, output: &str) {
    use crate::bishedit::motion::{MotionRange, MotionShape};
    let whole = a == 0 && b >= tb.line_count().saturating_sub(1);
    tb.set_cursor(a, 0);
    tb.delete_range(&MotionRange { shape: MotionShape::Linewise, from: (a, 0), to: (b, 0) });
    let replacement = output.strip_suffix('\n').unwrap_or(output);
    if replacement.is_empty() {
        // `:%!true` empties the buffer, exactly as it does in vim.
    } else if whole {
        tb.insert_text((0, 0), replacement);
    } else {
        insert_lines_at(tb, a, replacement);
    }
    tb.set_cursor(a.min(tb.line_count().saturating_sub(1)), 0);
}

/// Both ends of a range as 0-indexed rows, smaller first.
fn ordered(from: LineRef, to: LineRef, current: usize, last: usize) -> (usize, usize) {
    let a = from.resolve(current, last);
    let b = to.resolve(current, last);
    if a <= b { (a, b) } else { (b, a) }
}

/// Splices `text` in as whole lines starting at row `at`, which may be
/// one past the last line (appending at the end).
fn insert_lines_at(tb: &mut TextBuffer, at: usize, text: &str) {
    let count = tb.line_count();
    if at >= count {
        let end = count.saturating_sub(1);
        tb.set_cursor(end, tb.line_len(end));
        tb.insert_text((end, tb.line_len(end)), &format!("\n{text}"));
    } else {
        tb.insert_text((at, 0), &format!("{text}\n"));
    }
    tb.set_cursor(at.min(tb.line_count().saturating_sub(1)), 0);
}

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

    // An empty pattern is `:s//repl/` -- "the last one again" -- and is
    // filled in by `resolve_substitute`, which is the only place that
    // knows what the last one was.
    let pattern = scan_until_delim(&chars, &mut i, delim);
    let replacement = scan_until_delim(&chars, &mut i, delim);
    let flags: String = chars[i..].iter().collect();
    let f = match parse_substitute_flags(&flags) {
        Ok(parsed) => parsed,
        Err(e) => return Some(Err(e)),
    };
    Some(Ok(SubstituteCmd {
        from,
        to,
        pattern,
        replacement,
        global: f.global,
        ignore_case: f.ignore_case,
        count: f.count,
        count_only: f.count_only,
        no_error: f.no_error,
        keep_flags: f.keep_flags,
        print: f.print,
        confirm: f.confirm,
    }))
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
// `:[range]g/pattern/cmd` and its inverse `:g!` (`:v`) -- run `cmd` on
// every line that matches, or on every line that does not.
//
// The operation people reach for when a substitution is not enough:
// `:g/TODO/d`, `:g!/^$/d`, `:g/^fn /s/pub //`. Over the same hand-rolled
// ERE engine `:s` already uses, and honouring the same `ignorecase`.
struct GlobalCmd {
    from: LineRef,
    to: LineRef,
    pattern: String,
    invert: bool,
    command: String,
}

// `g`/`g!`/`v`, a delimiter, the pattern, the same delimiter, then the
// command. `None` for anything that is not an attempt at one at all, so
// an unrecognised `:gitfoo` still falls through to ordinary dispatch --
// the same contract `parse_substitute_command` follows.
fn parse_global_command(trimmed: &str) -> Option<Result<GlobalCmd, String>> {
    let chars: Vec<char> = trimmed.chars().collect();
    let mut i = 0;
    // Default range is the *whole file*, not the current line -- `:g`'s
    // own default in vim, and the opposite of `:s`'s.
    let (from, to) = parse_range_prefix(&chars, &mut i).unwrap_or((LineRef::Number(1), LineRef::Last));

    let invert = match chars.get(i) {
        // `:g!` is parsed here and does not currently *reach* here from
        // the colon line: bang-history expansion (history::expand, run
        // on every command-mode line before dispatch) claims the `!`
        // first and rewrites the line. `:v` is vim's own equally
        // standard spelling of the same thing and is unaffected, so
        // nothing is missing -- but this arm stays, because the moment
        // bang-history goes the collision goes with it.
        Some('g') if chars.get(i + 1) == Some(&'!') => {
            i += 2;
            true
        }
        Some('g') => {
            i += 1;
            false
        }
        Some('v') => {
            i += 1;
            true
        }
        _ => return None,
    };
    // A delimiter has to follow immediately, or this is some other
    // command that merely starts with the same letter (`gi`, `version`).
    let delimiter = match chars.get(i) {
        Some(c) if !c.is_alphanumeric() && !c.is_whitespace() => *c,
        _ => return None,
    };
    i += 1;
    let mut pattern = String::new();
    while let Some(&c) = chars.get(i) {
        i += 1;
        if c == '\\' && chars.get(i) == Some(&delimiter) {
            pattern.push(delimiter);
            i += 1;
            continue;
        }
        if c == delimiter {
            let command: String = chars[i..].iter().collect();
            let command = command.trim().to_string();
            if pattern.is_empty() {
                return Some(Err("g: empty pattern".to_string()));
            }
            if command.is_empty() {
                return Some(Err("g: no command to run on the matching lines".to_string()));
            }
            return Some(Ok(GlobalCmd { from, to, pattern, invert, command }));
        }
        pattern.push(c);
    }
    Some(Err("g: unterminated pattern".to_string()))
}

// Returns how many lines the command actually ran on.
//
// Two passes, exactly as vim does it and for the same reason: every
// matching line is found *first*, then the command runs on each. A
// single pass would have the command's own edits decide which lines the
// pattern still gets tested against -- so `:g/x/d` on consecutive
// matches would skip every other one.
//
// The marked lines are then run highest-first, so a deletion earlier in
// the file cannot shift a line that has not been reached yet.
fn run_global(tb: &mut TextBuffer, cmd: &GlobalCmd) -> Result<usize, String> {
    let last = tb.line_count().saturating_sub(1);
    let current = tb.cursor().0;
    let a = cmd.from.resolve(current, last);
    let b = cmd.to.resolve(current, last);
    let (start, end) = (a.min(b), a.max(b).min(last));
    let re = crate::regex::Regex::compile(&cmd.pattern, tb.search_ignore_case(&cmd.pattern));

    let marked: Vec<usize> = (start..=end)
        .filter(|&row| {
            let chars = tb.line_chars(row);
            re.find_at(&chars, 0).is_some() != cmd.invert
        })
        .collect();
    if marked.is_empty() {
        return Ok(0);
    }

    // Only the commands that mean something applied line by line. `d`
    // and a substitution cover what `:g` is actually used for; anything
    // else is refused by name rather than silently doing nothing.
    let body = cmd.command.trim();
    if body == "d" || body == "delete" {
        for &row in marked.iter().rev() {
            if tb.line_count() == 1 {
                // Never leave zero lines -- `delete_range`'s own
                // Linewise branch already restores that invariant, but
                // deleting the only line should clear it, not remove it.
                let len = tb.line_len(0);
                tb.delete_range(&motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (0, 0), to: (0, len) });
                continue;
            }
            tb.delete_range(&motion::MotionRange { shape: motion::MotionShape::Linewise, from: (row, 0), to: (row, 0) });
        }
        let row = marked[0].min(tb.line_count().saturating_sub(1));
        tb.set_cursor(row, 0);
        return Ok(marked.len());
    }
    if body.starts_with('s') {
        let mut ran = 0;
        for &row in marked.iter().rev() {
            if row >= tb.line_count() {
                continue;
            }
            // Each marked line is substituted on its own, by handing the
            // existing `:s` machinery a one-line range -- rather than a
            // second substitution implementation living here.
            let Some(parsed) = parse_substitute_command(body) else {
                return Err("g: not a command this can run (expected `d` or `s/.../.../`)".to_string());
            };
            let mut one = parsed?;
            one.from = LineRef::Number(row + 1);
            one.to = LineRef::Number(row + 1);
            if run_substitute(tb, &one)?.substitutions > 0 {
                ran += 1;
            }
        }
        return Ok(ran);
    }
    Err(format!("g: cannot run '{body}' on each line (expected `d` or `s/.../.../`)"))
}

/// The last `:s`, plus what the last `/` search was at the moment it
/// ran.
///
/// The second half is how "whichever pattern was used most recently"
/// gets answered without a clock. vim keeps one shared last-used
/// pattern that both `/` and `:s` write, so `:s/a/b/` then `/cCc` then
/// `:s//W/` substitutes `cCc` -- confirmed by running vim. Comparing the
/// search pattern now against the one recorded here says whether a
/// search has happened since, which is the same answer.
#[derive(Debug, Clone)]
struct LastSubstitute {
    cmd: SubstituteCmd,
    search_then: String,
}

/// Fills in the parts of a `:s` that refer to an earlier one: `&`
/// (that command's flags), and an empty pattern (`:s//repl/`, "the last
/// one again"), which `r` points at the last *search* instead.
///
/// `&` with nothing before it is not an error -- there are simply no
/// flags to take, which is also what vim does.
fn resolve_substitute(mut cmd: SubstituteCmd, last: Option<&LastSubstitute>, last_search: &str) -> Result<SubstituteCmd, String> {
    let previous = last.map(|l| &l.cmd);
    if cmd.keep_flags
        && let Some(p) = previous
    {
        // The flags, and only the flags: a count is not one of them, and
        // vim does not carry it over.
        cmd.global = p.global;
        cmd.ignore_case = p.ignore_case;
        cmd.count_only = p.count_only;
        cmd.no_error = p.no_error;
        cmd.confirm = p.confirm;
        cmd.print = p.print;
    }
    if cmd.pattern.is_empty() {
        // Whichever of the two was used more recently, which is what
        // vim's single shared pattern register amounts to: a search
        // since the last `:s` is newer than it, otherwise the `:s` is.
        //
        // `r` does not enter into this, and that is not an oversight.
        // Its job is to prefer the last *search* pattern over the last
        // *substitute* pattern -- but `:s` writes the search pattern
        // too, so after `/cCc` then `:s/a/Z/` the "previously used
        // search pattern" is `a`, and real vim answers `:s//W/r` with
        // `a` exactly as it answers `:s//W/`. Confirmed by running all
        // four orderings. Where `r` does bite is `:&r`, which is `:~` --
        // see `repeat_substitute`, whose whole point is that it reaches
        // back deliberately rather than by recency.
        let searched_since = last.is_none_or(|l| l.search_then != last_search);
        let substituted = previous.map(|p| p.pattern.as_str()).filter(|p| !p.is_empty());
        let pattern = match searched_since {
            true if !last_search.is_empty() => last_search,
            _ => substituted.unwrap_or(last_search),
        };
        if pattern.is_empty() {
            return Err("E35: no previous regular expression".to_string());
        }
        cmd.pattern = pattern.to_string();
    }
    Ok(cmd)
}

/// `:s/.../.../c` -- one match at a time, with the cursor on it and a
/// question under it.
///
/// Its own loop rather than a flag threaded through `run_substitute`,
/// because it is a different shape entirely: that one walks the range
/// deciding nothing, and this one has to draw, ask, and wait between
/// every match. It runs at the caller's level (`run_normal_mode_
/// navigation`) for the same reason `:normal` does -- that is where the
/// pane's rect, its `VimKeys` and its redraw live.
///
/// Answers are vim's: `y` replace, `n` skip, `a` all the rest without
/// asking, `q`/`Esc` stop here, `l` this one and then stop. Anything
/// else is ignored and the same match is asked about again.
///
/// The buffer is re-borrowed at each step rather than held: drawing the
/// frame needs the whole `NavBuffer`, and the question cannot be asked
/// without drawing what it is about.
fn confirm_substitute(
    buf: &mut NavBuffer,
    vk: &VimKeys,
    cmd: &SubstituteCmd,
    rect: Rect,
    term_rows: usize,
    term_cols: usize,
    color_overrides: Option<&highlight::ColorOverrides>,
) -> Result<SubstituteOutcome, String> {
    const NOT_EDITABLE: &str = "E21: not an editable buffer";
    let (mut row, mut end, fold) = {
        let tb = buf.as_editable_mut().ok_or(NOT_EDITABLE)?;
        let last = tb.line_count().saturating_sub(1);
        let current = tb.cursor().0;
        let (row, end) = substitute_rows(cmd.from.resolve(current, last), cmd.to.resolve(current, last), cmd);
        (row, end, cmd.ignore_case.unwrap_or_else(|| tb.search_ignore_case(&cmd.pattern)))
    };
    let re = crate::regex::Regex::compile(&cmd.pattern, fold);
    let mut out = SubstituteOutcome { substitutions: 0, lines: 0, last_row: None };
    let mut ask = true;
    let mut stop = false;

    while !stop {
        let (line_count, line_len) = {
            let tb = buf.as_editable_mut().ok_or(NOT_EDITABLE)?;
            (tb.line_count(), tb.line_len(row.min(tb.line_count().saturating_sub(1))))
        };
        if row > end || row >= line_count {
            break;
        }
        let mut col = 0;
        let mut touched = false;
        while col <= line_len {
            let found = {
                let tb = buf.as_editable_mut().ok_or(NOT_EDITABLE)?;
                let chars: Vec<char> = tb.line_chars(row);
                re.find_at_with_captures(&chars, col).map(|(start, match_end, caps)| (start, match_end, expand_replacement(&cmd.replacement, &caps)))
            };
            let Some((start, match_end, replacement)) = found else { break };

            let answer = if ask {
                // The cursor goes on the match before the question is
                // drawn, so the thing being asked about is the thing
                // under the cursor -- that *is* the question.
                buf.as_editable_mut().ok_or(NOT_EDITABLE)?.set_cursor(row, start);
                render_nav_frame(buf, vk, rect, term_rows, term_cols, color_overrides);
                show_command_mode_error(&format!("replace with {replacement} (y/n/a/q/l)?"), term_rows, term_cols);
                // A read that gives nothing is the terminal going away,
                // which is a `q` as far as this loop is concerned.
                match editor::read_key_idle(&mut || {}) {
                    Ok(Some(key)) => key,
                    _ => Key::Char('q'),
                }
            } else {
                Key::Char('y')
            };
            let (replace, last) = match answer {
                Key::Char('y') => (true, false),
                Key::Char('n') => (false, false),
                // Every remaining match, without asking again.
                Key::Char('a') => {
                    ask = false;
                    (true, false)
                }
                Key::Char('l') => (true, true),
                Key::Char('q') | Key::Escape => (false, true),
                // vim ignores a key it has no answer for and asks the
                // same question again.
                _ => continue,
            };
            if replace {
                let tb = buf.as_editable_mut().ok_or(NOT_EDITABLE)?;
                let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, start), to: (row, match_end) };
                tb.delete_range(&range);
                tb.insert_text((row, start), &replacement);
                out.substitutions += 1;
                out.last_row = Some(row);
                touched = true;
                // A replacement may carry newlines (a `\r` in the
                // replacement text), which pushes the rest of the range
                // down with it.
                let added = replacement.matches('\n').count();
                end += added;
                // `.max(start + 1)` so an empty replacement of an empty
                // match still advances, rather than asking about the
                // same position forever.
                col = (start + replacement.chars().count()).max(start + 1);
                if added > 0 {
                    row += added;
                    col = 0;
                }
            } else {
                col = match_end.max(start + 1);
            }
            if last {
                stop = true;
                break;
            }
            if !cmd.global {
                break;
            }
        }
        if touched {
            out.lines += 1;
        }
        row += 1;
    }
    if out.substitutions == 0 && !cmd.no_error {
        return Err(format!("E486: Pattern not found: {}", cmd.pattern));
    }
    Ok(out)
}

/// The rows a `:s` acts on, from its already-resolved range ends.
///
/// A trailing count replaces the range instead of narrowing it: vim
/// reads `:1,2s/x/y/ 3` as "three lines from line 2", which can reach
/// past where the range ended. See `SubstituteCmd::count`.
fn substitute_rows(a: usize, b: usize, cmd: &SubstituteCmd) -> (usize, usize) {
    match cmd.count {
        Some(n) => (a.max(b), a.max(b).saturating_add(n - 1)),
        None => (a.min(b), a.max(b)),
    }
}

/// One line as `p`, `#` or `l` would show it.
fn printed_line(tb: &TextBuffer, row: usize, style: PrintStyle) -> String {
    let text: String = tb.line_chars(row).into_iter().collect();
    match style {
        PrintStyle::Plain => text,
        // vim right-aligns the number in three columns and follows it
        // with one space.
        PrintStyle::Numbered => format!("{:>3} {text}", row + 1),
        // `:list` spelling: `$` marks where the line ends, so trailing
        // whitespace is visible, and a tab shows as the two characters
        // it would be typed as rather than as a jump the eye cannot
        // measure.
        PrintStyle::List => format!("{}$", text.replace('\\', "\\\\").replace('\t', "^I")),
    }
}

/// What one `:s` did, for the caller's report line and for `p`/`#`/`l`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubstituteOutcome {
    substitutions: usize,
    lines: usize,
    /// The last row a substitution touched, which is the line `p`, `#`
    /// and `l` print -- vim shows where it finished, not where it began.
    last_row: Option<usize>,
}

fn run_substitute(tb: &mut TextBuffer, cmd: &SubstituteCmd) -> Result<SubstituteOutcome, String> {
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
    let (mut row, mut end) = substitute_rows(a, b, cmd);
    // `i`/`I` when either was given, and only otherwise the
    // 'ignorecase'/'smartcase' answer the buffer would have used on its
    // own.
    let fold = cmd.ignore_case.unwrap_or_else(|| tb.search_ignore_case(&cmd.pattern));
    let re = crate::regex::Regex::compile(&cmd.pattern, fold);
    let mut total = 0usize;
    let mut lines_changed = 0usize;
    let mut last_row = None;
    while row <= end && row < tb.line_count() {
        let original: String = tb.line_chars(row).into_iter().collect();
        let (new_text, count) = substitute_line(&original, &re, &cmd.replacement, cmd.global);
        if count > 0 {
            total += count;
            lines_changed += 1;
            last_row = Some(row);
            // `n` counts and leaves the buffer alone -- the whole point
            // of asking is to find out before deciding.
            if !cmd.count_only {
                let line_len = original.chars().count();
                let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (row, 0), to: (row, line_len) };
                tb.delete_range(&range);
                tb.insert_text((row, 0), &new_text);
                let added = new_text.matches('\n').count();
                end += added;
                row += 1 + added;
                continue;
            }
        }
        row += 1;
    }
    if total == 0 && !cmd.no_error {
        return Err(format!("E486: Pattern not found: {}", cmd.pattern));
    }
    Ok(SubstituteOutcome { substitutions: total, lines: lines_changed, last_row })
}

// Command mode allows full control-flow syntax (if/while/for/etc -- every
// leaf command still funnels through the same restrict_to_builtins gate
// regardless of nesting) but not `(...)` subshells, coproc, function
// definitions, or multi-stage `|` pipelines: the first three self-exec (or
// register something persistent) rather than running through that gate,
// and would bypass the restriction entirely; function definitions would
// leak a callable function out into normal shell mode later. Walked
// recursively since these can be nested inside a control-flow body.
// bash's `HISTCONTROL=ignorespace`, except always on, the way fish has
// it -- there is no option here and none is planned. bash's default is
// to record everything, which is a 1991 default nobody keeps; the
// gesture is only useful if you can rely on it without having
// configured anything first, and a shell that sometimes remembers your
// password is worse than one that never does.
//
// Tab and space both count, since either is what a terminal produces
// when someone deliberately indents a line to hide it.
//
// Decided on the *first* line of a fresh command only. A continuation
// line's own leading whitespace is content -- a heredoc body's
// indentation, most obviously -- and has nothing to say about whether
// the command it belongs to should be remembered.
fn starts_off_the_record(first_line: &str) -> bool {
    first_line.starts_with([' ', '\t'])
}

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

    // The other half of the same report, and the half that was still
    // broken: a program that paints the whole grid parks its cursor
    // wherever it likes -- vim leaves it on line 1 -- and clipping the
    // addressable range at the cursor row showed one line of a full
    // screen. See `addressable_len`.
    #[test]
    fn a_full_screen_program_is_addressable_below_its_own_cursor() {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(6, 20)));
        {
            let mut s = screen.borrow_mut();
            s.feed(b"\x1b[?1049h");
            // Paint every row, then send the cursor back to the top --
            // exactly what a full-screen editor does before waiting for
            // a key.
            for row in 0..6 {
                s.feed(format!("\x1b[{};1Hrow {row}", row + 1).as_bytes());
            }
            s.feed(b"\x1b[1;1H");
            assert_eq!(s.cursor().0, 0, "cursor parked at the top, content below it");
        }
        let buf = ScreenBuffer::new(screen, 6);
        assert_eq!(buf.line_count(), 6, "the whole painted screen, not just the cursor's row");
        assert_eq!(buf.line_chars(5).into_iter().collect::<String>(), "row 5");
        // Entering does not read as "5 new lines arrived" either.
        assert_eq!(buf.new_lines(), 0);
    }

    // ...and the rule that was there before still holds where it was
    // right: a shell pane's rows below the cursor really are untouched
    // padding, and counting them would let `G` walk into empty space.
    #[test]
    fn a_shell_panes_blank_rows_below_the_cursor_are_still_not_content() {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(10, 20)));
        {
            let mut s = screen.borrow_mut();
            s.feed(b"one\r\ntwo\r\nthree$ ");
        }
        let buf = ScreenBuffer::new(screen, 10);
        assert_eq!(buf.line_count(), 3, "three written rows, seven blank ones below");
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
mod completion_menu_geometry_tests {
    use super::*;

    // A window split top/bottom, focus on the lower pane.
    fn split_window() -> WindowEntry {
        let mut w = WindowEntry::single(0, Frame::Session(0));
        w.focused_pane = w.add_pane(vec![Frame::Session(0)], true, false);
        w
    }

    #[test]
    fn an_unpromoted_terminal_has_no_absolute_menu_row() {
        // The relative `\n`/draw/`\x1b[1A` dance is the only option
        // there, and it's safe precisely because there are no panes.
        let w = WindowEntry::single(0, Frame::Session(0));
        assert_eq!(completion_menu_rows(&w, false, 0, 24, 80), None);
    }

    #[test]
    fn an_unsplit_pane_menu_sits_below_the_prompt_and_above_the_tab_bar() {
        let w = WindowEntry::single(0, Frame::Session(0));
        // content_rows(24) == 22, so the pane owns rows 0..=21.
        let (prompt_row, bottom) = completion_menu_rows(&w, true, 3, 24, 80).unwrap();
        assert_eq!((prompt_row, bottom), (3, 21));
    }

    #[test]
    fn a_split_pane_gets_a_menu_row_bounded_by_its_own_pane() {
        // Regression: this returned None for any split window, so the
        // menu was skipped entirely there -- Tab still cycled candidates
        // but nothing showed them. The absolute path clamps to the pane
        // on both axes (this row pair vertically, focused_col_origin
        // horizontally), so a split needs no more than an unsplit one.
        let w = split_window();
        let rect = pane_rect(&w, 1, 24, 80);
        let (prompt_row, bottom) = completion_menu_rows(&w, true, 2, 24, 80).unwrap();
        assert_eq!(prompt_row, rect.row + 2, "the prompt row is the pane's own, not the terminal's");
        assert_eq!(bottom, rect.row + rect.rows - 1, "and the menu can never be drawn past this pane");
        assert!(bottom < content_rows(24), "which is above the global status row and the tab bar");
        // The lower pane genuinely starts partway down the screen -- if
        // it didn't, this test would pass without proving anything.
        assert!(rect.row > 0);
    }

    #[test]
    fn a_prompt_on_the_panes_last_row_leaves_no_room_and_the_menu_is_skipped() {
        // redraw_with_completion_row draws at prompt_row + 1 and skips
        // when that exceeds the bottom -- the documented graceful
        // degradation, which this geometry has to keep reachable rather
        // than reporting a row outside the pane.
        let w = split_window();
        let rect = pane_rect(&w, 1, 24, 80);
        let (prompt_row, bottom) = completion_menu_rows(&w, true, rect.rows - 1, 24, 80).unwrap();
        assert_eq!(prompt_row, bottom);
        assert!(prompt_row + 1 > bottom, "no room below, so the menu row is skipped");
    }

    #[test]
    fn the_menu_never_spills_horizontally_either() {
        // The other half of the clamp: the row is drawn at col_origin for
        // `width` columns, both of which are the focused pane's own.
        let w = split_window();
        let (col_origin, width) = focused_col_origin(&w, true, 24, 80);
        let rect = pane_rect(&w, 1, 24, 80);
        assert_eq!((col_origin, width), (rect.col, rect.cols));
        assert!(col_origin + width <= 80);
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

    // A window's label is a path now, not a pre-abbreviated one, so the
    // bar is where it is made to fit -- a bar of full paths would run off
    // the end of the line instead of ending at it.
    #[test]
    fn a_tabs_label_is_fitted_to_its_share_of_the_bar() {
        // `" [0]  "` -- the id, its brackets and the spaces either side --
        // plus the column that keeps the bar off the line's own end.
        assert_eq!(tab_label_budget(80, 1), 80 - 7);
        assert_eq!(tab_label_budget(80, 2), 40 - 7);
        assert_eq!(tab_label_budget(80, 8), 10 - 7);
        // Narrower than the decoration: nothing left for a label, and no
        // underflow either.
        assert_eq!(tab_label_budget(4, 2), 0);

        let deep = prompt::display_path("/home/jussi/work/clients/acme/backend/services/billing", "/home/jussi");
        let one = prompt::fit_path(&deep, tab_label_budget(80, 1));
        let four = prompt::fit_path(&deep, tab_label_budget(80, 4));
        // One tab has room for the path as it is.
        assert_eq!(one, deep);
        assert_eq!(four, "~/…/billing", "a quarter of the bar still says which directory it is");
        for (label, windows) in [(one, 1), (four, 4)] {
            assert!(editor::visible_len(&tab_segment_text(1, &label)) <= 80 / windows, "{windows} tabs: {label:?} does not fit its share");
        }
    }

    // The dialog is the shell's, not a pane's, so it is placed against
    // the terminal rather than against anything inside it.
    // Both halves of "which destinations, starting where", including
    // every way the two options can disagree with each other.
    #[test]
    fn the_opener_offers_the_destinations_the_glob_matches() {
        use crate::opener::Destination;
        let read = |opts: &str| {
            let mut shell = exec::Shell::new();
            shell.run_source_here(opts, "<test>");
            opener_destinations(&shell)
        };
        // Shipped: all four, starting on a tab, which is what the
        // opener did before it could be asked.
        assert_eq!(read(""), (Destination::ALL.to_vec(), Destination::Tab));
        // A glob over their names, as `tabular` globs over languages.
        assert_eq!(
            read("bishopt --set opener_targets '*split'"),
            (vec![Destination::VSplit, Destination::HSplit], Destination::VSplit),
            "the start falls to the first survivor when the one asked for was excluded"
        );
        assert_eq!(read("bishopt --set opener_targets here"), (vec![Destination::Here], Destination::Here));
        // The start on its own.
        assert_eq!(read("bishopt --set opener_target hsplit").1, Destination::HSplit);
        assert_eq!(read("bishopt --set opener_target here; bishopt --set opener_targets '*'").1, Destination::Here);
        // Neither option can brick the dialog. A glob matching nothing
        // is a typo, and a dialog that cannot open anything is a worse
        // answer to it than ignoring it; a start naming nothing lands
        // on the first thing that is enabled.
        assert_eq!(read("bishopt --set opener_targets nosuchthing"), (Destination::ALL.to_vec(), Destination::Tab));
        assert_eq!(read("bishopt --set opener_target nosuchthing").1, Destination::Tab);
        assert_eq!(read("bishopt --set opener_target nope; bishopt --set opener_targets hsplit"), (vec![Destination::HSplit], Destination::HSplit));
    }

    #[test]
    fn the_opener_dialog_stays_inside_what_the_compositor_repaints() {
        // 80x24, a long list: three quarters of the width, and two rows
        // short of the content area, centred in it.
        let r = opener_dialog_rect(143, 24, 80);
        assert_eq!((r.row, r.col, r.rows, r.cols), (1, 10, 20, 60));
        // A short list gets a short dialog, still centred.
        let r = opener_dialog_rect(2, 24, 80);
        assert_eq!((r.row, r.col, r.rows, r.cols), (8, 10, 6, 60));
        // Wide terminal: capped, so the dialog does not become a wall.
        let r = opener_dialog_rect(143, 50, 200);
        assert_eq!((r.row, r.col, r.rows, r.cols), (1, 50, 46, 100));
        // Narrow terminal: the floor wins, up to the width there is.
        assert_eq!(opener_dialog_rect(143, 10, 30).cols, 24);
        assert_eq!(opener_dialog_rect(143, 10, 20).cols, 20, "never wider than the terminal");

        // The property the dialog's own bottom border was left behind
        // for: the last two rows of the terminal are the global status
        // row and the pinned tab bar, and `compositor_redraw` paints
        // neither the first of those nor anything below the content
        // area -- so a dialog that reaches there is a dialog that never
        // gets cleaned up.
        for term_rows in 1..=60 {
            let r = opener_dialog_rect(143, term_rows, 80);
            assert!(
                r.row + r.rows <= content_rows(term_rows),
                "{term_rows} rows: the dialog ends at {} and the content area at {}",
                r.row + r.rows,
                content_rows(term_rows)
            );
            assert!(r.rows >= 1 && r.cols >= 1, "{term_rows} rows: nothing to draw");
        }
        // Absurdly narrow, which only has to not wrap the arithmetic.
        for (rows, cols) in [(4, 10), (1, 1), (0, 0), (2, 3)] {
            let r = opener_dialog_rect(143, rows, cols);
            assert!(r.col + r.cols <= cols, "{rows}x{cols} -> {:?}", (r.row, r.col, r.rows, r.cols));
        }
    }

    // The terminal's own title answers the same question the tab bar
    // does -- which window is this -- except that a program in the pane
    // may already have answered it.
    #[test]
    fn a_program_that_named_its_window_keeps_that_name() {
        // vim said so; bish has nothing to add.
        assert_eq!(window_title("README.md (~/bish) - VIM", None, "/home/jussi/bish", "/home/jussi"), "README.md (~/bish) - VIM");
        // Said so *and* the window has a name of its own: the program
        // wins, because it is what is actually on screen.
        assert_eq!(window_title("htop", Some("monitors"), "/home/jussi", "/home/jussi"), "htop");
        // Nothing said: the window's own name, prefixed so that a tab
        // running bish is recognisable among a row of them.
        assert_eq!(window_title("", Some("monitors"), "/home/jussi/bish", "/home/jussi"), "bish: monitors");
        // Nothing said and no name: where it is.
        assert_eq!(window_title("", None, "/home/jussi/bish", "/home/jussi"), "bish: ~/bish");
        assert_eq!(window_title("", None, "/usr/local/bin", "/home/jussi"), "bish: /usr/local/bin");
        // Unlike the tab bar, a title has no budget to fit -- the
        // terminal shortens it however its own tabs need, and it is the
        // only one that knows how much room they have.
        let deep = "/home/jussi/work/clients/acme/backend/services/billing";
        assert_eq!(window_title("", None, deep, "/home/jussi"), format!("bish: {}", prompt::display_path(deep, "/home/jussi")));
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
            divider_sgr: String::new(),
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
            divider_sgr: String::new(),
            panes: vec![PaneSnapshot { rect: Rect { row: 0, col: 0, rows: 2, cols: 3 }, screen, focused: true }],
            dividers: vec![],
        };
        let frame = TerminalFrame::capture(&layout, "", 2, 3);
        assert_eq!(frame.cells[0].ch, 'a');
        assert_eq!(frame.cells[1].ch, 'b');
    }
}

#[cfg(test)]
mod normal_mode_status_tests {
    use super::*;

    fn nav_buffer(rows: usize, seed: &[u8]) -> ScreenBuffer {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(rows, 200)));
        screen.borrow_mut().feed(seed);
        ScreenBuffer::new(screen, rows)
    }

    fn status(buf: &ScreenBuffer, cols: usize) -> String {
        let plain = normal_mode_status_text(buf, &VimKeys::new(), None, cols);
        // The chips carry their own SGR; strip it to assert on text.
        let mut out = String::new();
        let mut chars = plain.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                for c2 in chars.by_ref() {
                    if c2 == 'm' {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn the_mode_line_always_says_how_to_get_back_out() {
        // The whole point: Ctrl+G into normal mode over a shell pane
        // is not something Escape or `q` gets you out of, so the way out
        // has to be on screen rather than known in advance.
        let buf = nav_buffer(10, b"$ echo hi\r\n");
        let text = status(&buf, 100);
        assert!(text.contains(NORMAL_MODE_EXIT_HINT), "{text:?}");
        assert!(text.starts_with("-- NORMAL --"), "{text:?}");
    }

    #[test]
    fn lines_arriving_while_you_read_are_reported() {
        let mut buf = nav_buffer(10, b"$ tail -f log\r\n");
        assert_eq!(buf.new_lines(), 0);
        assert!(!status(&buf, 100).contains("new line"));
        // Two more lines land in the pane after the excursion started.
        buf.screen.borrow_mut().feed(b"one\r\ntwo\r\n");
        assert_eq!(buf.new_lines(), 2);
        let text = status(&buf, 100);
        assert!(text.contains("+2 new lines"), "{text:?}");
        assert!(text.contains(NORMAL_MODE_EXIT_HINT), "the way out doesn't stop being shown: {text:?}");
        assert!(buf.new_lines_changed(), "the idle loop needs to know to redraw");
        assert!(!buf.new_lines_changed(), "...but only once per change");
    }

    #[test]
    fn one_new_line_is_singular() {
        let buf = nav_buffer(10, b"$ \r\n");
        buf.screen.borrow_mut().feed(b"only\r\n");
        assert!(status(&buf, 100).contains("+1 new line "), "{:?}", status(&buf, 100));
    }

    #[test]
    fn a_narrow_pane_shortens_the_hint_before_dropping_it() {
        let buf = nav_buffer(10, b"$ \r\n");
        // Wide: the sentence. Narrow: the short form. Narrower still:
        // nothing but the mode label and position, as before.
        assert!(status(&buf, 100).contains(NORMAL_MODE_EXIT_HINT));
        let narrow = status(&buf, 44);
        assert!(narrow.contains(NORMAL_MODE_EXIT_HINT_SHORT), "{narrow:?}");
        assert!(!narrow.contains(NORMAL_MODE_EXIT_HINT), "{narrow:?}");
        let tiny = status(&buf, 26);
        assert!(!tiny.contains("return"), "{tiny:?}");
        assert!(tiny.starts_with("-- NORMAL --"), "{tiny:?}");
    }

    #[test]
    fn every_width_produces_exactly_that_many_visible_columns() {
        let buf = nav_buffer(10, b"$ \r\n");
        buf.screen.borrow_mut().feed(b"a\r\nb\r\n");
        for cols in [10usize, 20, 26, 44, 60, 80, 120] {
            let text = normal_mode_status_text(&buf, &VimKeys::new(), None, cols);
            assert_eq!(editor::visible_len(&text), cols, "width {cols} produced {text:?}");
        }
    }

    #[test]
    fn a_command_line_being_typed_takes_the_whole_row() {
        let buf = nav_buffer(10, b"$ \r\n");
        buf.screen.borrow_mut().feed(b"late\r\n");
        let text = normal_mode_status_text(&buf, &VimKeys::new(), Some(":w foo"), 100);
        assert!(text.starts_with(":w foo"), "{text:?}");
        assert!(!text.contains("return"), "no chips over a live command line: {text:?}");
        assert!(!text.contains("new line"), "{text:?}");
    }
}

#[cfg(test)]
mod compositor_frame_output_tests {
    use super::*;
    // Only this module builds a layout by hand.

    #[test]
    fn a_folded_divider_draws_what_it_stands_for() {
        let line = folded_divider_line(30, 6);
        assert!(line.contains("6 panes"), "{line:?}");
        assert_eq!(editor::visible_len(&line), 30, "still exactly the divider's width");
        assert!(folded_divider_line(30, 1).contains("1 pane"), "singular");
    }

    // Regression: `bish --promoted`, then Ctrl-C, visibly flashed the
    // prompt. read_line draws the prompt straight to the real terminal
    // and never into the session's grid, so the full repaint that
    // follows Ctrl-C painted that row blank -- taking the `^C` with it --
    // and the fresh prompt then drew over the top, one blank frame
    // later. Recording the abandoned line into the grid first makes the
    // repaint draw what's already on screen, so there's nothing to
    // flash, and leaves the `^C` in the scrollback where bash leaves it.
    #[test]
    fn an_interrupted_line_is_recorded_in_the_grid_the_way_the_terminal_shows_it() {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(4, 40)));
        let mut session = SessionState {
            lsp: Rc::new(RefCell::new(lspclient::Table::default())),
            lsp_roots: HashMap::new(),
            lsp_semantic: HashMap::new(),
            lsp_semantic_applied: HashMap::new(),
            lsp_highlight: None,
            lsp_highlight_applied: None,
            lsp_inlay: None,
            lsp_inlay_applied: None,
            shell: exec::Shell::new(),
            buffer: String::new(),
            buffer_unrecorded: false,
            history: Rc::new(RefCell::new(History::load("/dev/null", 0))),
            screen: screen.clone(),
            warned_stopped_jobs: false,
            dir_history: Vec::new(),
            dir_history_index: 0,
            command_transcript: Vec::new(),
        };
        freeze_interrupted_line(&mut session, "echo hi");
        let row0: String = (0..40).map(|c| screen.borrow().cell(0, c).ch).collect::<String>().trim_end().to_string();
        assert!(row0.ends_with("echo hi^C"), "the abandoned line and its ^C both stay: {row0:?}");
        assert_eq!(screen.borrow().cursor().0, 1, "and the next prompt gets the row below it");
    }

    // Regression: an editor frame (`Frame::Edit`/`Frame::Hex`) used to
    // wipe its session's whole grid on quit (`\x1b[2J\x1b[H`), destroying
    // the shell scrollback underneath it -- the prompt, the `e` command
    // that opened it, everything before that -- so a `bish --promoted`,
    // `e FILE`, `:q!` sequence came back to an empty pane. It now uses
    // the alternate screen `vt100::Screen` already implements, which is
    // what a real full-screen terminal program does and what leaves the
    // primary buffer (and its cursor) exactly as it was found.
    #[test]
    fn leaving_an_editor_frame_restores_the_shell_scrollback_under_it() {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(4, 40)));
        let rows = || -> Vec<String> {
            let s = screen.borrow();
            (0..4).map(|r| (0..40).map(|c| s.cell(r, c).ch).collect::<String>().trim_end().to_string()).collect()
        };
        screen.borrow_mut().feed(b"$ echo hi\r\nhi\r\n$ e f.bash\r\n");
        let before = rows();
        let cursor_before = screen.borrow().cursor();
        assert_eq!(before[2], "$ e f.bash");

        enter_alt_screen(&screen);
        // Whatever the editor freezes in while it owns the pane lands on
        // the alternate buffer, not on top of any of that.
        screen.borrow_mut().feed(b"\x1b[H  1 export FOO=1");
        assert_ne!(rows(), before, "the alternate buffer is what shows while the editor is up");

        leave_alt_screen(&screen);
        assert_eq!(rows(), before, "quitting puts the pane back exactly as it was found");
        assert_eq!(screen.borrow().cursor(), cursor_before, "including the row the next prompt draws on");
    }

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
    fn minimizing_collapses_the_focused_pane_and_moves_off_it() {
        // Built directly rather than through a pty: this is layout
        // arithmetic, and a screen-scrape cannot tell "collapsed to a
        // divider" from "scrolled out of view".
        let mut w = WindowEntry::single(1, Frame::Session(0));
        let top = w.focused_pane;
        let bottom = w.add_pane(vec![Frame::Session(0)], false, false);
        w.focused_pane = bottom;

        minimize_focused_pane(&mut w);

        let minimized_of = |w: &WindowEntry, id: PaneId| match &w.layout {
            PaneLayout::Split { children, .. } => children.iter().find(|c| matches!(c.layout, PaneLayout::Leaf(l) if l == id)).unwrap().minimized,
            PaneLayout::Leaf(_) => unreachable!(),
        };
        assert!(minimized_of(&w, bottom), "the focused pane collapses");
        assert!(!minimized_of(&w, top), "its neighbour does not");
        assert_ne!(w.focused_pane, bottom, "focus leaves it -- one row is nowhere to type");

        // Going back to it is what restores it; there is no second
        // command to remember.
        restore_if_minimized(&mut w, bottom);
        assert!(!minimized_of(&w, bottom));
    }

    #[test]
    fn a_lone_pane_will_not_minimize_itself_out_of_view() {
        // Nowhere for focus to go, and a window showing nothing but a
        // divider is not a state worth being able to reach.
        let mut w = WindowEntry::single(1, Frame::Session(0));
        let only = w.focused_pane;
        minimize_focused_pane(&mut w);
        assert_eq!(w.focused_pane, only);
        assert!(matches!(w.layout, PaneLayout::Leaf(_)), "layout untouched");
    }

    #[test]
    fn the_prompt_only_claims_the_mouse_once_it_has_something_to_do_with_it() {
        // Unpromoted: nothing to hit-test and nowhere to scroll, so
        // claiming reporting would take the terminal's wheel and
        // selection and give nothing back.
        assert!(!prompt_claims_mouse(true, false));
        // Promoted: tabs, panes and dividers exist and are clickable.
        assert!(prompt_claims_mouse(true, true));
        // The bishopt still wins in both directions -- `mouse off` is
        // how you ask for the terminal's own selection back even in a
        // split.
        assert!(!prompt_claims_mouse(false, true));
        assert!(!prompt_claims_mouse(false, false));
    }

    #[test]
    fn build_compositor_frame_output_never_erases_the_whole_display() {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(2, 3)));
        let layout = CompositorLayout {
            divider_sgr: String::new(),
            panes: vec![PaneSnapshot { rect: Rect { row: 0, col: 0, rows: 2, cols: 3 }, screen, focused: true }],
            dividers: vec![],
        };
        let out = build_compositor_frame_output(&layout, "tab", 3);
        assert!(!out.contains("\x1b[2J"), "{out:?}");
    }

    #[test]
    fn build_compositor_frame_output_still_paints_every_row_and_the_tab_bar() {
        let screen = Rc::new(RefCell::new(vt100::Screen::new(2, 3)));
        screen.borrow_mut().feed(b"ab");
        let layout = CompositorLayout {
            divider_sgr: String::new(),
            panes: vec![PaneSnapshot { rect: Rect { row: 0, col: 0, rows: 2, cols: 3 }, screen, focused: true }],
            dividers: vec![],
        };
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
    fn parse_line_command_reads_the_fold_commands_and_their_abbreviations() {
        use LineCommand::*;
        let fold = |from, to, action| Some(Fold { from, to, action });
        assert_eq!(parse_line_command("2,3fold"), fold(LineRef::Number(2), LineRef::Number(3), FoldAction::Create));
        assert_eq!(parse_line_command("fo"), fold(LineRef::Current, LineRef::Current, FoldAction::Create));
        assert_eq!(parse_line_command("%foldopen!"), fold(LineRef::Number(1), LineRef::Last, FoldAction::Open { all: true }));
        assert_eq!(parse_line_command("foldc"), fold(LineRef::Current, LineRef::Current, FoldAction::Close { all: false }));
        assert_eq!(parse_line_command("fold!"), None, "`:fold` takes no `!`");
        assert_eq!(parse_line_command("foldx"), None);
        assert_eq!(parse_line_command("format"), None, "a word that is not one goes on to the other commands");
    }

    #[test]
    fn parse_line_command_recognizes_an_address_a_delete_a_move_and_normal() {
        use LineCommand::*;
        assert_eq!(parse_line_command(">"), Some(Shift { from: LineRef::Current, to: LineRef::Current, right: true, times: 1 }));
        assert_eq!(parse_line_command(">>"), Some(Shift { from: LineRef::Current, to: LineRef::Current, right: true, times: 2 }));
        assert_eq!(parse_line_command("1,2<"), Some(Shift { from: LineRef::Number(1), to: LineRef::Number(2), right: false, times: 1 }));
        assert_eq!(parse_line_command("1t$"), Some(Copy { from: LineRef::Number(1), to: LineRef::Number(1), dest: LineRef::Last }));
        assert_eq!(parse_line_command("2"), Some(Goto(LineRef::Number(2))));
        assert_eq!(parse_line_command("$"), Some(Goto(LineRef::Last)));
        assert_eq!(parse_line_command("2d"), Some(Delete { from: LineRef::Number(2), to: LineRef::Number(2) }));
        assert_eq!(parse_line_command("2,4d"), Some(Delete { from: LineRef::Number(2), to: LineRef::Number(4) }));
        assert_eq!(parse_line_command("%d"), Some(Delete { from: LineRef::Number(1), to: LineRef::Last }));
        assert_eq!(parse_line_command("1m$"), Some(Move { from: LineRef::Number(1), to: LineRef::Number(1), dest: LineRef::Last }));
        assert_eq!(parse_line_command("1,2m0"), Some(Move { from: LineRef::Number(1), to: LineRef::Number(2), dest: LineRef::Number(0) }));
        assert_eq!(parse_line_command("normal dd"), Some(Normal("dd".to_string())));
        assert_eq!(parse_line_command("norm x"), Some(Normal("x".to_string())));

        // Everything else still belongs to the ordinary dispatch, which
        // only ever sees it if this says no.
        // A bare `d` and a bare `>` mean the current line, as they do
        // in vim -- the rest still belong to the ordinary dispatch,
        // which only ever sees them if this says no.
        assert_eq!(parse_line_command("d"), Some(Delete { from: LineRef::Current, to: LineRef::Current }));
        for other in ["w", "wq", "q!", "set number", "diag", "2dd", "2x", "1m", "normal"] {
            assert_eq!(parse_line_command(other), None, "{other:?}");
        }
    }

    // `:preview` was a documented command that the colon line painted
    // red and would not complete, because COLON_COMMANDS is a copy of a
    // `match` and copies drift. This is what stops the next one:
    // everything the help screen tells you to type has to be a name the
    // line knows.
    #[test]
    fn every_documented_colon_command_is_one_the_line_knows() {
        // The two documented entries that are not command *words*:
        // `:!CMD` runs a shell command, and `:s/PAT/REPL/` reaches its
        // own parser before the dispatch ever splits a word off.
        let not_words = ["!CMD", "s"];
        let chars: Vec<char> = EDITOR_HELP_MARKDOWN.chars().collect();
        let mut documented = Vec::new();
        for i in 0..chars.len().saturating_sub(1) {
            if chars[i] != '`' || chars[i + 1] != ':' {
                continue;
            }
            let name: String = chars[i + 2..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == '!').collect();
            if !name.is_empty() {
                documented.push(name);
            }
        }
        assert!(documented.len() > 5, "the help screen's colon table should have been found: {documented:?}");
        for name in documented {
            if not_words.contains(&name.as_str()) {
                continue;
            }
            assert!(COLON_COMMANDS.contains(&name.as_str()), "`:{name}` is documented but the colon line does not know it");
        }
    }

    #[test]
    fn ex_shift_and_copy_change_the_buffer() {
        let mut buf = buffer_of("a\nb\n");
        run_line_command(&mut buf, &LineCommand::Shift { from: LineRef::Current, to: LineRef::Current, right: true, times: 1 }).unwrap();
        assert_eq!(lines_of(&buf), "    a\nb\n");
    }

    fn buffer_of(text: &str) -> TextBuffer {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), text);
        buf.set_cursor(0, 0);
        buf
    }

    fn lines_of(buf: &TextBuffer) -> String {
        (0..buf.line_count()).map(|l| buf.line_chars(l).into_iter().collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn run_line_command_goes_to_a_line_deletes_a_range_and_moves_one() {
        let mut buf = buffer_of("a\nb\nc");
        run_line_command(&mut buf, &LineCommand::Goto(LineRef::Number(2))).unwrap();
        assert_eq!(buf.cursor(), (1, 0));
        // A bare address lands on the first non-blank, not column zero.
        let mut buf = buffer_of("a\n    b");
        run_line_command(&mut buf, &LineCommand::Goto(LineRef::Last)).unwrap();
        assert_eq!(buf.cursor(), (1, 4));

        let mut buf = buffer_of("a\nb\nc\nd");
        run_line_command(&mut buf, &LineCommand::Delete { from: LineRef::Number(2), to: LineRef::Number(3) }).unwrap();
        assert_eq!(lines_of(&buf), "a\nd");

        // `:1m$` -- to the very end.
        let mut buf = buffer_of("one\ntwo");
        run_line_command(&mut buf, &LineCommand::Move { from: LineRef::Number(1), to: LineRef::Number(1), dest: LineRef::Last }).unwrap();
        assert_eq!(lines_of(&buf), "two\none");

        // `:3m0` -- "after line zero" is the top, which is why the
        // destination is a line number and not a row.
        let mut buf = buffer_of("one\ntwo\nthree");
        run_line_command(&mut buf, &LineCommand::Move { from: LineRef::Number(3), to: LineRef::Number(3), dest: LineRef::Number(0) }).unwrap();
        assert_eq!(lines_of(&buf), "three\none\ntwo");

        // A range moved past itself, and the same range moved into
        // itself, which vim refuses.
        let mut buf = buffer_of("a\nb\nc\nd");
        run_line_command(&mut buf, &LineCommand::Move { from: LineRef::Number(1), to: LineRef::Number(2), dest: LineRef::Last }).unwrap();
        assert_eq!(lines_of(&buf), "c\nd\na\nb");
        let mut buf = buffer_of("a\nb\nc");
        let into_itself = LineCommand::Move { from: LineRef::Number(1), to: LineRef::Number(2), dest: LineRef::Number(1) };
        assert!(run_line_command(&mut buf, &into_itself).is_err());
        assert_eq!(lines_of(&buf), "a\nb\nc", "and the buffer is left alone");
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

    // `!` at the head of the line, with or without a range in front of
    // it -- and nothing else in this dispatch mistaken for one.
    #[test]
    fn a_leading_bang_is_a_shell_command_and_a_range_before_it_is_a_filter() {
        assert!(matches!(parse_bang_command("!echo %"), Some(BangCommand::Run("echo %"))));
        // Everything after the `!` is the command, spaces and all --
        // `:! ls` is `: ls`, which the shell reads the same way.
        assert!(matches!(parse_bang_command("! ls"), Some(BangCommand::Run(" ls"))));
        assert!(matches!(parse_bang_command("!"), Some(BangCommand::Run(""))));

        for line in ["%!sort", "1,5!sort", ".!date", "$!tac", "1,$!tac"] {
            assert!(matches!(parse_bang_command(line), Some(BangCommand::Filter { .. })), "{line}");
        }
        // The range and the command both come back, and everything past
        // the `!` is the command.
        let Some(BangCommand::Filter { from, to, cmd }) = parse_bang_command("1,5!column -t") else { panic!() };
        assert_eq!((from, to), (LineRef::Number(1), LineRef::Number(5)));
        assert_eq!(cmd, "column -t");
        let Some(BangCommand::Filter { from, to, .. }) = parse_bang_command("%!sort") else { panic!() };
        assert_eq!((from, to), (LineRef::Number(1), LineRef::Last));
        // Not a range this parser reads, so not a filter either -- see
        // parse_range_prefix's own note on visual-mode marks.
        assert!(parse_bang_command("'<,'>!sort").is_none());

        // Not a bang command at all, and the ordinary dispatch must
        // still see every one of these unchanged.
        for line in ["w", "wq", "q!", "s/a/b/", "normal dd", "1,5d", "diag", "echo hi!"] {
            assert!(parse_bang_command(line).is_none(), "{line}");
        }
    }

    // The two edges a filter's result has to land on, neither of which
    // needs a shell to reach: the command's trailing newline, and the
    // empty line a whole-buffer delete leaves behind. Checked against
    // real vim, which is where the `:%!true` expectation comes from.
    #[test]
    fn a_filters_output_replaces_exactly_the_lines_it_was_given() {
        let mut buf = buffer_of("c\na\nb");
        replace_lines(&mut buf, 0, 2, "a\nb\nc\n");
        assert_eq!(buf.line_count(), 3, "the trailing newline ends the last line, it does not add one");
        assert_eq!(lines_of(&buf), "a\nb\nc");

        // One line in the middle, and the rest untouched.
        let mut buf = buffer_of("c\na\nb");
        replace_lines(&mut buf, 1, 1, "A\n");
        assert_eq!(lines_of(&buf), "c\nA\nb");

        // Fewer lines out than in, and more.
        let mut buf = buffer_of("c\na\nb");
        replace_lines(&mut buf, 0, 2, "one\n");
        assert_eq!(lines_of(&buf), "one");
        let mut buf = buffer_of("c\na\nb");
        replace_lines(&mut buf, 2, 2, "x\ny\n");
        assert_eq!(lines_of(&buf), "c\na\nx\ny");

        // `:%!true` -- no output at all empties the buffer.
        let mut buf = buffer_of("c\na\nb");
        replace_lines(&mut buf, 0, 2, "");
        assert_eq!(lines_of(&buf), "");

        // A command that writes no trailing newline still gets its
        // lines, rather than one line with an embedded newline.
        let mut buf = buffer_of("c\na\nb");
        replace_lines(&mut buf, 0, 2, "x\ny");
        assert_eq!(lines_of(&buf), "x\ny");
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

    // Still an error, but no longer at parse time: `:s//bar/` is a real
    // command meaning "that pattern again", so the parser accepts it and
    // `resolve_substitute` is what finds there is no previous one.
    #[test]
    fn an_empty_pattern_with_nothing_before_it_is_an_error() {
        let cmd = parse_substitute_command("s//bar/").unwrap().unwrap();
        assert_eq!(cmd.pattern, "", "the parser passes it along");
        assert_eq!(resolve_substitute(cmd, None, "").unwrap_err(), "E35: no previous regular expression");
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
        let re = crate::regex::Regex::compile("o", false);
        let (text, count) = substitute_line("foo bar", &re, "0", false);
        assert_eq!(text, "f0o bar");
        assert_eq!(count, 1);
    }

    #[test]
    fn substitute_line_every_match_with_g() {
        let re = crate::regex::Regex::compile("o", false);
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
        let re = crate::regex::Regex::compile("x*", false);
        let (text, count) = substitute_line("ab", &re, "-", true);
        assert_eq!(text, "-a-b");
        assert_eq!(count, 2);
    }

    #[test]
    fn substitute_line_zero_width_on_a_single_char_line() {
        let re = crate::regex::Regex::compile("x*", false);
        let (text, count) = substitute_line("a", &re, "-", true);
        assert_eq!(text, "-a");
        assert_eq!(count, 1);
    }

    #[test]
    fn substitute_line_zero_width_on_an_empty_line_still_substitutes_once() {
        let re = crate::regex::Regex::compile("x*", false);
        let (text, count) = substitute_line("", &re, "-", true);
        assert_eq!(text, "-");
        assert_eq!(count, 1);
    }

    #[test]
    fn substitute_line_a_real_match_reaching_the_end_leaves_nothing_further_to_match() {
        let re = crate::regex::Regex::compile("b*", false);
        let (text, count) = substitute_line("ab", &re, "-", true);
        assert_eq!(text, "-a-");
        assert_eq!(count, 2);
        let (text, count) = substitute_line("abb", &re, "-", true);
        assert_eq!(text, "-a-");
        assert_eq!(count, 2);
    }

    #[test]
    fn parse_global_command_reads_all_three_spellings() {
        let ok = |s: &str| parse_global_command(s).unwrap().unwrap();
        let g = ok("g/TODO/d");
        assert_eq!((g.pattern.as_str(), g.invert, g.command.as_str()), ("TODO", false, "d"));
        let g = ok("g!/TODO/d");
        assert!(g.invert, "`g!` is the inverse");
        let g = ok("v/TODO/d");
        assert!(g.invert, "and `v` is the same thing");
        // Default range is the whole file, unlike `:s`'s current line.
        assert_eq!((g.from, g.to), (LineRef::Number(1), LineRef::Last));
        let g = ok("2,4g/x/d");
        assert_eq!((g.from, g.to), (LineRef::Number(2), LineRef::Number(4)));
        // Any non-alphanumeric delimiter, like `:s`.
        assert_eq!(ok("g#a/b#d").pattern, "a/b");

        // Not an attempt at all: falls through to ordinary dispatch.
        assert!(parse_global_command("git diff").is_none());
        assert!(parse_global_command("gi").is_none());
        assert!(parse_global_command("w").is_none());
        // Recognised but malformed.
        assert!(parse_global_command("g/unterminated").unwrap().is_err());
        assert!(parse_global_command("g//d").unwrap().is_err());
        assert!(parse_global_command("g/x/").unwrap().is_err());
    }

    // Every expectation here was read off real `vim -u NONE -i NONE -N`.
    // Before this, everything after the last delimiter was parsed and
    // thrown away, so `:%s/a/Z/gi` did a case-*sensitive* substitution
    // and reported success -- the wrong answer, delivered confidently.
    #[test]
    fn substitute_flags_are_read_rather_than_dropped() {
        let parse = |line: &str| parse_substitute_command(line).expect("recognized as `s`");

        let cmd = parse("%s/a/Z/gi").unwrap();
        assert!(cmd.global);
        assert_eq!(cmd.ignore_case, Some(true));
        // `I` is not the absence of `i`: it overrides 'ignorecase' the
        // other way, which is why this is an Option and not a bool.
        assert_eq!(parse("%s/a/Z/I").unwrap().ignore_case, Some(false));
        assert_eq!(parse("%s/a/Z/g").unwrap().ignore_case, None);

        assert!(parse("%s/a/Z/n").unwrap().count_only);
        assert!(parse("2s/a/Z/e").unwrap().no_error);
        // A count, with or without the space vim allows before it, and
        // with flags in front of it.
        assert_eq!(parse("1,2s/A/Z/ 3").unwrap().count, Some(3));
        assert_eq!(parse("s/A/Z/12").unwrap().count, Some(12));
        assert_eq!(parse("s/A/Z/g 2").unwrap().count, Some(2));

        // Refused, not ignored.
        assert!(parse("%s/a/Z/gq").is_err(), "an unknown flag is an error");
        assert!(parse("%s/a/Z/2x").is_err(), "a letter after the count is trailing junk");
        assert!(parse("%s/a/Z/0").is_err(), "a zero count is a typo, not an empty range");
    }

    // `:s`, `:&`, `:&&`, `:~` -- and the `s` here must not shadow a
    // real one, nor `:set`/`:sort`.
    #[test]
    fn the_repeat_commands_parse_without_shadowing_anything() {
        let kind = |line: &str| parse_repeat_substitute(line).map(|r| r.map(|(from, to, k, f)| (from, to, k, f.global)));
        assert!(matches!(kind("s"), Some(Ok((LineRef::Current, LineRef::Current, RepeatSubstitute::Bare, _)))));
        assert!(matches!(kind("&"), Some(Ok((_, _, RepeatSubstitute::Bare, _)))));
        assert!(matches!(kind("&&"), Some(Ok((_, _, RepeatSubstitute::KeepFlags, _)))));
        assert!(matches!(kind("~"), Some(Ok((_, _, RepeatSubstitute::LastSearch, _)))));
        // Ranges, own flags, own count.
        assert!(matches!(kind("2,3&&"), Some(Ok((LineRef::Number(2), LineRef::Number(3), RepeatSubstitute::KeepFlags, _)))));
        assert!(matches!(kind("%~g"), Some(Ok((LineRef::Number(1), LineRef::Last, RepeatSubstitute::LastSearch, true)))));
        assert!(matches!(kind("s g"), Some(Ok((_, _, RepeatSubstitute::Bare, true)))));

        // Not repeats: a real substitute, and two ordinary commands that
        // merely start with the same letter.
        for line in ["s/a/b/", "s/a/b/g", "set", "sort", "w", "1,5d"] {
            assert!(parse_repeat_substitute(line).is_none(), "{line}");
        }
    }

    // What each of them reaches back for. The distinction the whole `r`
    // flag exists for is here: `:&` takes the last *substitute*'s
    // pattern even when a `/` search has happened since, where a
    // `:s//x/` would have taken the search. Checked against real vim.
    #[test]
    fn a_repeat_takes_its_pattern_deliberately_not_by_recency() {
        let previous = LastSubstitute {
            cmd: parse_substitute_command("%s/a/Z/g").unwrap().unwrap(),
            // The search at the time -- a later `/cCc` is newer than it.
            search_then: String::new(),
        };
        let repeat = |line: &str| {
            let Some(Ok((from, to, kind, flags))) = parse_repeat_substitute(line) else { panic!("{line}") };
            repeat_substitute(Some(&previous), "cCc", from, to, kind, flags).unwrap()
        };

        // `:&` and a bare `:s` take the substitute's pattern, and drop
        // its flags.
        for line in ["&", "s"] {
            let cmd = repeat(line);
            assert_eq!(cmd.pattern, "a", "{line}");
            assert_eq!(cmd.replacement, "Z", "{line}");
            assert!(!cmd.global, "{line}: the flags do not come along");
        }
        // `:&&` keeps them.
        assert!(repeat("&&").global);
        // `:~` and `:&r` are the same command spelled two ways.
        assert_eq!(repeat("~").pattern, "cCc");
        assert_eq!(repeat("&r").pattern, "cCc");
        // Its own flags go on top of whichever base it started from.
        assert!(repeat("&g").global);
        assert_eq!(repeat("&& 2").count, Some(2), "a count is never inherited, only written");

        // Nothing to repeat.
        let Some(Ok((from, to, kind, flags))) = parse_repeat_substitute("&") else { panic!() };
        assert!(repeat_substitute(None, "", from, to, kind, flags).is_err());
    }

    #[test]
    fn the_remaining_flags_parse() {
        let parse = |line: &str| parse_substitute_command(line).expect("recognized as `s`");
        assert!(parse("%s/a/Z/c").unwrap().confirm);
        assert!(parse("%s/a/Z/&").unwrap().keep_flags);
        assert_eq!(parse("%s/a/Z/p").unwrap().print, Some(PrintStyle::Plain));
        assert_eq!(parse("%s/a/Z/#").unwrap().print, Some(PrintStyle::Numbered));
        assert_eq!(parse("%s/a/Z/l").unwrap().print, Some(PrintStyle::List));
        // vim takes `&` only in first position and rejects the command
        // otherwise rather than reading it out of order.
        assert!(parse("%s/a/Z/g&").is_err());
        assert!(parse("%s/a/Z/&g").is_ok());
    }

    // `&` takes the previous command's flags -- and only its flags. A
    // count is not one of them, which is the part I would have guessed
    // wrong: `:1,2s/A/Z/ 3` then `:%s/a/Y/&` acts on the whole buffer.
    #[test]
    fn ampersand_takes_the_previous_flags_but_not_its_count() {
        let previous = parse_substitute_command("1,2s/A/Z/gi 3").unwrap().unwrap();
        let cmd = parse_substitute_command("%s/a/Y/&").unwrap().unwrap();
        let last = LastSubstitute { cmd: previous, search_then: String::new() };
        let resolved = resolve_substitute(cmd, Some(&last), "").unwrap();
        assert!(resolved.global);
        assert_eq!(resolved.ignore_case, Some(true));
        assert_eq!(resolved.count, None, "the count stays with the command that wrote it");
        // Nothing before it is not an error -- there are simply no flags
        // to take.
        let cmd = parse_substitute_command("%s/a/Y/&").unwrap().unwrap();
        assert!(resolve_substitute(cmd, None, "").is_ok());
    }

    // `:s//repl/` means "that pattern again", and which one that is
    // depends on whether a `/` search has happened since. All four
    // orderings were checked against real vim.
    #[test]
    fn an_empty_pattern_takes_whichever_was_used_more_recently() {
        let ran = |pattern: &str, search_then: &str| LastSubstitute {
            cmd: parse_substitute_command(&format!("%s/{pattern}/Z/")).unwrap().unwrap(),
            search_then: search_then.to_string(),
        };
        let empty = || parse_substitute_command("%s//W/").unwrap().unwrap();

        // A search since the `:s` is the newer of the two.
        let last = ran("a", "");
        assert_eq!(resolve_substitute(empty(), Some(&last), "cCc").unwrap().pattern, "cCc");
        // No search since, so the substitute's own pattern stands.
        let last = ran("a", "cCc");
        assert_eq!(resolve_substitute(empty(), Some(&last), "cCc").unwrap().pattern, "a");
        // `r` changes neither answer -- see resolve_substitute for the
        // vim runs that show why.
        let with_r = parse_substitute_command("%s//W/r").unwrap().unwrap();
        assert_eq!(resolve_substitute(with_r, Some(&last), "cCc").unwrap().pattern, "a");
        // Nothing at all to fall back on.
        assert!(resolve_substitute(empty(), None, "").is_err());
        // A search, but no substitute yet.
        assert_eq!(resolve_substitute(empty(), None, "cCc").unwrap().pattern, "cCc");
    }

    #[test]
    fn p_and_hash_and_l_spell_the_line_the_way_vim_does() {
        let buf = buf_from("aAa\nb\tb\naAa");
        assert_eq!(printed_line(&buf, 2, PrintStyle::Plain), "aAa");
        // Right-aligned in three columns, then one space -- vim's own
        // layout.
        assert_eq!(printed_line(&buf, 2, PrintStyle::Numbered), "  3 aAa");
        // `:list` style: the end of the line is marked, so trailing
        // space shows, and a tab is spelled rather than jumped.
        assert_eq!(printed_line(&buf, 1, PrintStyle::List), "b^Ib$");
    }

    #[test]
    fn i_and_capital_i_beat_the_buffers_own_ignorecase() {
        let run = |flags: &str| {
            let mut buf = buf_from("aAa\nbBb\naAa");
            let cmd = parse_substitute_command(&format!("%s/a/Z/{flags}")).unwrap().unwrap();
            let counts = run_substitute(&mut buf, &cmd);
            (text_of(&buf), counts)
        };
        assert_eq!(run("gi").0, "ZZZ\nbBb\nZZZ");
        assert_eq!(run("gI").0, "ZAZ\nbBb\nZAZ");
        assert_eq!(run("g").0, "ZAZ\nbBb\nZAZ", "neither flag leaves the buffer's own setting in charge");
    }

    // vim's count *replaces* the range rather than narrowing it, and
    // counts from the range's own last line -- so it can reach lines the
    // range never named. `:1,2s/A/Z/ 3` acts on lines 2, 3 and 4.
    #[test]
    fn a_trailing_count_replaces_the_range_from_its_last_line() {
        let mut buf = buf_from("aAa\nbBb\naAa\ncCc\naAa");
        let cmd = parse_substitute_command("1,2s/A/Z/ 3").unwrap().unwrap();
        run_substitute(&mut buf, &cmd).unwrap();
        assert_eq!(text_of(&buf), "aAa\nbBb\naZa\ncCc\naAa", "lines 2..4, not lines 1..2");
    }

    #[test]
    fn n_counts_without_touching_the_buffer_and_e_forgives_a_miss() {
        let mut buf = buf_from("aAa\nbBb\naAa");
        let cmd = parse_substitute_command("%s/a/Z/gn").unwrap().unwrap();
        let out = run_substitute(&mut buf, &cmd).unwrap();
        assert_eq!((out.substitutions, out.lines), (4, 2));
        assert_eq!(text_of(&buf), "aAa\nbBb\naAa", "`n` answers the question and changes nothing");

        // A pattern that matches nothing is an error...
        let mut buf = buf_from("aAa");
        let cmd = parse_substitute_command("%s/zzz/Z/").unwrap().unwrap();
        assert!(run_substitute(&mut buf, &cmd).is_err());
        // ...unless `e` says it is fine.
        let cmd = parse_substitute_command("%s/zzz/Z/e").unwrap().unwrap();
        let out = run_substitute(&mut buf, &cmd).unwrap();
        assert_eq!((out.substitutions, out.lines), (0, 0));
    }

    #[test]
    fn run_substitute_current_line_only_by_default() {
        let mut buf = buf_from("foo\nfoo\nfoo");
        buf.set_cursor(1, 0);
        let cmd = SubstituteCmd {
            from: LineRef::Current,
            to: LineRef::Current,
            pattern: "foo".to_string(),
            replacement: "bar".to_string(),
            global: false,
            ignore_case: None,
            count: None,
            count_only: false,
            no_error: false,
            keep_flags: false,
            print: None,
            confirm: false,
        };
        let out = run_substitute(&mut buf, &cmd).unwrap();
        let (subs, lines) = (out.substitutions, out.lines);
        assert_eq!((subs, lines), (1, 1));
        assert_eq!(text_of(&buf), "foo\nbar\nfoo");
    }

    #[test]
    fn run_substitute_whole_buffer_with_global_flag() {
        let mut buf = buf_from("foo foo\nfoo\nbaz");
        let cmd = SubstituteCmd {
            from: LineRef::Number(1),
            to: LineRef::Last,
            pattern: "foo".to_string(),
            replacement: "X".to_string(),
            global: true,
            ignore_case: None,
            count: None,
            count_only: false,
            no_error: false,
            keep_flags: false,
            print: None,
            confirm: false,
        };
        let out = run_substitute(&mut buf, &cmd).unwrap();
        let (subs, lines) = (out.substitutions, out.lines);
        assert_eq!((subs, lines), (3, 2));
        assert_eq!(text_of(&buf), "X X\nX\nbaz");
    }

    #[test]
    fn run_substitute_backreference_and_capture() {
        let mut buf = buf_from("hello world");
        let cmd = SubstituteCmd {
            from: LineRef::Current,
            to: LineRef::Current,
            pattern: "(hello) (world)".to_string(),
            replacement: r"\2 \1".to_string(),
            global: false,
            ignore_case: None,
            count: None,
            count_only: false,
            no_error: false,
            keep_flags: false,
            print: None,
            confirm: false,
        };
        run_substitute(&mut buf, &cmd).unwrap();
        assert_eq!(text_of(&buf), "world hello");
    }

    #[test]
    fn run_substitute_r_splits_a_line_and_still_advances_the_range_correctly() {
        // Range is the whole 2-line buffer; line 0's own substitution
        // splits it into two lines via \r -- line 1 (originally "b")
        // must still be reached afterward, now at row 2.
        let mut buf = buf_from("a,a\nb");
        let cmd = SubstituteCmd {
            from: LineRef::Number(1),
            to: LineRef::Last,
            pattern: ",".to_string(),
            replacement: r"\r".to_string(),
            global: false,
            ignore_case: None,
            count: None,
            count_only: false,
            no_error: false,
            keep_flags: false,
            print: None,
            confirm: false,
        };
        let out = run_substitute(&mut buf, &cmd).unwrap();
        let (subs, lines) = (out.substitutions, out.lines);
        assert_eq!(subs, 1);
        assert_eq!(lines, 1);
        assert_eq!(text_of(&buf), "a\na\nb");
    }

    #[test]
    fn run_substitute_reports_pattern_not_found() {
        let mut buf = buf_from("foo");
        let cmd = SubstituteCmd {
            from: LineRef::Current,
            to: LineRef::Current,
            pattern: "zzz".to_string(),
            replacement: "x".to_string(),
            global: false,
            ignore_case: None,
            count: None,
            count_only: false,
            no_error: false,
            keep_flags: false,
            print: None,
            confirm: false,
        };
        assert_eq!(run_substitute(&mut buf, &cmd).unwrap_err(), "E486: Pattern not found: zzz");
    }

    #[test]
    fn end_to_end_percent_s_g_via_run_command_mode_parsing_path() {
        // Confirms the two pieces (parse + run) compose correctly, not
        // just each in isolation.
        let mut buf = buf_from("cat cat\ndog");
        let parsed = parse_substitute_command("%s/cat/dog/g").unwrap().unwrap();
        let out = run_substitute(&mut buf, &parsed).unwrap();
        let (subs, lines) = (out.substitutions, out.lines);
        assert_eq!((subs, lines), (2, 1));
        assert_eq!(text_of(&buf), "dog dog\ndog");
    }

    // The rule itself. That it is consulted only on a fresh command's
    // first line is the two call sites' own job -- both share the gate
    // history expansion already uses.
    #[test]
    fn a_leading_space_or_tab_keeps_a_command_off_the_record() {
        assert!(starts_off_the_record(" echo secret"));
        assert!(starts_off_the_record("\techo secret"));
        assert!(starts_off_the_record("  !make"), "spacebang, the case command mode cares about");
        assert!(!starts_off_the_record("echo remembered"));
        assert!(!starts_off_the_record("!make"));
        assert!(!starts_off_the_record(""));
        // Trailing and interior whitespace say nothing about it.
        assert!(!starts_off_the_record("echo a  b "));
    }
}

#[cfg(test)]
mod capture_text_tests {
    use super::*;

    // A view that paints the terminal directly -- the editor, the pager,
    // a review -- is read back out of the frame it painted: only the
    // pane's rectangle, positioned the way the frame positions itself,
    // with nothing from outside it leaking in.
    #[test]
    fn a_painted_frame_is_read_back_inside_its_pane() {
        let rect = Rect { row: 1, col: 4, rows: 2, cols: 6 };
        let frame = "\x1b[1;1Hstatus-outside\x1b[2;5Hhello!\x1b[3;5H\x1b[1mbold\x1b[0m\x1b[4;1Hbelow";
        assert_eq!(painted_rows(rect, frame, 5, 20), vec!["hello!".to_string(), "bold  ".to_string()]);
    }

    fn screen_showing(lines: &[&str], rows: usize, cols: usize) -> vt100::Screen {
        let mut screen = vt100::Screen::new(rows, cols);
        for line in lines {
            screen.feed(line.as_bytes());
            screen.feed(b"\r\n");
        }
        screen
    }

    // focused_pane_text needs a whole App to find the focused pane; its
    // interesting half is turning one grid into text, which this is.
    fn text_of(screen: &vt100::Screen) -> String {
        let (rows, _cols) = screen.size();
        let mut lines: Vec<String> = (0..rows)
            .map(|row| {
                let mut line: String = screen.row_text(row);
                while line.ends_with(' ') {
                    line.pop();
                }
                line
            })
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.push(String::new());
        lines.join("\n")
    }

    #[test]
    fn a_captured_screen_is_its_rows_without_the_padding() {
        // A grid is a rectangle, so every row is `cols` wide whatever is
        // on it. A script grepping the capture wants the words, not the
        // spaces after them.
        let screen = screen_showing(&["$ echo hi", "hi"], 10, 40);
        assert_eq!(text_of(&screen), "$ echo hi\nhi\n");
    }

    #[test]
    fn the_blank_bottom_of_the_screen_is_not_part_of_the_capture() {
        // Eight empty rows under two lines of output is what the grid
        // holds; it is not what the pane shows.
        let screen = screen_showing(&["one", "two"], 10, 20);
        assert_eq!(text_of(&screen).lines().count(), 2);
    }

    #[test]
    fn a_blank_line_between_two_lines_of_output_survives() {
        // Only the run at the *bottom* is padding. One in the middle is
        // something the command printed.
        let screen = screen_showing(&["first", "", "third"], 8, 20);
        assert_eq!(text_of(&screen), "first\n\nthird\n");
    }

    #[test]
    fn an_empty_screen_captures_as_nothing_at_all() {
        // Not even the trailing newline every other capture ends with:
        // once every row has been trimmed away there is no last line
        // for one to go after, and `capture` on a blank pane printing a
        // stray blank line would be worse than printing nothing.
        let screen = vt100::Screen::new(6, 20);
        assert_eq!(text_of(&screen), "");
    }
}

#[cfg(test)]
mod editorconfig_charset_tests {
    use super::*;

    // The property that used to be parsed and then thrown away. A
    // project that says its files are latin-1 gets latin-1 written
    // back, and a name this shell cannot write leaves the file's own
    // encoding in place rather than quietly converting it.
    #[test]
    fn charset_decides_what_a_buffer_is_written_as() {
        let dir = std::env::temp_dir().join(format!("bish-editorconfig-charset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shell = exec::Shell::new();
        let path = dir.join("f.txt");
        std::fs::write(&path, "text\n").unwrap();

        let check = |charset: &str| {
            std::fs::write(dir.join(".editorconfig"), format!("root = true\n[*]\ncharset = {charset}\n")).unwrap();
            let mut buf = TextBuffer::open(&path, 10).unwrap();
            apply_editorconfig(&shell, &mut buf);
            (buf.encoding, buf.bom)
        };

        assert_eq!(check("latin1"), (crate::encoding::Encoding::Latin1, false));
        assert_eq!(check("utf-8"), (crate::encoding::Encoding::Utf8, false));
        assert_eq!(check("utf-8-bom"), (crate::encoding::Encoding::Utf8, true), "the mark is the whole difference between these two");
        assert_eq!(check("utf-16le"), (crate::encoding::Encoding::Utf16Le, true), "a UTF-16 file nothing can identify is not worth writing");
        assert_eq!(check("shift_jis"), (crate::encoding::Encoding::Utf8, false), "left as the file's own, which is what it was read as");

        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod watched_file_tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bish-watched-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The kernel delivers on its own schedule, so this waits for the
    /// answer rather than reading once and calling the machine's mood a
    /// result.
    fn settle(watcher: &mut crate::watch::Watcher, buf: &mut TextBuffer) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if sync_watched_file(Some(watcher), buf) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn a_file_rewritten_underneath_says_so_once() {
        let dir = scratch("changed");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut buf = TextBuffer::open(&path, 10).unwrap();
        let mut watcher = crate::watch::Watcher::new().unwrap();
        // The first call takes the watch out and reports nothing --
        // there is nothing to report yet.
        assert!(!sync_watched_file(Some(&mut watcher), &mut buf));

        std::fs::write(&path, "two\n").unwrap();
        assert!(settle(&mut watcher, &mut buf), "the change was never noticed");
        assert!(buf.echoed_message.as_deref().is_some_and(|m| m.contains("W11")), "{:?}", buf.echoed_message);
        // `changed_on_disk` keeps answering yes until the buffer reads
        // or writes the file again, so the message must not be re-echoed
        // on every tick from here on.
        assert!(!sync_watched_file(Some(&mut watcher), &mut buf), "said twice");
        std::fs::remove_dir_all(&dir).ok();
    }

    // The reason inotify says *when* to look rather than what happened:
    // a formatter that changed nothing, a `touch`, this editor's own
    // save. Warning about any of those is crying wolf.
    #[test]
    fn a_write_that_changed_nothing_is_not_a_change() {
        let dir = scratch("same");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut buf = TextBuffer::open(&path, 10).unwrap();
        let mut watcher = crate::watch::Watcher::new().unwrap();
        assert!(!sync_watched_file(Some(&mut watcher), &mut buf));

        std::fs::write(&path, "one\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!sync_watched_file(Some(&mut watcher), &mut buf), "the content is the same");
        assert_eq!(buf.echoed_message, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    // The case an inode watch cannot see, and the reason watch.rs
    // watches directories: a formatter or a branch switch replaces the
    // file rather than writing through it.
    #[test]
    fn a_file_replaced_by_rename_is_noticed_too() {
        let dir = scratch("renamed");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut buf = TextBuffer::open(&path, 10).unwrap();
        let mut watcher = crate::watch::Watcher::new().unwrap();
        assert!(!sync_watched_file(Some(&mut watcher), &mut buf));

        let temp = dir.join("f.txt.tmp");
        std::fs::write(&temp, "two\n").unwrap();
        std::fs::rename(&temp, &path).unwrap();
        assert!(settle(&mut watcher, &mut buf), "a rename over the file went unnoticed");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_buffer_with_no_file_is_nothing_to_watch() {
        let mut buf = TextBuffer::new_unnamed(10);
        let mut watcher = crate::watch::Watcher::new().unwrap();
        assert!(!sync_watched_file(Some(&mut watcher), &mut buf));
        assert!(!sync_watched_file(None, &mut buf), "and no watcher at all is not an error either");
    }
}

#[cfg(test)]
mod gutter_hover_tests {
    use super::*;

    fn buf() -> TextBuffer {
        let mut tb = TextBuffer::new_unnamed(10);
        tb.insert_text((0, 0), "alpha\nbravo\ncharlie");
        tb
    }

    // The marker in the gutter says only "something is wrong here". The
    // hover is where it says what -- there is no room for a message in
    // one coloured bullet.
    #[test]
    fn the_diagnostic_column_says_what_the_bullet_stands_for() {
        let mut tb = buf();
        tb.diagnostics = vec![crate::bishedit::lint::Diagnostic {
            start: 6,
            end: 11,
            severity: crate::bishedit::lint::Severity::Warning,
            code: std::borrow::Cow::Borrowed("unquoted-expansion"),
            source: None,
            message: "wrap it in double quotes".to_string(),
            fix: None,
        }];
        assert_eq!(gutter_hover_lines(&tb, 1, fileeditor::GutterKind::Diagnostic), vec!["unquoted-expansion: wrap it in double quotes".to_string()]);
        // A line with no diagnostic on it has nothing to say, and a
        // popup that appeared there would be describing the line above.
        assert!(gutter_hover_lines(&tb, 0, fileeditor::GutterKind::Diagnostic).is_empty());
    }

    #[test]
    fn the_blame_column_says_the_whole_of_what_it_abbreviates() {
        let mut tb = buf();
        tb.blame = Some(vec![
            Some(crate::git::BlameLine { short_commit: "abc1234".into(), author: "Ada Lovelace".into(), date: "2026-09-07".into() }),
            None,
            None,
        ]);
        assert_eq!(gutter_hover_lines(&tb, 0, fileeditor::GutterKind::Blame), vec!["abc1234 Ada Lovelace".to_string(), "2026-09-07".to_string()]);
        assert!(gutter_hover_lines(&tb, 1, fileeditor::GutterKind::Blame).is_empty(), "a line git had nothing to say about");
    }

    #[test]
    fn the_diff_column_says_which_kind_of_change() {
        let mut tb = buf();
        tb.diff = Some(std::collections::HashMap::from([(1, crate::git::DiffMark::Added), (2, crate::git::DiffMark::Removed)]));
        assert!(gutter_hover_lines(&tb, 1, fileeditor::GutterKind::Diff)[0].contains("added"));
        assert!(gutter_hover_lines(&tb, 2, fileeditor::GutterKind::Diff)[0].contains("removed"));
        assert!(gutter_hover_lines(&tb, 0, fileeditor::GutterKind::Diff).is_empty(), "an unchanged line has no marker to explain");
    }

    // Blame and diff do nothing on purpose, and this is where that is
    // held. Both have an obvious action waiting for them, and shipping
    // an incidental "puts the cursor on the line" in the meantime is
    // how a placeholder becomes something people rely on -- after
    // which giving the click its real meaning takes something away.
    #[test]
    fn the_undecided_columns_are_reserved_rather_than_given_a_default() {
        assert_eq!(gutter_action(fileeditor::GutterKind::Blame, 3), GutterAction::Nothing);
        assert_eq!(gutter_action(fileeditor::GutterKind::Diff, 3), GutterAction::Nothing);
        assert_eq!(gutter_action(fileeditor::GutterKind::Diagnostic, 3), GutterAction::Nothing);
    }

    #[test]
    fn the_decided_columns_do_what_their_marker_says() {
        // 1-based on the way out, because that is how breakpoints are
        // numbered everywhere else.
        assert_eq!(gutter_action(fileeditor::GutterKind::Breakpoint, 3), GutterAction::ToggleBreakpoint(4));
        assert_eq!(gutter_action(fileeditor::GutterKind::LineNumber, 3), GutterAction::GoToLine(3));
    }

    // These two are controls rather than descriptions: the breakpoint
    // marker already says the whole of what a breakpoint is, and a line
    // number says its own number. A popup on either would be noise.
    #[test]
    fn the_columns_that_act_do_not_also_narrate() {
        let mut tb = buf();
        tb.breakpoints.insert(2);
        assert!(gutter_hover_lines(&tb, 1, fileeditor::GutterKind::Breakpoint).is_empty());
        assert!(gutter_hover_lines(&tb, 1, fileeditor::GutterKind::LineNumber).is_empty());
    }
}

#[cfg(test)]
mod debug_adapter_tests {
    use super::*;

    /// Same skip-rather-than-fail contract the dapclient tests have:
    /// this needs a C compiler and a gdb new enough to speak DAP.
    fn fixture(name: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
        std::process::Command::new("gdb")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?
            .success()
            .then_some(())?;
        let dir = std::env::temp_dir().join(format!("bish-dbg-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let source = dir.join("prog.c");
        std::fs::write(
            &source,
            "#include <stdio.h>\nint add(int a, int b) { int s = a + b; return s; }\nint main(void) {\n    int total = 0;\n    for (int i = 1; i <= 3; i++) { total = add(total, i); }\n    printf(\"%d\\n\", total);\n    return 0;\n}\n",
        )
        .ok()?;
        let program = dir.join("prog");
        std::process::Command::new("cc")
            .args(["-g", "-O0", "-o"])
            .arg(&program)
            .arg(&source)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?
            .success()
            .then_some((dir, source, program))
    }

    fn launched(source: &Path, program: &Path, breakpoints: &[usize]) -> Option<(DapFrame, TextBuffer)> {
        let mut buf = TextBuffer::open(source, 20).ok()?;
        for line in breakpoints {
            buf.breakpoints.insert(*line);
        }
        let dir = source.parent().expect("a directory");
        let launch = crate::json::Value::Object(vec![
            ("program".to_string(), crate::json::Value::Str(program.to_string_lossy().into_owned())),
            ("cwd".to_string(), crate::json::Value::Str(dir.to_string_lossy().into_owned())),
        ]);
        let mut session = crate::dapclient::Session::start(&["gdb".to_string(), "-i=dap".to_string()], dir, launch).expect("gdb starts");
        session.set_breakpoints(source, breakpoints);
        Some((
            DapFrame {
                session,
                program: program.file_name()?.to_string_lossy().into_owned(),
                thread: None,
                frames: Vec::new(),
                awaiting_stack: None,
                awaiting_answer: None,
                landing: false,
            },
            buf,
        ))
    }

    /// Ticks the way the editor's own idle loop does, until `done`.
    fn until(
        frame: &mut DapFrame,
        buf: &mut TextBuffer,
        screen: Option<&Rc<RefCell<vt100::Screen>>>,
        seconds: u64,
        done: impl Fn(&DapFrame, &TextBuffer) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        while std::time::Instant::now() < deadline {
            advance_debug_adapter(frame, buf, screen);
            if done(frame, buf) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    /// The whole point of the feature: a program stops, and the cursor
    /// is sitting on the line it stopped at.
    #[test]
    fn a_stop_lands_the_cursor_on_the_line_it_stopped_at() {
        let Some((dir, source, program)) = fixture("land") else { return };
        let (mut frame, mut buf) = launched(&source, &program, &[2]).expect("a session");
        assert_eq!(buf.cursor().0, 0, "the cursor starts at the top");

        let landed = until(&mut frame, &mut buf, None, 20, |f, _| !f.frames.is_empty());
        let cursor = buf.cursor().0;
        let notice = buf.echoed_message.clone();
        let names: Vec<String> = frame.frames.iter().map(|f| f.name.clone()).collect();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(landed, "never stopped: {:?}", frame.session.log());
        // Line 2 of the file is `int add(...)`, and the cursor counts
        // from zero.
        assert_eq!(cursor, 1, "the cursor did not move to the breakpoint");
        assert_eq!(names.first().map(String::as_str), Some("add"), "{names:?}");
        let notice = notice.expect("the status line says where it is");
        assert!(notice.contains("breakpoint"), "{notice}");
        assert!(notice.contains("add"), "{notice}");
        assert!(notice.contains("prog.c:2"), "{notice}");
    }

    // A breakpoint set before there is anything to set it on: the marks
    // go in the gutter first, and the session sends them when the
    // adapter says it is ready. This is why `:dbg break` no longer
    // insists on being attached first.
    #[test]
    fn breakpoints_set_before_the_program_ran_are_the_ones_that_stop_it() {
        let Some((dir, source, program)) = fixture("early") else { return };
        // Line 4 is `int total = 0;`, the first statement of `main`.
        let (mut frame, mut buf) = launched(&source, &program, &[4]).expect("a session");
        let landed = until(&mut frame, &mut buf, None, 20, |f, _| !f.frames.is_empty());
        let (cursor, top) = (buf.cursor().0, frame.frames.first().map(|f| f.name.clone()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(landed, "never stopped: {:?}", frame.session.log());
        assert_eq!(cursor, 3);
        assert_eq!(top.as_deref(), Some("main"));
    }

    #[test]
    fn an_evaluated_expression_comes_back_to_the_status_line() {
        let Some((dir, source, program)) = fixture("evaluate") else { return };
        let (mut frame, mut buf) = launched(&source, &program, &[2]).expect("a session");
        assert!(until(&mut frame, &mut buf, None, 20, |f, _| !f.frames.is_empty()), "{:?}", frame.session.log());

        // What `:dbg print a + b` does: send it, then let the idle tick
        // pick the answer up.
        let at = frame.current_frame();
        let seq = frame.session.evaluate("a + b", at, "watch");
        frame.awaiting_answer = Some((seq, "a + b".to_string()));
        let answered = until(&mut frame, &mut buf, None, 10, |f, _| f.awaiting_answer.is_none());
        let shown = buf.echoed_message.clone();

        // And a refusal reaches the user as the debugger's own words.
        let seq = frame.session.evaluate("no_such_symbol", at, "watch");
        frame.awaiting_answer = Some((seq, "no_such_symbol".to_string()));
        let refused = until(&mut frame, &mut buf, None, 10, |f, _| f.awaiting_answer.is_none());
        let refusal = buf.echoed_message.clone();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(answered);
        assert_eq!(shown.as_deref(), Some("a + b = 1"), "add(0, 1) on the first call");
        assert!(refused);
        assert!(refusal.is_some_and(|r| r.to_lowercase().contains("no symbol")), "the debugger's own words");
    }

    // Program output belongs in the debug-run pane, and a bare newline
    // has to become a carriage return and one -- a terminal grid moves
    // down but not back, so without that the second line starts where
    // the first ended.
    #[test]
    fn the_programs_own_output_reaches_the_run_pane() {
        let Some((dir, source, program)) = fixture("output") else { return };
        let (mut frame, mut buf) = launched(&source, &program, &[]).expect("a session");
        let screen = Rc::new(RefCell::new(vt100::Screen::new(10, 40)));
        let ran = until(&mut frame, &mut buf, Some(&screen), 20, |_, b| {
            b.echoed_message.as_deref().is_some_and(|m| m.contains("exited") || m.contains("ended"))
        });
        let painted: String = {
            let screen = screen.borrow();
            (0..screen.size().0).map(|row| screen.row_text(row)).collect()
        };
        let notice = buf.echoed_message.clone();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(ran, "the program never finished: {:?}", frame.session.log());
        assert!(painted.contains('6'), "0+1+2+3, printed by the program: {painted:?}");
        // The message names the program, not just "the program".
        assert!(notice.is_some_and(|n| n.contains("prog")), "the exit message names what exited");
    }

    // A stop in a file that is not the one on screen moves nothing.
    // Jumping to a line number belonging to another file would be worse
    // than staying put.
    #[test]
    fn a_stop_somewhere_else_leaves_the_cursor_alone() {
        let Some((dir, source, program)) = fixture("elsewhere") else { return };
        let (mut frame, mut _unused) = launched(&source, &program, &[2]).expect("a session");
        // The buffer is pointed at a different file than the one the
        // adapter will report, which is the situation a breakpoint
        // inside a library produces.
        let other = dir.join("other.c");
        std::fs::write(&other, "int main(void) { return 0; }\n").unwrap();
        let mut elsewhere = TextBuffer::open(&other, 20).unwrap();
        elsewhere.set_cursor(0, 0);

        let landed = until(&mut frame, &mut elsewhere, None, 20, |f, _| !f.frames.is_empty());
        let cursor = elsewhere.cursor().0;
        let notice = elsewhere.echoed_message.clone();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(landed, "{:?}", frame.session.log());
        assert_eq!(cursor, 0, "the cursor stayed where it was");
        // It still says what happened, which is the part that is useful
        // wherever the stop was.
        assert!(notice.is_some_and(|n| n.contains("add")), "the stop is still reported");
    }

    #[test]
    fn launch_arguments_are_read_the_way_they_are_typed() {
        assert_eq!(parse_launch_arguments("./prog"), Some((None, "./prog".to_string())));
        assert_eq!(parse_launch_arguments("--adapter=lldb-dap ./prog"), Some((Some(vec!["lldb-dap".to_string()]), "./prog".to_string())));
        assert_eq!(
            parse_launch_arguments("--adapter=gdb -i=dap ./prog").map(|(a, _)| a),
            Some(Some(vec!["gdb".to_string(), "-i=dap".to_string()])),
            "an adapter is a command line, not a word"
        );
        // A program is the one thing with no default.
        assert_eq!(parse_launch_arguments("--adapter=gdb"), None);
        assert_eq!(parse_launch_arguments(""), None);
    }

    #[test]
    fn a_language_with_no_known_adapter_says_so_rather_than_guessing() {
        assert_eq!(default_debug_adapter("c"), Some(vec!["gdb".to_string(), "-i=dap".to_string()]));
        assert_eq!(default_debug_adapter("rust"), Some(vec!["gdb".to_string(), "-i=dap".to_string()]));
        assert_eq!(default_debug_adapter("python"), None);
        // Not an oversight -- see the function's own doc comment: gdb
        // loads a Go binary and then misdescribes it, and delve's
        // invocation is not guessed at.
        assert_eq!(default_debug_adapter("go"), None);
        assert_eq!(default_debug_adapter("bash"), None, "bish debugs its own scripts, and not through an adapter");
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    /// The bug this exists for: a theme declared and activated in the
    /// config, and `::bish hl` listing nothing at all.
    ///
    /// It was never the theme machinery -- that always worked from a
    /// shell that had read the file. It was that a detachable session
    /// never read the file, because loading it was the *caller's* job
    /// and the session daemon was a caller nobody had told. So the test
    /// is of the loading, and `repl::run` is now the only place that
    /// does it.
    #[test]
    fn a_theme_declared_in_the_config_is_in_force_after_loading_it() {
        let mut shell = exec::Shell::new();
        assert!(shell.hl_colors().is_empty(), "nothing is themed before the config is read");

        load_config_source(
            &mut shell,
            "::bish theme begin\n             bishopt --set theme paper\n             ::bish hl --set keyword \"#DDAE36, -bish-yellow\"\n             ::bish hl --set string \"#78C953, -bish-green\"\n             ::bish theme end\n             bishopt --set theme paper\n",
            "config.bash",
        );

        let colours: HashMap<String, String> = shell.hl_colors().into_iter().collect();
        assert_eq!(colours.get("keyword").map(String::as_str), Some("#DDAE36, -bish-yellow"));
        assert_eq!(colours.get("string").map(String::as_str), Some("#78C953, -bish-green"));
    }

    // A config that is never read leaves exactly the state the report
    // described: a theme registered nowhere and an empty listing. Kept
    // as the other half of the pair, so "it loaded" and "it did not"
    // are both pinned rather than only the happy one.
    #[test]
    fn without_the_config_there_is_nothing_to_list() {
        let shell = exec::Shell::new();
        assert!(shell.hl_colors().is_empty());
        assert_eq!(shell.hl_color("keyword"), None);
    }

    /// Declaring a theme is not activating it -- the block only
    /// registers, and the `bishopt --set theme` *after* it is what puts
    /// it in force. Worth pinning because the config that prompted this
    /// has the same line inside the block and after it, and only the
    /// second one counts.
    #[test]
    fn a_theme_that_is_declared_but_not_activated_is_not_in_force() {
        let mut shell = exec::Shell::new();
        load_config_source(
            &mut shell,
            "::bish theme begin\n             bishopt --set theme paper\n             ::bish hl --set keyword \"#DDAE36, -bish-yellow\"\n             ::bish theme end\n",
            "config.bash",
        );
        assert!(shell.hl_colors().is_empty(), "registered, not activated");
    }
}
