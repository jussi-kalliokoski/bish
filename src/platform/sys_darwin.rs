//! What macOS's own numbers and structs are. Tables, not logic -- see
//! this directory's `mod.rs` for why they are collected here.
//!
//! Values are from the macOS SDK headers of the same name as the system
//! call they belong to, and the `sys_tables` test next door keeps this
//! file and `sys_linux.rs` exposing the same names, so a port cannot
//! half-happen.

// A table entry is a fact about this OS, and some facts are only ever
// consumed by the *other* OS's implementation -- `F_SETFD` is how Darwin
// sets close-on-exec where Linux passes `O_CLOEXEC` to `pipe2`, and each
// is dead on the side that does not need it. They are both here because
// the pairing test next door says a name missing from one table is a
// port that half-happened, which is the error worth catching; an unused
// constant is not.
#![allow(dead_code)]

// mmap(2)/mprotect(2), from <sys/mman.h>. `MAP_ANON` is the name Darwin
// spells it; `MAP_ANONYMOUS` is a Linux alias for the same idea, with a
// different value.
pub(crate) const PROT_NONE: i32 = 0x00;
pub(crate) const PROT_READ: i32 = 0x01;
pub(crate) const PROT_WRITE: i32 = 0x02;
pub(crate) const MAP_PRIVATE: i32 = 0x0002;
pub(crate) const MAP_ANON: i32 = 0x1000;

// open(2)/fcntl(2). `O_CLOEXEC` is the one that differs; `F_SETFD` and
// `FD_CLOEXEC` are the same two numbers everywhere and are here so that
// a reader finds all three in one place.
pub(crate) const O_CLOEXEC: i32 = 0x100_0000;
pub(crate) const F_SETFD: i32 = 2;
pub(crate) const FD_CLOEXEC: i32 = 1;

// `termios`, as macOS lays it out: the flag words and the speeds are
// `unsigned long` here, not 32-bit, there is no line-discipline byte at
// all, and `c_cc` holds 20 entries rather than 32. A struct of Linux's
// shape handed to `tcsetattr` on this OS would be read as nonsense from
// the second field onwards.
pub(crate) type Flag = u64;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Termios {
    pub(crate) c_iflag: Flag,
    pub(crate) c_oflag: Flag,
    pub(crate) c_cflag: Flag,
    pub(crate) c_lflag: Flag,
    pub(crate) c_cc: [u8; 20],
    pub(crate) c_ispeed: Flag,
    pub(crate) c_ospeed: Flag,
}

// From <sys/termios.h>. These are the BSD numbers, and they are not
// Linux's: `ISIG` is 0x80 against 1, `ICANON` 0x100 against 2, `IEXTEN`
// 0x400 against 0o100000. `ECHO` happens to agree at 8, which is exactly
// the sort of coincidence that makes a half-ported table look right.
pub(crate) const IGNBRK: Flag = 0x0000_0001;
pub(crate) const BRKINT: Flag = 0x0000_0002;
pub(crate) const PARMRK: Flag = 0x0000_0008;
pub(crate) const ISTRIP: Flag = 0x0000_0020;
pub(crate) const INLCR: Flag = 0x0000_0040;
pub(crate) const IGNCR: Flag = 0x0000_0080;
pub(crate) const ICRNL: Flag = 0x0000_0100;
pub(crate) const IXON: Flag = 0x0000_0200;
pub(crate) const OPOST: Flag = 0x0000_0001;
pub(crate) const CSIZE: Flag = 0x0000_0300;
pub(crate) const CS8: Flag = 0x0000_0300;
pub(crate) const ISIG: Flag = 0x0000_0080;
pub(crate) const ICANON: Flag = 0x0000_0100;
pub(crate) const ECHO: Flag = 0x0000_0008;
pub(crate) const IEXTEN: Flag = 0x0000_0400;

// Indices into `c_cc`: 16 and 17 here against Linux's 6 and 5. See the
// Linux table for what writing them at the wrong index looks like.
pub(crate) const VMIN: usize = 16;
pub(crate) const VTIME: usize = 17;

pub(crate) const TCSANOW: i32 = 0;

