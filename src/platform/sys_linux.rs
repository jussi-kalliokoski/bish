//! What Linux's own numbers and structs are. Tables, not logic -- see
//! this directory's `mod.rs` for why they are collected here.

// A table entry is a fact about this OS, and some facts are only ever
// consumed by the *other* OS's implementation -- `F_SETFD` is how Darwin
// sets close-on-exec where Linux passes `O_CLOEXEC` to `pipe2`, and each
// is dead on the side that does not need it. They are both here because
// the pairing test next door says a name missing from one table is a
// port that half-happened, which is the error worth catching; an unused
// constant is not.
#![allow(dead_code)]

// mmap(2)/mprotect(2). The protection bits agree across every Unix bish
// targets; the mapping flags do not -- an anonymous mapping is 0x20
// here and 0x1000 on Darwin, and nothing but the wrong pages would say
// so.
pub(crate) const PROT_NONE: i32 = 0;
pub(crate) const PROT_READ: i32 = 1;
pub(crate) const PROT_WRITE: i32 = 2;
pub(crate) const MAP_PRIVATE: i32 = 2;
pub(crate) const MAP_ANON: i32 = 0x20;

// open(2)/fcntl(2). `O_CLOEXEC` is the one that differs; `F_SETFD` and
// `FD_CLOEXEC` are the same two numbers everywhere and are here so that
// a reader finds all three in one place.
pub(crate) const O_CLOEXEC: i32 = 0o2000000;
pub(crate) const F_SETFD: i32 = 2;
pub(crate) const FD_CLOEXEC: i32 = 1;

// `termios`, as glibc lays it out on Linux: four 32-bit flag words, a
// line discipline byte that only Linux has, 32 control characters, and
// the two speeds. `repr(C)` then reproduces the padding a C compiler
// would insert, so it can be handed straight to tcgetattr/tcsetattr.
pub(crate) type Flag = u32;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Termios {
    pub(crate) c_iflag: Flag,
    pub(crate) c_oflag: Flag,
    pub(crate) c_cflag: Flag,
    pub(crate) c_lflag: Flag,
    pub(crate) c_line: u8,
    pub(crate) c_cc: [u8; 32],
    pub(crate) c_ispeed: Flag,
    pub(crate) c_ospeed: Flag,
}

// Input, output, control and local flags. Every one of these is a
// different number on Darwin, and nothing but a terminal behaving oddly
// would say so.
pub(crate) const IGNBRK: Flag = 0o0000001;
pub(crate) const BRKINT: Flag = 0o0000002;
pub(crate) const PARMRK: Flag = 0o0000010;
pub(crate) const ISTRIP: Flag = 0o0000040;
pub(crate) const INLCR: Flag = 0o0000100;
pub(crate) const IGNCR: Flag = 0o0000200;
pub(crate) const ICRNL: Flag = 0o0000400;
pub(crate) const IXON: Flag = 0o0002000;
pub(crate) const OPOST: Flag = 0o0000001;
pub(crate) const CSIZE: Flag = 0o0000060;
pub(crate) const CS8: Flag = 0o0000060;
pub(crate) const ISIG: Flag = 0o0000001;
pub(crate) const ICANON: Flag = 0o0000002;
pub(crate) const ECHO: Flag = 0o0000010;
pub(crate) const IEXTEN: Flag = 0o0100000;

// Indices into `c_cc`, and the ones most likely to be got wrong: 6 and 5
// here, 16 and 17 on Darwin. A raw mode that wrote them at the wrong
// index would set two unrelated control characters and leave the read
// behaviour alone, which looks like a terminal that will not answer.
pub(crate) const VMIN: usize = 6;
pub(crate) const VTIME: usize = 5;

// `tcsetattr`: apply this now rather than after the output drains.
pub(crate) const TCSANOW: i32 = 0;

// Signal numbers. Four of these agree with Darwin's and one does not:
// `SIGTSTP` is 20 here and 18 there, where 20 is `SIGCHLD` -- so a
// suspend-self written with this number would reap a child instead of
// stopping the shell.
pub(crate) const SIGHUP: i32 = 1;
pub(crate) const SIGINT: i32 = 2;
pub(crate) const SIGTSTP: i32 = 20;
pub(crate) const SIGTTIN: i32 = 21;
pub(crate) const SIGTTOU: i32 = 22;

// `signal(2)`'s "ignore this" handler, as a plain number because that is
// what the C declaration takes.
pub(crate) const SIG_IGN: usize = 1;

// `ioctl(2)` request numbers for a terminal. Linux's are small
// sequential constants; Darwin encodes the direction and the argument
// size into them, so not one of these four agrees -- and an `ioctl` with
// the wrong number fails with ENOTTY at best and asks the terminal for
// something else entirely at worst.
pub(crate) const TIOCGWINSZ: u64 = 0x5413;
pub(crate) const TIOCSWINSZ: u64 = 0x5414;
pub(crate) const TIOCSCTTY: u64 = 0x540E;
pub(crate) const TIOCSPGRP: u64 = 0x5410;

