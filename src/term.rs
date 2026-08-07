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
const SIG_IGN: usize = 1;

unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

// Makes the shell itself immune to SIGINT for the rest of the process's
// life, the same trick real interactive shells use instead of process-
// group/job-control plumbing: a *caught* signal (which SIG_IGN counts as)
// is reset to its default disposition by exec() (POSIX-guaranteed), so a
// foreground child still dies/interrupts normally on Ctrl-C while bish,
// which keeps ignoring it, survives. Call once, at interactive startup.
pub fn ignore_sigint() {
    unsafe {
        signal(SIGINT, SIG_IGN);
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
