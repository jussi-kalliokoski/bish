// Hand-rolled pseudo-terminal allocation and control, in the same
// no-libc-crate, extern "C"-against-glibc style as term.rs. Provides the
// primitive `Pty::open()` (allocate a master/slave pair) and
// `spawn_attached()` (launch a child with the slave side as its
// controlling terminal), plus TIOCGWINSZ/TIOCSWINSZ helpers. Full-screen
// programs (vim, htop, less, ...) actively probe isatty()/TIOCGWINSZ and
// misbehave without a real pty; a hidden window's job output also needs
// to be captured on the master side rather than inherited straight to
// bish's own real terminal fd. Consumed by the future VT100 emulator and
// compositor -- this module only builds and owns the pty itself.
//
// Wired into exec.rs (run_single's background-job spawn path attaches a
// pty when promoted and unredirected) and repl.rs (drives a fg'd job's
// pty master directly). #![allow(dead_code)] stays regardless -- some
// items here (e.g. Pty::resize) are for future callers.
#![allow(dead_code)]

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

// Linux x86_64 ioctl request numbers (stable/standard, same "safe to
// hardcode" reasoning term.rs already uses for signal numbers).
const TIOCGWINSZ: u64 = 0x5413;
const TIOCSWINSZ: u64 = 0x5414;
const TIOCSCTTY: u64 = 0x540E;

const O_RDWR: i32 = 0o2;
const O_NOCTTY: i32 = 0o400;
const F_SETFD: i32 = 2;
const FD_CLOEXEC: i32 = 1;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0o4000;
// Linux signal number (stable/standard -- see term.rs's own comment on
// hardcoding these) and disposition constant for resetting SIGINT below.
const SIGINT: i32 = 2;
const SIG_DFL: usize = 0;

unsafe extern "C" {
    fn posix_openpt(flags: i32) -> i32;
    fn grantpt(fd: i32) -> i32;
    fn unlockpt(fd: i32) -> i32;
    fn ptsname_r(fd: i32, buf: *mut u8, buflen: usize) -> i32;
    fn ioctl(fd: i32, request: u64, arg: usize) -> i32;
    fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    fn setsid() -> i32;
    fn signal(signum: i32, handler: usize) -> usize;
    #[link_name = "open"]
    fn c_open(path: *const i8, flags: i32, mode: i32) -> i32;
    fn close(fd: i32) -> i32;
    fn dup2(oldfd: i32, newfd: i32) -> i32;
}

// A master/slave pty pair. `master` is the end bish itself reads/writes
// (the "far end" of the terminal from the child's perspective); `slave_path`
// (e.g. "/dev/pts/7") is opened fresh inside the child by spawn_attached,
// never held open here -- keeping only the path (not an fd) avoids leaking
// a slave fd into every future child spawned from this process.
pub struct Pty {
    pub master: File,
    pub slave_path: String,
}

