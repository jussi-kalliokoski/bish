// The terminal as bish needs it: raw mode for anything drawing its own
// line, echo off for `read -s`, and the signals an interactive shell
// cares about.
//
// The OS side (the terminal's mode, and deriving raw, cooked or
// echoless from it) is in `platform`, as the bits and the `termios`
// layout differ between OSes. Here is when to ask for which, and the
// escape sequences that don't involve the kernel.

use crate::platform::{self, TerminalMode};
use std::io::{self, Write};

// The signal numbers come from the platform layer: BSD renumbered the
// job-control ones, so `SIGTSTP` differs between macOS and Linux.
pub const SIGINT: i32 = platform::SIGINT;
#[allow(dead_code)]
pub const SIGHUP: i32 = platform::SIGHUP;
pub const SIGTSTP: i32 = platform::SIGTSTP;
pub const SIGTTIN: i32 = platform::SIGTTIN;
pub const SIGTTOU: i32 = platform::SIGTTOU;

// Makes the shell immune to SIGINT for the rest of its life. SIG_IGN is
// inherited by children and survives exec (only handlers are reset), so
// every spawn site (`apply_fd_redirects`, `command`,
// `pty::spawn_attached`) resets SIGINT to SIG_DFL in its pre-exec hook,
// or the child would ignore Ctrl-C. Call once, at interactive startup.
pub fn ignore_sigint() {
    platform::ignore_signal(SIGINT);
}

// Lets a detached session daemon survive the terminal or ssh session
// that launched it, which sends SIGHUP to its foreground group.
// `session::daemonize` also `setsid`s; this covers the window before.
// Inherited across fork and exec, like `ignore_sigint`.
#[allow(dead_code)]
pub fn ignore_sighup() {
    platform::ignore_signal(SIGHUP);
}

// Job control: the shell ignores SIGTTIN and SIGTTOU. Once a job owns
// the terminal (`tcsetpgrp`), the shell's group is in the background,
// and touching the terminal, including the `tcsetpgrp` that takes it
// back, would stop the shell. SIGTSTP isn't ignored: `suspend_self`
// raises it. Children have both reset to SIG_DFL in exec.rs's pre-exec
// hooks.
pub fn ignore_tty_signals() {
    platform::ignore_signal(SIGTTIN);
    platform::ignore_signal(SIGTTOU);
}

// Suspends the shell itself (not a job) with SIGTSTP, as any program
// stopped from its terminal; no process groups involved. Returns once
// something resumes it (`fg` in the parent shell, `kill -CONT`).
pub fn suspend_self() {
    platform::raise_signal(SIGTSTP);
}

// Mouse reporting (button and drag tracking, 1002, in SGR coordinates,
// 1006), shared by `RawGuard::enable_with_mouse` and repl.rs's
// `sync_mouse_reporting`, which mirrors a job's own request
// independently of any guard.
pub const MOUSE_REPORTING_ENABLE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
// 1003 goes off here too though nothing above enables it: it's asked
// for separately (`HOVER_TRACKING_ENABLE`), and a mode left on is
// inherited by the next program.
pub const MOUSE_REPORTING_DISABLE: &str = "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

// Any-event mouse tracking: on exactly while something reads it.
//
// Process-global, as a terminal mode is: there is one terminal. Not a
// guard owned by the editor's loop: `:q!` reaches a prompt without
// that loop returning, so a `Drop` there wouldn't run. Whatever draws
// says what it needs, and the last word wins.
//
// The mode can outlive an editor in flows that neither redraw nor
// return, as 1000/1002/1006 stay on across a session; the cost is
// motion reports a loop ignores, and `mouse_hover` turns it off.
//
// Idempotent, so callers can set it on every redraw.
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

/// What the terminal's title is. Emitted only on change, process-global
/// like `set_hover_tracking`, and not at all when stdout isn't a
/// terminal, where it would be bytes in someone's data. `safe_text`
/// because a program's title is its text, and `evil<ESC>[2J` would be
/// an instruction to the terminal.
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
    // OSC 2 sets the window title only; OSC 0 would also set the icon name,
    // one more thing to restore.
    print!("\x1b]2;{title}\x07");
    let _ = io::stdout().flush();
    *current = title;
}

