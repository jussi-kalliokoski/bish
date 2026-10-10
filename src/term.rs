// The terminal, as bish needs it to behave: raw mode for anything that
// draws its own line, echo off for `read -s`, and the handful of signals
// an interactive shell has an opinion about.
//
// What the OS provides is in `platform`: the terminal's own mode, and
// deriving raw or cooked or echoless from it. Which bits those are, and
// what a `termios` even looks like, differ between the OSes bish runs on
// and are not this file's business. What is here is when to ask for
// which, and the escape sequences that have nothing to do with the
// kernel at all.

use crate::platform::{self, TerminalMode};
use std::io::{self, Write};

// The signal numbers, from the platform layer rather than written out
// here: BSD renumbered the job-control ones, so `SIGTSTP` is not the
// same number on macOS as on Linux. Re-exported under this module's own
// name because this is where the rest of bish has always asked.
pub const SIGINT: i32 = platform::SIGINT;
// Not yet called from anywhere -- session.rs's own daemonize (a
// follow-up commit, once there's an actual accept loop to daemonize
// into) is the first real caller. Same "land the seam, wire it in
// later" pattern pty.rs's own module doc comment already names.
#[allow(dead_code)]
pub const SIGHUP: i32 = platform::SIGHUP;
pub const SIGTSTP: i32 = platform::SIGTSTP;
pub const SIGTTIN: i32 = platform::SIGTTIN;
pub const SIGTTOU: i32 = platform::SIGTTOU;

// Makes the shell itself immune to SIGINT for the rest of the process's
// life, the same trick real interactive shells use instead of process-
// group/job-control plumbing. This disposition is inherited by every
// forked child *and* survives exec() -- POSIX only resets signals with a
// real handler function back to SIG_DFL across exec; SIG_IGN is
// explicitly preserved unchanged. So every spawn site that forks a real
// foreground/background child (apply_fd_redirects, the `command` builtin,
// pty::spawn_attached) must explicitly reset SIGINT to SIG_DFL in its own
// pre_exec hook, or that child would silently inherit "ignore SIGINT" and
// never respond to Ctrl-C. Call once, at interactive startup.
pub fn ignore_sigint() {
    platform::ignore_signal(SIGINT);
}

// A detached `bish session` daemon must survive the terminal that
// launched it going away -- closing a real terminal, or the ssh
// connection that started `bish session new` dropping, sends SIGHUP to
// whatever's still in its foreground process group. `session::
// daemonize` already leaves that process group behind via `setsid()`
// before this matters in practice, but installing this too is cheap,
// standard defense-in-depth (the same belt-and-suspenders `nohup`
// itself relies on) against any window where the daemon is still
// reachable by a HUP before its own setsid() has taken effect. Same
// inherited-across-fork/exec caveat as ignore_sigint above.
#[allow(dead_code)]
pub fn ignore_sighup() {
    platform::ignore_signal(SIGHUP);
}

// Real job control (M11): every job-control shell ignores SIGTTIN/SIGTTOU
// for itself. Textbook reason: once the shell hands the terminal's
// foreground status to a job (tcsetpgrp), the shell's own process group
// becomes a *background* one relative to that terminal for as long as
// the job holds it -- and a background process group that isn't
// ignoring/blocking SIGTTIN/SIGTTOU gets stopped by the kernel the
// moment it touches the terminal (SIGTTIN on any read, SIGTTOU on a
// write if the terminal has TOSTOP set), including, in practice, right
// around the shell's own tcsetpgrp call to reclaim the terminal once the
// job finishes or stops -- exactly the call that's supposed to be how it
// gets control back. Confirmed by reproducing this exact failure mode
// (bish silently dying instead of reclaiming the terminal) without this,
// then confirming it disappears with it -- deliberately NOT touching
// SIGTSTP here, unlike those two: term::suspend_self's own deliberate
// `raise(SIGTSTP)` (Ctrl-Z at a plain prompt, not a job) needs SIGTSTP's
// default disposition to still apply, or self-suspending the shell would
// silently stop working. Same inherits-across-exec caveat as
// ignore_sigint applies to these too -- see exec.rs's pre_exec hooks for
// job-controlled children, which reset them back to SIG_DFL.
pub fn ignore_tty_signals() {
    platform::ignore_signal(SIGTTIN);
    platform::ignore_signal(SIGTTOU);
}