// Signal numbers. BSD renumbered the job-control signals: `SIGTSTP` is
// 18 here where Linux has 20 (which is `SIGCHLD` on this OS), and
// `SIGSTOP`/`SIGCONT`/`SIGCHLD` are shuffled the same way. The three
// bish needs that do agree -- HUP, INT, TTIN, TTOU -- are written out
// here too rather than shared, because a table that holds only the
// differences is a table that has to be read twice.
pub(crate) const SIGHUP: i32 = 1;
pub(crate) const SIGINT: i32 = 2;
pub(crate) const SIGTSTP: i32 = 18;
pub(crate) const SIGTTIN: i32 = 21;
pub(crate) const SIGTTOU: i32 = 22;

pub(crate) const SIG_IGN: usize = 1;

// `ioctl(2)` request numbers for a terminal, from <sys/ttycom.h>, where
// they are built by `_IOR`/`_IOW`/`_IO` macros that pack the direction
// and the argument's size in alongside the number: `TIOCGWINSZ` is
// `_IOR('t', 104, struct winsize)`, which is 0x40087468, where Linux has
// 0x5413. Nothing about using the wrong one is a build error.
pub(crate) const TIOCGWINSZ: u64 = 0x4008_7468;
pub(crate) const TIOCSWINSZ: u64 = 0x8008_7467;
pub(crate) const TIOCSCTTY: u64 = 0x2000_7461;
pub(crate) const TIOCSPGRP: u64 = 0x8004_7476;

// `O_NOCTTY` and `O_NONBLOCK` are both different numbers here --
// `O_NONBLOCK` is 4 against Linux's 2048, which is `O_NOCTTY`'s
// neighbourhood there. Setting one and getting the other is the kind of
// thing that makes a pty hang instead of fail.
pub(crate) const O_RDWR: i32 = 0x0002;
pub(crate) const O_NOCTTY: i32 = 0x0002_0000;
pub(crate) const O_NONBLOCK: i32 = 0x0004;
pub(crate) const F_GETFL: i32 = 3;
pub(crate) const F_SETFL: i32 = 4;

pub(crate) const SIG_DFL: usize = 0;

// `getrlimit`/`setrlimit` resources, and `sysconf`'s clock-tick name.
//
// `SC_CLK_TCK` is 3 here where Linux has 2, and asking for the wrong one
// returns a different system property entirely -- CPU times quietly
// divided by the wrong number.
pub(crate) const RLIMIT_STACK: i32 = 3;
pub(crate) const SC_CLK_TCK: i32 = 3;

/// Which `RLIMIT_*` a `ulimit` flag asks about, or `None` where this OS
/// has no such limit.
///
/// Darwin agrees with Linux up to `CORE` and then diverges: `MEMLOCK`,
/// `NPROC` and `NOFILE` are 6, 7 and 8 against Linux's 8, 6 and 7, so
/// three flags would each have asked about one of the others. The six
/// Linux-only limits -- file locks, pending signals, message queues,
/// nice, and the two real-time ones -- do not exist here at all, and
/// `None` is how `ulimit` comes to leave them out of `-a` and refuse to
/// set them, which is what bash on this OS does too.
///
/// `-v` is `RLIMIT_RSS`, because that is what Darwin's `RLIMIT_AS` is
/// defined as: the same number, under two names.
pub(crate) fn rlimit_number(flag: char) -> Option<i32> {
    Some(match flag {
        't' => 0,            // CPU
        'f' => 1,            // FSIZE
        'd' => 2,            // DATA
        's' => RLIMIT_STACK, // STACK
        'c' => 4,            // CORE
        'm' => 5,            // RSS
        'v' => 5,            // AS, which is RSS here
        'l' => 6,            // MEMLOCK
        'u' => 7,            // NPROC
        'n' => 8,            // NOFILE
        _ => return None,
    })
}

// `poll(2)`'s count argument: `nfds_t` is `unsigned int` here where Linux
// has `unsigned long`.
pub(crate) type NFds = u32;

pub(crate) const SIGWINCH: i32 = 28;

// `fcntl(2)` commands. `F_DUPFD_CLOEXEC` is 67 here against Linux's 1030
// -- see that table for what the wrong one costs.
pub(crate) const F_DUPFD_CLOEXEC: i32 = 67;
pub(crate) const F_GETFD: i32 = 1;