/// Saves the terminal's title for bish to give back, with xterm's title
/// stack (`CSI 22;2t` push, `CSI 23;2t` pop). Terminals without it
/// ignore the push, leaving the title as bish would have, so there's no
/// need to probe.
pub fn push_window_title() {
    if !crate::platform::is_terminal(1) {
        return;
    }
    print!("\x1b[22;2t");
    let _ = io::stdout().flush();
}

/// Gives the terminal its title back, on the way out.
pub fn pop_window_title() {
    if !crate::platform::is_terminal(1) {
        return;
    }
    print!("\x1b[23;2t");
    let _ = io::stdout().flush();
}

// DECSET 1003, any-event tracking: reports the pointer crossing cells with
// no button held, the only way to know where it rests. Separate from the
// mouse block above: one view asks for it (the editor, for hover), and it
// is a report per cell crossed.
pub const HOVER_TRACKING_ENABLE: &str = "\x1b[?1003h";
pub const HOVER_TRACKING_DISABLE: &str = "\x1b[?1003l";

// DECSET 2004: the terminal wraps pastes in `CSI 200 ~` and `CSI 201 ~`,
// so a burst can be told from typing. Separate from `RawGuard`: one
// view asks for it during one mode.
pub const BRACKETED_PASTE_ENABLE: &str = "\x1b[?2004h";
pub const BRACKETED_PASTE_DISABLE: &str = "\x1b[?2004l";

/// On while it lives, off when dropped, on every early return too.
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

// Puts fd (usually stdin) into raw mode, restoring the previous
// settings on drop: no line buffering (ICANON), no echo (we draw the
// line), no signal keys (ISIG; editor.rs reads ^C and ^Z as bytes), and
// no output processing (OPOST; callers write "\r\n").
pub struct RawGuard {
    fd: i32,
    saved: TerminalMode,
    // Whether this guard turned on mouse reporting (`enable_with_mouse`) and
    // so turns it off on drop. Plain `enable` callers leave it false.
    mouse: bool,
}

/// Whether the terminal on `fd` is in raw mode. ICANON is the bit: with
/// it set, the line discipline holds bytes until a newline. A pty's two
/// ends share these settings, so `tcgetattr` on the master tells the
/// driving side that the program on the slave took the terminal, which
/// the vimdiff harness waits for before typing. Test-only, as that's its
/// only caller.
#[cfg(test)]
pub(crate) fn is_raw(fd: i32) -> bool {
    platform::terminal_mode(fd).is_some_and(|mode| !platform::is_canonical(&mode))
}

impl RawGuard {
    pub fn enable(fd: i32) -> io::Result<RawGuard> {
        Self::enable_impl(fd, false)
    }

    // Raw mode, with mouse reporting if the `mouse` bishopt asks. Off
    // never enables reporting, so the terminal keeps its own selection.
    /// Raw mode, with mouse reporting only if asked for -- the
    /// `mouse` bishopt's own switch. Off is not "ignore the events":
    /// reporting is never enabled, so the terminal keeps its own
    /// click-and-drag selection, which is the whole reason to want it
    /// off.
    pub fn enable_maybe_mouse(fd: i32, mouse: bool) -> io::Result<RawGuard> {
        if mouse { RawGuard::enable_with_mouse(fd) } else { RawGuard::enable(fd) }
    }

    // `enable`, plus mouse reporting, for bish's own UI reading keys
    // (`read_line`, Normal mode). Each call site gets its own guard, and drop
    // turns reporting off with the termios restore, on every exit path.
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

    // Puts the terminal back in cooked mode without giving up the guard
    // (no drop; mouse reporting untouched), for one ordinary blocking read
    // with kernel echo and line editing, such as the `read` builtin under
    // the debugger's raw terminal. Pair with `resume_raw`.
    //
    // Derived from the live termios rather than restoring `self.saved`: a
    // guard taken inside another raw session saved a raw baseline, so
    // restoring it would leave raw mode on (`read -p` lost its echo under
    // `:dbg` from the editor). Flipping back only the bits raw mode clears
    // works at any nesting depth.
    pub fn suspend_raw(&self) {
        let Some(current) = platform::terminal_mode(self.fd) else { return };
        let _ = platform::set_terminal_mode(self.fd, &platform::cooked_mode(&current));
    }