// Suspends the *shell itself* (not a child job) via SIGTSTP, exactly like
// any other well-behaved interactive program stopped from its own
// controlling terminal. This is deliberately not job control: there's no
// process-group/tcsetpgrp reassignment here, just the same self-suspend
// every foreground program gets for free when it doesn't otherwise handle
// SIGTSTP. Returns once something (`fg` in the invoking shell, `kill
// -CONT`, ...) resumes this process.
pub fn suspend_self() {
    platform::raise_signal(SIGTSTP);
}

// Real-terminal mouse reporting (SGR extended coordinates, mode 1006,
// plus button-event/drag tracking, mode 1002): the single shared source
// of truth for both `RawGuard::enable_with_mouse` (bish's own UI reading
// keys directly) and repl.rs's `sync_mouse_reporting` (mirroring a
// foreground job's own DECSET request instead -- see that function's own
// doc comment for why it can't just use `RawGuard` itself: it needs to
// track ON/OFF independently of any raw-mode guard's own lifetime).
pub const MOUSE_REPORTING_ENABLE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
// 1003 goes off here too, though nothing above turns it on: it is asked
// for separately (see `HOVER_TRACKING_ENABLE`) and by one view only,
// and a mode left set on the way out is one the next program inherits.
pub const MOUSE_REPORTING_DISABLE: &str = "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

// DECSET 1003, "any-event" tracking: report the pointer crossing a cell
// even with no button held. That is the whole difference between 1002
// and this one, and the only way a terminal can say where the mouse is
// resting rather than where it was dragged.
//
// Its own pair rather than part of the block above, for the reason
// bracketed paste has its own: this is asked for by one view (the
// editor, for hover) and for the duration of that view, not by
// everything that wants a mouse. It is also a *lot* of traffic -- one
// report per cell crossed -- which is worth paying only where something
// reads it.
/// Any-event mouse tracking: on exactly while something is reading it.
///
/// Process-global, because that is what a terminal mode is -- there is
/// one terminal, and its modes are not anybody's local variable. An
/// earlier attempt made this a guard owned by the editor's loop, and
/// that is worth recording as the wrong shape: the editor does not
/// unwind that loop on its way out (`:q!` reaches a prompt without the
/// call returning), so a `Drop` there never ran and the mode outlived
/// what it was for. Anything that draws says what it needs instead, and
/// the last word wins.
///
/// The mode can still outlive an editor in flows that neither redraw
/// nor return -- the same way this shell already leaves 1000/1002/1006
/// on across a session. The cost is motion reports arriving at a loop
/// that ignores them; `mouse_hover` is the switch for anyone who would
/// rather not pay it.
///
/// Idempotent, so callers can say it on every redraw without thinking
/// about what was set before.
pub fn set_hover_tracking(on: bool) {
    static ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if ON.swap(on, std::sync::atomic::Ordering::SeqCst) == on {
        return;
    }
    print!("{}", if on { HOVER_TRACKING_ENABLE } else { HOVER_TRACKING_DISABLE });
    let _ = io::stdout().flush();
}

/// What the real terminal calls itself.
///
/// Emitted only when it changes, and process-global for the same reason
/// `set_hover_tracking` is: there is one terminal, and what it is called
/// is not anybody's local variable. Nothing is sent when stdout is not a
/// terminal -- a title written into a pipe is bytes in somebody's data.
///
/// `safe_text` because a title arriving from a program in a pane is that
/// program's text, and `evil<ESC>[2J` in it would be an instruction to
/// the real terminal rather than a name.
pub fn set_window_title(title: &str) {
    static CURRENT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    if !crate::platform::is_terminal(1) {
        return;
    }
    let title = safe_text(title);
    let mut current = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    if *current == title {
        return;
    }
    // OSC 2 sets the window title alone. OSC 0 would set the icon name
    // with it, which is a second thing to have changed on the way out
    // for no gain.
    print!("\x1b]2;{title}\x07");
    let _ = io::stdout().flush();
    *current = title;
}

