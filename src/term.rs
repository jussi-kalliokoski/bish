// Raw termios control, hand-rolled against glibc's Linux x86_64 struct
// layout (no libc crate -- this project stays dependency-free). Shared by
// the interactive line editor (editor.rs) and, eventually, `read -s`.

use std::io;

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
}

impl RawGuard {
    pub fn enable(fd: i32) -> io::Result<RawGuard> {
        let mut saved: Termios = unsafe { std::mem::zeroed() };
        if unsafe { tcgetattr(fd, &mut saved) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = saved;
        raw.c_iflag &= !(IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR | ICRNL | IXON);
        raw.c_oflag &= !OPOST;
        raw.c_lflag &= !(ECHO | ICANON | IEXTEN | ISIG);
        raw.c_cflag &= !CSIZE;
        raw.c_cflag |= CS8;
        raw.c_cc[VMIN] = 1;
        raw.c_cc[VTIME] = 0;
        if unsafe { tcsetattr(fd, TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(RawGuard { fd, saved })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe {
            tcsetattr(self.fd, TCSANOW, &self.saved);
        }
    }
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
const POLLIN: i16 = 0x0001;

unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
}

// True if a byte is available on stdin within timeout_ms. Used to tell a
// standalone Esc keypress (nothing follows) apart from the start of a
// terminal escape sequence (whose bytes arrive back-to-back) without
// blocking forever on the ambiguous case.
pub fn stdin_ready(timeout_ms: i32) -> bool {
    let mut pfd = PollFd { fd: 0, events: POLLIN, revents: 0 };
    unsafe { poll(&mut pfd, 1, timeout_ms) > 0 && (pfd.revents & POLLIN) != 0 }
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
