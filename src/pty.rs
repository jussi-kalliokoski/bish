// Pseudo-terminals: `open()` for a master and slave, `spawn_attached()`
// to start a child with the slave as its controlling terminal, and the
// size helpers. Full-screen programs (vim, htop, less) check
// isatty()/TIOCGWINSZ and misbehave without a real pty, and a hidden
// window's job output must be captured on the master. The syscalls are
// in `platform`; here is when to attach, what a child restores before
// exec, and who owns which end.
#![allow(dead_code)]

use crate::platform;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

/// A terminal's size, from the platform layer, re-exported where bish
/// asks.
pub(crate) use crate::platform::Winsize;

// A master/slave pty pair. `master` is the end bish reads and writes;
// `slave_path` (like "/dev/pts/7") is opened in the child by
// `spawn_attached`, never held here, which would leak a slave fd into
// every later child.
pub struct Pty {
    pub master: File,
    pub slave_path: String,
    /// What the OS needs held open for the master to answer before a child
    /// attaches (`platform::pty_keepalive`), dropped with the `Pty`, when the
    /// child has its own.
    _keepalive: Option<File>,
}

pub fn open() -> io::Result<Pty> {
    let (master, slave_path) = platform::open_pty()?;
    let _keepalive = platform::pty_keepalive(&slave_path)?;
    Ok(Pty { master, slave_path, _keepalive })
}

// Spawns `cmd` with stdin, stdout and stderr on a newly opened slave,
// made its controlling terminal, as a terminal emulator starts a shell:
// `setsid()`, open the slave with an explicit `TIOCSCTTY` (rather than
// rely on a session leader's first tty becoming it, as on Linux), dup2 it
// onto 0-2.
pub fn spawn_attached(mut cmd: Command, slave_path: &str) -> io::Result<Child> {
    attach_on_exec(&mut cmd, slave_path)?;
    cmd.spawn()
}

/// `spawn_attached`'s first half: arranges for the child to take the
/// slave as fds 0-2, without spawning. Split so a caller's descriptor
/// work can follow: `pre_exec` closures run in registration order, so
/// registering this before a command's redirects gives a pane's command
/// the pty for the streams it didn't redirect.
pub fn attach_on_exec(cmd: &mut Command, slave_path: &str) -> io::Result<()> {
    let path = CString::new(slave_path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // A pty means nothing without fork and exec; there the path is dropped
    // and the spawn fails on its own.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(move || {
            // bish ignores SIGINT itself (`term::ignore_sigint`), inherited
            // across fork and kept across exec (POSIX resets only handled
            // signals), so without this a job on this pty would ignore a
            // forwarded Ctrl-C.
            platform::default_signal(platform::SIGINT);
            platform::attach_to_pty_slave(&path)
        });
    }
    #[cfg(not(unix))]
    {
        let _ = (&mut *cmd, path);
    }
    Ok(())
}

// Makes `slave_path` this running process's controlling terminal, with
// no fork: session.rs's daemon gives a detached `bish session` a local
// pty (fds 0-2 on the slave), so everything assuming a real terminal
// there (raw mode, `TIOCGWINSZ`, `read_line`) works unchanged. The daemon
// keeps the master to bridge bytes with its client's socket
// (`SessionBridge`).
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

// Makes `pgrp` the controlling terminal's foreground group: with
// exec.rs's setpgid isolating a job, this sends Ctrl-C and Ctrl-Z to the
// job instead of bish. Used on fd 0 and switched back to bish's group
// when the job exits or stops. exec.rs ignores errors (fd 0 not a tty):
// losing this only misroutes Ctrl-Z.
pub fn tcsetpgrp(fd: RawFd, pgrp: i32) -> io::Result<()> {
    platform::set_foreground_pgrp(fd, pgrp)
}

// For a poll-driven fg loop (`drive_fg_job`), draining a job's pty
// without blocking.
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

    // A real pty and a child attached to its slave, which must see a
    // controlling terminal (`test -t 0`): the isatty() check full-screen
    // programs make, which fails with inherited or piped fds.
    #[test]
    fn spawn_attached_gives_child_a_real_tty() {
        let pty = open().expect("open pty");
        let mut cmd = Command::new(crate::toolpath::require("sh"));
        cmd.arg("-c").arg("if [ -t 0 ] && [ -t 1 ]; then echo TTY_OK; else echo TTY_NO; fi");
        let mut child = spawn_attached(cmd, &pty.slave_path).expect("spawn attached");

        let mut master = pty.master.try_clone().expect("clone master");
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        // The child's output comes back through the line discipline to the
        // master.
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
        // The line discipline echoes this back too; the scan is for "got:".
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