/// Remembers the terminal's own title, so bish can give it back.
///
/// xterm's title stack (`CSI 22;2t` to push, `CSI 23;2t` to pop), which
/// every terminal worth naming either implements or ignores -- and an
/// ignored push leaves the title exactly as bish would have left it
/// anyway, which is why this is worth doing unconditionally rather than
/// probing for support.
pub fn push_window_title() {
    if !crate::platform::is_terminal(1) {
        return;
    }
    print!("\x1b[22;2t");
    let _ = io::stdout().flush();
}

/// Gives the terminal its own title back, on the way out.
pub fn pop_window_title() {
    if !crate::platform::is_terminal(1) {
        return;
    }
    print!("\x1b[23;2t");
    let _ = io::stdout().flush();
}

pub const HOVER_TRACKING_ENABLE: &str = "\x1b[?1003h";
pub const HOVER_TRACKING_DISABLE: &str = "\x1b[?1003l";

// DECSET 2004: ask the terminal to wrap pasted text in `CSI 200 ~` and
// `CSI 201 ~`, so a burst of characters can be told apart from typing.
//
// Its own escape pair rather than a flag on `RawGuard` for the same
// reason mouse reporting has `sync_bracketed_paste` alongside the guard:
// this is asked for by one specific view for the duration of one
// specific mode, not by everything that happens to want raw mode.
pub const BRACKETED_PASTE_ENABLE: &str = "\x1b[?2004h";
pub const BRACKETED_PASTE_DISABLE: &str = "\x1b[?2004l";

/// RAII counterpart: on while it lives, off when it drops -- including
/// down every early return of whatever loop is holding it.
pub struct BracketedPasteGuard;

impl BracketedPasteGuard {
    pub fn enable() -> BracketedPasteGuard {
        use std::io::Write;
        print!("{BRACKETED_PASTE_ENABLE}");
        let _ = std::io::stdout().flush();
        BracketedPasteGuard
    }
}

impl Drop for BracketedPasteGuard {
    fn drop(&mut self) {
        use std::io::Write;
        print!("{BRACKETED_PASTE_DISABLE}");
        let _ = std::io::stdout().flush();
    }
}

// RAII guard: puts fd (almost always 0/stdin) into raw mode on construction,
// restores the terminal's prior settings on drop. Raw mode here means no
// line buffering (ICANON off), no local echo (we draw the line ourselves),
// no ^C/^Z-generates-a-signal behavior (ISIG off -- editor.rs reads those
// as plain bytes and decides what to do), and no output post-processing
// (OPOST off, so callers must emit "\r\n" explicitly instead of relying on
// the tty to translate "\n").
pub struct RawGuard {
    fd: i32,
    saved: TerminalMode,
    // Whether this guard also turned on real mouse reporting (see
    // `enable_with_mouse`) and so owes turning it back off on drop.
    // `enable`'s plain callers (drive_fg_job, which manages mouse
    // reporting itself gated on a job's own request; query_cursor_column,
    // a microsecond DSR query with nothing to click on) leave this false.
    mouse: bool,
}

/// Whether the terminal on `fd` has been put into raw mode.
///
/// ICANON is the bit that matters: with it set, the line discipline
/// holds typed bytes until a newline and a program reading the
/// terminal never sees them. On a pty the two ends share one set of
/// these settings, so `tcgetattr` on the *master* is how the side
/// driving it can tell that the program on the slave has taken the
/// terminal -- which the vimdiff harness needs before it types
/// anything (see its handshake).
///
/// That harness is its only caller, and it is a test, so this is one
/// too: a release build was otherwise warning about dead code that is
/// not dead, it just is not part of the shell.
#[cfg(test)]
pub(crate) fn is_raw(fd: i32) -> bool {
    platform::terminal_mode(fd).is_some_and(|mode| !platform::is_canonical(&mode))
}