// The rest of the `open`/`fcntl` numbers, beside `O_CLOEXEC` above.
// `O_RDWR` and the two `fcntl` commands are the same everywhere;
// `O_NOCTTY` and `O_NONBLOCK` are not.
pub(crate) const O_RDWR: i32 = 0o2;
pub(crate) const O_NOCTTY: i32 = 0o400;
pub(crate) const O_NONBLOCK: i32 = 0o4000;
pub(crate) const F_GETFL: i32 = 3;
pub(crate) const F_SETFL: i32 = 4;

// `signal(2)`'s "do whatever you would have done" handler, the other
// half of `SIG_IGN`.
pub(crate) const SIG_DFL: usize = 0;

// `getrlimit`/`setrlimit` resources, and `sysconf`'s clock-tick name.
//
// Darwin renumbers most of these and simply does not have six of them,
// so the mapping from `ulimit`'s own flags to resource numbers is a
// per-OS fact rather than a shared table -- see `rlimit_number`.
pub(crate) const RLIMIT_STACK: i32 = 3;
pub(crate) const SC_CLK_TCK: i32 = 2;

/// Which `RLIMIT_*` a `ulimit` flag asks about, or `None` where this OS
/// has no such limit.
///
/// Linux's numbers, in its own order: the six after `NOFILE` are
/// Linux-only, which is why `ulimit -a` lists more here than on a Mac --
/// as bash's own does.
pub(crate) fn rlimit_number(flag: char) -> Option<i32> {
    Some(match flag {
        't' => 0,            // CPU
        'f' => 1,            // FSIZE
        'd' => 2,            // DATA
        's' => RLIMIT_STACK, // STACK
        'c' => 4,            // CORE
        'm' => 5,            // RSS
        'u' => 6,            // NPROC
        'n' => 7,            // NOFILE
        'l' => 8,            // MEMLOCK
        'v' => 9,            // AS
        'x' => 10,           // LOCKS
        'i' => 11,           // SIGPENDING
        'q' => 12,           // MSGQUEUE
        'e' => 13,           // NICE
        'r' => 14,           // RTPRIO
        'R' => 15,           // RTTIME
        _ => return None,
    })
}

// `poll(2)`'s count argument: `nfds_t` is `unsigned long` here and
// `unsigned int` on Darwin. Same call, different width.
pub(crate) type NFds = u64;

// Terminal-resize signal. 28 on both, unlike the job-control numbers --
// written out in both tables rather than shared, so one file answers
// "which number is this signal here".
pub(crate) const SIGWINCH: i32 = 28;

// `fcntl(2)` commands beyond `F_SETFD` above. `F_DUPFD_CLOEXEC` is 1030
// here and 67 on Darwin -- one of the quieter wrong numbers available: a
// duplicate that is not close-on-exec leaks into every child, and the
// call does not fail.
pub(crate) const F_DUPFD_CLOEXEC: i32 = 1030;
pub(crate) const F_GETFD: i32 = 1;

// `struct sigaction` as glibc lays it out on Linux: the handler, then a
// 128-byte signal set, the flags, and a restorer glibc fills in itself.
// Darwin's is 16 bytes with a 32-bit set and no restorer at all, so a
// struct of this shape passed there would be read as nonsense past the
// handler -- and `sigaction` would be setting a mask out of whatever
// happened to be on the stack.
#[repr(C)]
#[derive(Default)]
pub(crate) struct SigAction {
    pub(crate) handler: usize,
    pub(crate) mask: [u64; 16],
    pub(crate) flags: i32,
    pub(crate) restorer: usize,
}

// Job control's own signals. BSD renumbered these: `SIGSTOP` is 19 here
// and 17 there, `SIGCONT` 18 against 19.
pub(crate) const SIGCONT: i32 = 18;
pub(crate) const SIGSTOP: i32 = 19;

/// The highest signal number this OS has, which is what `trap` and
/// `kill` accept up to. Linux's real-time range tops out at 64.
pub(crate) const HIGHEST_SIGNAL: i32 = 64;

/// Linux's real-time signals, which have no names of their own and are
/// referred to as `RTMIN+n`.
pub(crate) const REALTIME_SIGNALS: Option<(i32, i32)> = Some((34, 64));

/// Name to number for every signal a script can trap here.
///
/// KILL and STOP are deliberately absent: neither can be caught or
/// ignored, and bash refuses to let `trap` name them. The numbers are
/// Linux's -- more than a third of them differ from Darwin's, which is
/// why this is a table per OS rather than one shared list.
pub(crate) const SIGNAL_NAMES: &[(&str, i32)] = &[
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("STKFLT", 16),
    ("CHLD", 17),
    ("CONT", 18),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("URG", 23),
    ("XCPU", 24),
    ("XFSZ", 25),
    ("VTALRM", 26),
    ("PROF", 27),
    ("WINCH", 28),
    ("IO", 29),
    ("PWR", 30),
    ("SYS", 31),
];
