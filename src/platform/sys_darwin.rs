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