impl RawGuard {
    pub fn enable(fd: i32) -> io::Result<RawGuard> {
        Self::enable_impl(fd, false)
    }

    // Same as `enable`, but also puts the real terminal into mouse-report
    // mode -- for the handful of call sites that are bish's own UI
    // reading keys directly (read_line, run_normal_mode_navigation), as
    // opposed to raw mode acquired for some other reason. Never held
    // globally (see this codebase's own "raw mode is acquired
    // independently, per call" convention) -- each such call site gets
    // its own guard, and Drop below unwinds mouse reporting right along
    // with the termios restore, so every one of read_line's several exit
    // paths (Eof, Enter, Ctrl-C, Ctrl-D, Ctrl-Z, ...) gets this for free.
    /// Raw mode, with mouse reporting only if asked for -- the
    /// `mouse` bishopt's own switch. Off is not "ignore the events":
    /// reporting is never enabled, so the terminal keeps its own
    /// click-and-drag selection, which is the whole reason to want it
    /// off.
    pub fn enable_maybe_mouse(fd: i32, mouse: bool) -> io::Result<RawGuard> {
        if mouse { RawGuard::enable_with_mouse(fd) } else { RawGuard::enable(fd) }
    }

    pub fn enable_with_mouse(fd: i32) -> io::Result<RawGuard> {
        Self::enable_impl(fd, true)
    }

    fn enable_impl(fd: i32, mouse: bool) -> io::Result<RawGuard> {
        let Some(saved) = platform::terminal_mode(fd) else {
            return Err(io::Error::last_os_error());
        };
        platform::set_terminal_mode(fd, &platform::raw_mode(&saved))?;
        if mouse {
            print!("{MOUSE_REPORTING_ENABLE}");
            let _ = io::stdout().flush();
        }
        Ok(RawGuard { fd, saved, mouse })
    }

    // Temporarily puts the terminal back into exactly the settings it
    // had before this guard ever went raw, without giving up the guard
    // itself (no Drop runs, mouse reporting is untouched either way --
    // see its own doc comment for why that specifically isn't part of
    // this). For a caller that needs one ordinary, cooked-mode blocking
    // read to behave the way it would outside a raw-mode session --
    // kernel-driven echo, line editing, backspace, the works -- bish's
    // own `read` builtin among them, which does none of that itself
    // (unlike editor.rs's line editor, which deliberately relies on raw
    // mode to draw its own line) and so silently breaks (invisible
    // typing, no backspace) under a raw terminal the debugger holds for
    // its own, unrelated reason. Pair with `resume_raw` once whatever
    // needed cooked mode is done.
    //
    // Deliberately does NOT just restore `self.saved` -- a debugger
    // session invoked from *inside* an already-raw outer session (`:dbg`
    // launched from the real file editor's own command mode, itself
    // already holding its own RawGuard) would have captured an
    // *already-raw* baseline as `self.saved`, so "restoring" it would
    // silently just reapply raw mode, not cooked mode (a real,
    // interactively-caught bug: `read -p`'s own echo stayed broken
    // specifically in this nested case, while working fine for a
    // standalone `bish tool debug`, where the real terminal genuinely
    // was cooked when the one guard captured it). Deriving cooked mode
    // fresh from whatever the *live* termios happens to be right now --
    // the exact inverse of derive_raw's own flag-clearing -- is correct
    // regardless of nesting depth, since it only ever flips back on the
    // specific bits raw mode turns off, never assumes a stored snapshot
    // is still the right baseline to return to.
    pub fn suspend_raw(&self) {
        let Some(current) = platform::terminal_mode(self.fd) else { return };
        let _ = platform::set_terminal_mode(self.fd, &platform::cooked_mode(&current));
    }

