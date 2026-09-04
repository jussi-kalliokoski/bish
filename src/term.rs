// Raw termios control, hand-rolled against glibc's Linux x86_64 struct
// layout (no libc crate -- this project stays dependency-free). Shared by
// the interactive line editor (editor.rs) and `read -s` (exec.rs).

use std::io::{self, Write};

// Matches glibc's `struct termios` (bits/termios.h) on Linux x86_64 field
// for field; repr(C) then reproduces the same padding/alignment a C
// compiler would insert, so this can be handed straight to tcgetattr/
// tcsetattr.
#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 32],
    c_ispeed: u32,
    c_ospeed: u32,
}

const ICANON: u32 = 0o0000002;
const ECHO: u32 = 0o0000010;
const ISIG: u32 = 0o0000001;
const IEXTEN: u32 = 0o0100000;
const IGNBRK: u32 = 0o0000001;
const BRKINT: u32 = 0o0000002;
const PARMRK: u32 = 0o0000010;
const ISTRIP: u32 = 0o0000040;
const INLCR: u32 = 0o0000100;
const IGNCR: u32 = 0o0000200;
const ICRNL: u32 = 0o0000400;
const IXON: u32 = 0o0002000;
const OPOST: u32 = 0o0000001;
const CSIZE: u32 = 0o0000060;
const CS8: u32 = 0o0000060;

const VMIN: usize = 6;
const VTIME: usize = 5;

const TCSANOW: i32 = 0;

unsafe extern "C" {
    fn tcgetattr(fd: i32, termios_p: *mut Termios) -> i32;
    fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const Termios) -> i32;
    fn raise(sig: i32) -> i32;
}

// Linux signal numbers (stable/standard across architectures -- safe to
// hardcode rather than pulling in exec.rs's trap-oriented signal tables).
pub const SIGINT: i32 = 2;
// Not yet called from anywhere -- session.rs's own daemonize (a
// follow-up commit, once there's an actual accept loop to daemonize
// into) is the first real caller. Same "land the seam, wire it in
// later" pattern pty.rs's own module doc comment already names.
#[allow(dead_code)]
pub const SIGHUP: i32 = 1;
pub const SIGTSTP: i32 = 20;
pub const SIGTTIN: i32 = 21;
pub const SIGTTOU: i32 = 22;
const SIG_IGN: usize = 1;

unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

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
    unsafe {
        signal(SIGINT, SIG_IGN);
    }
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
    unsafe {
        signal(SIGHUP, SIG_IGN);
    }
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
    unsafe {
        signal(SIGTTIN, SIG_IGN);
        signal(SIGTTOU, SIG_IGN);
    }
}

// Suspends the *shell itself* (not a child job) via SIGTSTP, exactly like
// any other well-behaved interactive program stopped from its own
// controlling terminal. This is deliberately not job control: there's no
// process-group/tcsetpgrp reassignment here, just the same self-suspend
// every foreground program gets for free when it doesn't otherwise handle
// SIGTSTP. Returns once something (`fg` in the invoking shell, `kill
// -CONT`, ...) resumes this process.
pub fn suspend_self() {
    unsafe {
        raise(SIGTSTP);
    }
}

