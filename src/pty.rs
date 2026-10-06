// Pseudo-terminal allocation and control: `open()` for a master/slave
// pair, `spawn_attached()` to launch a child with the slave side as its
// controlling terminal, and the size helpers around them. Full-screen
// programs (vim, htop, less, ...) probe isatty()/TIOCGWINSZ and misbehave
// without a real pty, and a hidden window's job output has to be captured
// on the master side rather than going straight to bish's own terminal.
//
// The syscalls and their numbers are in `platform`: which `ioctl` request
// means "set the controlling terminal" is a different number on every
// Unix, and what is left here is the part that is bish's -- when to
// attach, what a child has to put back before it execs, and who owns
// which end.
//
// Wired into exec.rs (run_single's background-job spawn path attaches a
// pty when promoted and unredirected) and repl.rs (drives a fg'd job's
// pty master directly). #![allow(dead_code)] stays regardless -- some
// items here (e.g. Pty::resize) are for future callers.
#![allow(dead_code)]

use crate::platform;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

/// A terminal's size. The platform layer's, because `struct winsize` is
/// the kernel's; re-exported here because this is where bish asks.
pub(crate) use crate::platform::Winsize;

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
    let (master, slave_path) = platform::open_pty()?;
    Ok(Pty { master, slave_path })
}

// Spawns `cmd` with its stdin/stdout/stderr replaced by a freshly-opened
// slave fd for `slave_path`, made its controlling terminal. Mirrors what a
// real terminal emulator's shell-spawning does: setsid() to leave bish's
// own process group/session, open the slave (the first tty a session
// leader opens without one yet becomes its controlling terminal on
// Linux, but TIOCSCTTY is called explicitly here to not depend on that
// implicit-acquisition subtlety), then dup2 it onto 0/1/2.
pub fn spawn_attached(mut cmd: Command, slave_path: &str) -> io::Result<Child> {
    attach_on_exec(&mut cmd, slave_path)?;
    cmd.spawn()
}

/// `spawn_attached`'s first half: arranges for the child to take the
/// slave as its fd 0/1/2, without spawning it.
///
/// Split out so a caller can put its own descriptor work after this
/// one's. `pre_exec` closures run in the order they were registered, so
/// registering this first and a command's redirects second gives a
/// command in a pane the pty for the streams it did not redirect and
/// its own targets for the streams it did.
pub fn attach_on_exec(cmd: &mut Command, slave_path: &str) -> io::Result<()> {
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
            platform::default_signal(platform::SIGINT);
            platform::attach_to_pty_slave(&path)
        });
    }
    Ok(())
}

// Makes `slave_path` *this same, already-running* process's own
// controlling terminal -- unlike spawn_attached above, there's no fork
// here at all. Used by session.rs's daemon bootstrap to give a detached
// `bish session` a real local pty of its own (fd 0/1/2 become the slave
// side) so every part of this codebase that already assumes a real
// terminal on those fds (RawGuard's raw mode, get_size/TIOCGWINSZ,
// read_line's own raw reads) keeps working completely unmodified --
// `repl::run` itself needs zero changes to run under a session daemon.
// The daemon keeps `Pty::master` itself (see `Pty::open`'s own doc
// comment on why only the path, not an fd, is kept for the slave side)
// to bridge raw bytes against its attached client's socket; see
// session.rs's SessionBridge for that half.
pub fn attach_self_to_pty(slave_path: &str) -> io::Result<()> {
    let path = CString::new(slave_path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    platform::attach_to_pty_slave(&path)
}

pub fn get_size(fd: RawFd) -> io::Result<Winsize> {
    platform::terminal_size(fd)
}

pub fn set_size(fd: RawFd, rows: u16, cols: u16) -> io::Result<()> {
    platform::set_terminal_size(fd, rows, cols)
}

// Makes `pgrp` the controlling terminal's foreground process group --
// real job control's other half (exec.rs's setpgid isolates a job into
// its own group; this is what actually hands the terminal to it, so a
// signal generated by the terminal driver itself, like Ctrl-C/Ctrl-Z,
// targets that job instead of bish). Works on any tty fd, same as
// get_size/set_size -- used here against fd 0 (bish's own controlling
// terminal), toggled back to bish's own pgid once the foreground job
// exits or stops. Errors (e.g. fd 0 not a tty) are deliberately ignored
// by exec.rs's callers -- losing this is a UX regression (Ctrl-Z would
// misbehave again), not a correctness one.
pub fn tcsetpgrp(fd: RawFd, pgrp: i32) -> io::Result<()> {
    platform::set_foreground_pgrp(fd, pgrp)
}

// Used by a poll-driven fg loop (repl.rs's drive_fg_job) so it can drain
// whatever output has arrived on a job's pty master without ever
// blocking on it.
pub fn set_nonblocking(fd: RawFd) {
    platform::set_nonblocking(fd);
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
        let mut cmd = Command::new(crate::toolpath::require("sh"));
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
        let mut cmd = Command::new(crate::toolpath::require("sh"));
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