    // The inverse of `suspend_raw` -- re-derives raw settings fresh from
    // whatever the live termios is right now (same reasoning as
    // suspend_raw's own doc comment: not from `self.saved`, which may
    // not be this session's real cooked baseline at all in a nested
    // invocation).
    pub fn resume_raw(&self) {
        let Some(current) = platform::terminal_mode(self.fd) else { return };
        let _ = platform::set_terminal_mode(self.fd, &platform::raw_mode(&current));
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if self.mouse {
            print!("{MOUSE_REPORTING_DISABLE}");
            let _ = io::stdout().flush();
        }
        let _ = platform::set_terminal_mode(self.fd, &self.saved);
    }
}

// RAII guard: turns off local echo (ECHO) on fd (almost always 0/stdin)
// while leaving everything else exactly as it already was -- ICANON
// (kernel-driven line editing, so Enter/backspace still behave
// normally), ISIG (Ctrl-C/Ctrl-Z still generate signals), IEXTEN, the
// works. Unlike `RawGuard`, this is NOT raw mode: a caller using this
// still gets one ordinary, kernel-buffered cooked-mode line -- just
// without the terminal echoing typed characters back. This is exactly
// `read -s`'s own contract (bash: read a line from the terminal
// normally, don't show what's being typed), and nothing else in this
// codebase needs "echo off, otherwise unchanged" -- editor.rs's own
// line editor wants full raw mode (RawGuard) since it draws its own
// line from scratch.
pub struct NoEchoGuard {
    fd: i32,
    saved: TerminalMode,
}

impl NoEchoGuard {
    pub fn enable(fd: i32) -> io::Result<NoEchoGuard> {
        let Some(saved) = platform::terminal_mode(fd) else {
            return Err(io::Error::last_os_error());
        };
        platform::set_terminal_mode(fd, &platform::echoless_mode(&saved))?;
        Ok(NoEchoGuard { fd, saved })
    }
}

impl Drop for NoEchoGuard {
    fn drop(&mut self) {
        let _ = platform::set_terminal_mode(self.fd, &self.saved);
    }
}

/// One character, made safe to write to a terminal.
///
/// A control character reaching a terminal is not text, it is an
/// instruction -- and most of the text this shell draws was written by
/// somebody else: a filename, a git ref, a line of a file, a completion
/// candidate. `evil<ESC>[2J.txt` clears the screen merely by appearing
/// in a listing. Same hazard `url::is_safe` exists for, one layer down.
///
/// One character in, one character out, so every index into the text --
/// a fuzzy match position, a selection span, a caret column -- still
/// means what it meant. U+FFFD rather than `?` because it says "this
/// could not be shown" rather than looking like part of the name.
pub fn safe_char(c: char) -> char {
    match c.is_control() || is_bidi_control(c) {
        true => '\u{FFFD}',
        false => c,
    }
}

/// The characters that reorder what is written around them.
///
/// `char::is_control` is C0 and C1 and nothing else, so every one of
/// these went through untouched -- and they are instructions to a
/// renderer in exactly the sense the comment above means. A file named
/// `safe<U+202E>txt.exe` lists itself as `safeexe.txt`, which is the
/// oldest spoof there is, and a source line with an override in a
/// comment reads one way and runs another, which is Trojan Source
/// (CVE-2021-42574). Both were measured reaching the terminal: the
/// filename through the browser's listing, the line through the editor.
///
/// This is also the one part of the sanitiser that is not purely about
/// safety. bish draws a grid and puts the cursor at a column it computed
/// itself; a run the terminal is free to reverse has no stable column,
/// so bidirectional text was never rendered *correctly* here, it was
/// rendered in an order bish's own arithmetic did not agree with. One
/// visible replacement per control is an honest answer to that, where
/// passing them through was a quiet wrong one -- and it is the trade
/// being made, not an oversight: a genuine Hebrew or Arabic document
/// shows these markers in bish where it would reorder elsewhere.
///
/// Deliberately only the reordering ones. The zero-width joiners are
/// invisible rather than reordering, and one of them is how an emoji
/// family is spelled.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// `safe_char` over a whole string, for the places that build terminal
/// output as text rather than as cells.
pub fn safe_text(s: &str) -> String {
    s.chars().map(safe_char).collect()
}