// Real-terminal mouse reporting (SGR extended coordinates, mode 1006,
// plus button-event/drag tracking, mode 1002): the single shared source
// of truth for both `RawGuard::enable_with_mouse` (bish's own UI reading
// keys directly) and repl.rs's `sync_mouse_reporting` (mirroring a
// foreground job's own DECSET request instead -- see that function's own
// doc comment for why it can't just use `RawGuard` itself: it needs to
// track ON/OFF independently of any raw-mode guard's own lifetime).
pub const MOUSE_REPORTING_ENABLE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub const MOUSE_REPORTING_DISABLE: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

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
    saved: Termios,
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
pub(crate) fn is_raw(fd: i32) -> bool {
    let mut current: Termios = unsafe { std::mem::zeroed() };
    if unsafe { tcgetattr(fd, &mut current) } != 0 {
        return false;
    }
    current.c_lflag & ICANON == 0
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
        let mut saved: Termios = unsafe { std::mem::zeroed() };
        if unsafe { tcgetattr(fd, &mut saved) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let raw = derive_raw(&saved);
        if unsafe { tcsetattr(fd, TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
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
        let Some(current) = query(self.fd) else { return };
        let cooked = derive_cooked(&current);
        unsafe {
            tcsetattr(self.fd, TCSANOW, &cooked);
        }
    }

    // The inverse of `suspend_raw` -- re-derives raw settings fresh from
    // whatever the live termios is right now (same reasoning as
    // suspend_raw's own doc comment: not from `self.saved`, which may
    // not be this session's real cooked baseline at all in a nested
    // invocation).
    pub fn resume_raw(&self) {
        let Some(current) = query(self.fd) else { return };
        let raw = derive_raw(&current);
        unsafe {
            tcsetattr(self.fd, TCSANOW, &raw);
        }
    }
}

fn query(fd: i32) -> Option<Termios> {
    let mut current: Termios = unsafe { std::mem::zeroed() };
    if unsafe { tcgetattr(fd, &mut current) } != 0 {
        return None;
    }
    Some(current)
}

// The raw-mode derivation `enable_impl` applies once at construction,
// factored out so `RawGuard::resume_raw` can re-derive the identical
// settings without duplicating the flag arithmetic.
fn derive_raw(base: &Termios) -> Termios {
    let mut raw = *base;
    raw.c_iflag &= !(IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR | ICRNL | IXON);
    raw.c_oflag &= !OPOST;
    raw.c_lflag &= !(ECHO | ICANON | IEXTEN | ISIG);
    raw.c_cflag &= !CSIZE;
    raw.c_cflag |= CS8;
    raw.c_cc[VMIN] = 1;
    raw.c_cc[VTIME] = 0;
    raw
}

// NOT the literal bit-for-bit inverse of derive_raw: raw mode clears
// IGNCR alongside ICRNL (both off, so a bare CR passes through
// untranslated and unconsumed either way), but IGNCR and ICRNL aren't
// independent -- POSIX has IGNCR take priority, discarding CR entirely
// before ICRNL ever gets a say. Turning *both* back on (the naive
// symmetric inverse, tried first here and caught by interactive
// testing) silently ate every Enter keystroke: a script's own `read`
// would echo whatever was typed but never see a line terminator at
// all, hanging forever on something that looked like it should have
// worked. Real cooked mode leaves IGNCR/INLCR/ISTRIP/PARMRK/IGNBRK/
// BRKINT alone -- only ICANON/ECHO/ISIG/IEXTEN, OPOST, and ICRNL/IXON
// are what actually need forcing back on for kernel-driven line
// editing/echo to behave normally, applied to whatever the *current*
// termios is rather than a potentially-stale stored snapshot (see
// suspend_raw's own doc comment for why that distinction matters).
fn derive_cooked(base: &Termios) -> Termios {
    let mut cooked = *base;
    cooked.c_iflag |= ICRNL | IXON;
    cooked.c_oflag |= OPOST;
    cooked.c_lflag |= ECHO | ICANON | IEXTEN | ISIG;
    cooked
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if self.mouse {
            print!("{MOUSE_REPORTING_DISABLE}");
            let _ = io::stdout().flush();
        }
        unsafe {
            tcsetattr(self.fd, TCSANOW, &self.saved);
        }
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
    saved: Termios,
}

impl NoEchoGuard {
    pub fn enable(fd: i32) -> io::Result<NoEchoGuard> {
        let Some(saved) = query(fd) else {
            return Err(io::Error::last_os_error());
        };
        let mut silent = saved;
        silent.c_lflag &= !ECHO;
        if unsafe { tcsetattr(fd, TCSANOW, &silent) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(NoEchoGuard { fd, saved })
    }
}

impl Drop for NoEchoGuard {
    fn drop(&mut self) {
        unsafe {
            tcsetattr(self.fd, TCSANOW, &self.saved);
        }
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
    match c.is_control() {
        true => '\u{FFFD}',
        false => c,
    }
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

unsafe extern "C" {
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
}

fn read_one_byte() -> Option<u8> {
    let mut b = [0u8; 1];
    loop {
        let n = unsafe { read(0, b.as_mut_ptr(), 1) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        return if n == 0 { None } else { Some(b[0]) };
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