pub fn open() -> io::Result<Pty> {
    let master_fd = unsafe { posix_openpt(O_RDWR | O_NOCTTY) };
    if master_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Close-on-exec so ordinary child processes (external commands, not
    // explicitly attached to this pty) never inherit the master fd.
    unsafe { fcntl(master_fd, F_SETFD, FD_CLOEXEC) };

    let setup = || -> io::Result<String> {
        if unsafe { grantpt(master_fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { unlockpt(master_fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = [0u8; 64];
        if unsafe { ptsname_r(master_fd, buf.as_mut_ptr(), buf.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let len = buf.iter().position(|&b| b == 0).unwrap_or(0);
        Ok(String::from_utf8_lossy(&buf[..len]).into_owned())
    };

    match setup() {
        Ok(slave_path) => Ok(Pty { master: unsafe { File::from_raw_fd(master_fd) }, slave_path }),
        Err(e) => {
            unsafe { close(master_fd) };
            Err(e)
        }
    }
}

// Spawns `cmd` with its stdin/stdout/stderr replaced by a freshly-opened
// slave fd for `slave_path`, made its controlling terminal. Mirrors what a
// real terminal emulator's shell-spawning does: setsid() to leave bish's
// own process group/session, open the slave (the first tty a session
// leader opens without one yet becomes its controlling terminal on
// Linux, but TIOCSCTTY is called explicitly here to not depend on that
// implicit-acquisition subtlety), then dup2 it onto 0/1/2.
pub fn spawn_attached(mut cmd: Command, slave_path: &str) -> io::Result<Child> {
    let path = CString::new(slave_path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    unsafe {
        cmd.pre_exec(move || {
            // bish ignores SIGINT for itself (term::ignore_sigint, called
            // once at interactive startup) so it survives Ctrl-C from its
            // own controlling terminal -- but that disposition is
            // inherited across fork, and POSIX only resets *handled*
            // signals to SIG_DFL across exec; SIG_IGN is explicitly left
            // unchanged. Without this reset, a job attached to this pty
            // would silently inherit "ignore SIGINT" and never respond to
            // a forwarded Ctrl-C, even though the pty's line discipline
            // correctly raises the signal.
            signal(SIGINT, SIG_DFL);
            if setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            let slave_fd = c_open(path.as_ptr(), O_RDWR, 0);
            if slave_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            if ioctl(slave_fd, TIOCSCTTY, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            for target in 0..3 {
                if dup2(slave_fd, target) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if slave_fd > 2 {
                close(slave_fd);
            }
            Ok(())
        });
    }
    cmd.spawn()
}

pub fn get_size(fd: RawFd) -> io::Result<Winsize> {
    let mut ws = Winsize::default();
    if unsafe { ioctl(fd, TIOCGWINSZ, &mut ws as *mut Winsize as usize) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ws)
}

pub fn set_size(fd: RawFd, rows: u16, cols: u16) -> io::Result<()> {
    let ws = Winsize { rows, cols, xpixel: 0, ypixel: 0 };
    if unsafe { ioctl(fd, TIOCSWINSZ, &ws as *const Winsize as usize) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Used by a poll-driven fg loop (repl.rs's drive_fg_job) so it can drain
// whatever output has arrived on a job's pty master without ever
// blocking on it.
pub fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = fcntl(fd, F_GETFL, 0);
        fcntl(fd, F_SETFL, flags | O_NONBLOCK);
    }
}

impl Pty {
    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        set_size(self.master.as_raw_fd(), rows, cols)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    // End-to-end: allocate a real pty, spawn a child attached to its slave,
    // and confirm the child sees a real controlling terminal (`test -t 0`)
    // -- exactly the isatty() check full-screen programs make that fails
    // under a plain inherited-fd or piped spawn.
    #[test]
    fn spawn_attached_gives_child_a_real_tty() {
        let pty = open().expect("open pty");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("if [ -t 0 ] && [ -t 1 ]; then echo TTY_OK; else echo TTY_NO; fi");
        let mut child = spawn_attached(cmd, &pty.slave_path).expect("spawn attached");

        let mut master = pty.master.try_clone().expect("clone master");
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        // The child's output loops back through the pty line discipline
        // (which echoes input, but there is none here) onto the master.
        let start = std::time::Instant::now();
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if out.windows(6).any(|w| w == b"TTY_OK" || w == b"TTY_NO") {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
            if start.elapsed() > Duration::from_secs(5) {
                break;
            }
        }
        let _ = child.wait();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("TTY_OK"), "expected child to see a real tty, got: {:?}", s);
    }

    #[test]
    fn resize_roundtrips_through_get_size() {
        let pty = open().expect("open pty");
        pty.resize(40, 120).expect("set size");
        let ws = get_size(pty.master.as_raw_fd()).expect("get size");
        assert_eq!(ws.rows, 40);
        assert_eq!(ws.cols, 120);
    }

    #[test]
    fn master_write_is_visible_as_child_stdin() {
        let pty = open().expect("open pty");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("read line; echo \"got:$line\"");
        let mut child = spawn_attached(cmd, &pty.slave_path).expect("spawn attached");

        let mut master = pty.master.try_clone().expect("clone master");
        // The pty line discipline echoes this back to us too, in addition
        // to the child reading it -- that's fine, we just scan for "got:".
        master.write_all(b"hello\n").expect("write to master");

        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        let start = std::time::Instant::now();
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if out.windows(4).any(|w| w == b"got:") {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
            if start.elapsed() > Duration::from_secs(5) {
                break;
            }
        }
        let _ = child.wait();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("got:hello"), "expected child to read written input, got: {:?}", s);
    }
}