// True if a byte is available on stdin within timeout_ms. Used to tell a
// standalone Esc keypress (nothing follows) apart from the start of a
// terminal escape sequence (whose bytes arrive back-to-back) without
// blocking forever on the ambiguous case. The actual poll(2) FFI lives
// in poll.rs (shared with the main event loop) -- this is just the
// fixed-to-stdin convenience wrapper that predates that module.
pub fn stdin_ready(timeout_ms: i32) -> bool {
    crate::poll::poll_one(0, timeout_ms)
}

fn read_one_byte() -> Option<u8> {
    let mut b = [0u8; 1];
    match platform::read_bytes(0, &mut b) {
        Ok(1) => Some(b[0]),
        _ => None,
    }
}

// How long to wait for the terminal to answer a DSR query before giving
// up -- generous relative to how fast a real (local) terminal emulator
// actually answers (well under a millisecond in practice), but still
// short enough that a terminal/environment that doesn't support DSR at
// all (piped stdout, an unusual emulator) doesn't stall a prompt draw.
const DSR_TIMEOUT_MS: i32 = 200;

// Device Status Report (`\x1b[6n`): asks the real terminal for its own
// actual cursor position and returns the column (1-indexed). Used by
// repl.rs to find out whether an external command's own output (which,
// unlike bish's own builtins, bish never sees a byte of -- it goes
// straight from the child process to the inherited terminal fd) left
// the cursor mid-row, the one case Shell::real_output_needs_newline
// can't track on its own. Puts fd 0 into raw mode for the duration (the
// reply arrives on stdin, the same as any keystroke, and needs reading
// back before whatever line-buffering/echo mode the terminal would
// otherwise apply to it) -- restored via RawGuard's own Drop regardless
// of how this returns. `None` on any failure to enable raw mode, a
// timeout (some terminals/environments don't answer DSR at all), or a
// reply that doesn't parse as the expected `ESC [ row ; col R` -- every
// caller treats that the same as "don't know," not as an error, and
// just leaves the terminal alone rather than guessing.
pub fn query_cursor_column() -> Option<usize> {
    let _guard = RawGuard::enable(0).ok()?;
    {
        use std::io::Write;
        print!("\x1b[6n");
        std::io::stdout().flush().ok()?;
    }
    if !stdin_ready(DSR_TIMEOUT_MS) {
        return None;
    }
    if read_one_byte()? != 0x1b {
        return None;
    }
    if read_one_byte()? != b'[' {
        return None;
    }
    // Row digits, up to the ';' -- not needed, just consumed so parsing
    // can continue past them to the column.
    loop {
        let b = read_one_byte()?;
        if b == b';' {
            break;
        }
        if !b.is_ascii_digit() {
            return None;
        }
    }
    let mut col = String::new();
    loop {
        let b = read_one_byte()?;
        if b == b'R' {
            break;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        col.push(b as char);
    }
    col.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{safe_char, safe_text};

    // The sanitiser's own contract: one character in, one out, and
    // nothing left that a terminal reads as an instruction rather than
    // as text.
    #[test]
    fn safe_text_neutralises_what_reorders_and_keeps_what_joins() {
        // What it always did.
        assert_eq!(safe_text("evil\x1b[2J.txt"), "evil\u{fffd}[2J.txt");
        // And what it used to let through: `char::is_control` is C0 and
        // C1 only, so every bidirectional control was text as far as it
        // was concerned.
        for c in [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}',
            '\u{2069}',
        ] {
            assert_eq!(safe_char(c), '\u{fffd}', "{c:?} reorders what is written around it");
        }
        // The spoof this closes, as it would arrive: a name that lists
        // itself as `safeexe.txt`.
        assert_eq!(safe_text("safe\u{202e}txt.exe"), "safe\u{fffd}txt.exe");
        // One character in, one out -- every match position, selection
        // span and caret column depends on it.
        assert_eq!(safe_text("a\u{202e}b\u{2069}c").chars().count(), 5);
        // The joiners are invisible rather than reordering, and one of
        // them is how an emoji family is spelled.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        assert_eq!(safe_text(family), family);
        assert_eq!(safe_char('\u{200b}'), '\u{200b}');
    }
}