    // The inverse of `suspend_raw`, derived from the live termios for the
    // same reason.
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

// Turns off echo on fd (usually stdin) and nothing else: ICANON, ISIG
// and IEXTEN stay, so the kernel still edits the line and Ctrl-C still
// signals. Not raw mode: one ordinary cooked line, unechoed, as
// `read -s` wants.
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
/// A control character reaching a terminal is an instruction, and most
/// text this shell draws was written by someone else: a filename, a git
/// ref, a line of a file. `evil<ESC>[2J.txt` clears the screen just by
/// being listed. `url::is_safe` guards the same hazard a layer down.
///
/// One character in, one out, so match positions, selection spans and
/// caret columns keep their meaning. U+FFFD rather than `?`, which
/// would look like part of the name.
pub fn safe_char(c: char) -> char {
    match c.is_control() || is_bidi_control(c) {
        true => '\u{FFFD}',
        false => c,
    }
}

/// The characters that reorder text around them, which
/// `char::is_control` (C0 and C1 only) lets through. A file named
/// `safe<U+202E>txt.exe` lists as `safeexe.txt`, and a source line with
/// an override in a comment reads one way and runs another (Trojan
/// Source, CVE-2021-42574).
///
/// Not only about safety: bish computes cursor columns itself, and text
/// the terminal may reverse has no stable column, so bidirectional
/// text never rendered correctly here anyway. A visible replacement per
/// control is the trade: a Hebrew or Arabic document shows markers in
/// bish where it would reorder elsewhere.
///
/// Only the reordering ones: zero-width joiners are invisible, and one
/// spells an emoji family.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// `safe_char` over a string, for output built as text rather than cells.
pub fn safe_text(s: &str) -> String {
    s.chars().map(safe_char).collect()
}

// Whether a byte arrives on stdin within `timeout_ms`, to tell a lone
// Esc from the start of an escape sequence without blocking.
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

// How long to wait for a DSR reply: far above a local terminal's
// sub-millisecond answer, short enough that one that never answers
// (piped stdout, an odd emulator) doesn't stall the prompt.
const DSR_TIMEOUT_MS: i32 = 200;

// Asks the terminal for its cursor position (DSR, `\x1b[6n`) and
// returns the 1-based column. repl.rs uses it to learn whether an
// external command's output, which bish never sees, left the cursor
// mid-row, which `Shell::real_output_needs_newline` can't track. Stdin
// is raw meanwhile, as the reply arrives there. `None` on any failure
// (no raw mode, no answer, an unparseable reply); callers treat that as
// don't know.
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
    // Row digits up to the `;`, consumed to reach the column.
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

    // One character in, one out, and nothing left a terminal reads as an
    // instruction.
    #[test]
    fn safe_text_neutralises_what_reorders_and_keeps_what_joins() {
        // Escape sequences.
        assert_eq!(safe_text("evil\x1b[2J.txt"), "evil\u{fffd}[2J.txt");
        // Bidirectional controls, which `char::is_control` doesn't cover.
        for c in [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}',
            '\u{2069}',
        ] {
            assert_eq!(safe_char(c), '\u{fffd}', "{c:?} reorders what is written around it");
        }
        // The spoof, as it would arrive: a name listing as `safeexe.txt`.
        assert_eq!(safe_text("safe\u{202e}txt.exe"), "safe\u{fffd}txt.exe");
        // One in, one out: positions and columns depend on it.
        assert_eq!(safe_text("a\u{202e}b\u{2069}c").chars().count(), 5);
        // Joiners are invisible, not reordering; one spells an emoji family.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        assert_eq!(safe_text(family), family);
        assert_eq!(safe_char('\u{200b}'), '\u{200b}');
    }
}
