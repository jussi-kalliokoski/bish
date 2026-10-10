//! What Linux's numbers and structs are: tables, not logic (see this
//! directory's `mod.rs`).

// A table entry is a fact about this OS, and some are used only by the
// other OS's implementation (`F_SETFD` is how Darwin sets close-on-exec,
// where Linux passes `O_CLOEXEC` to `pipe2`). Both tables keep them, since
// the pairing test treats a name missing from one as a half-done port,
// which matters; an unused constant doesn't.
#![allow(dead_code)]

// mmap(2)/mprotect(2). Protection bits agree across bish's Unixes;
// mapping flags don't: anonymous is 0x20 here and 0x1000 on Darwin, and
// only the wrong pages would show it.
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

// Indices into `c_cc`, the likeliest to be got wrong: 6 and 5 here, 16
// and 17 on Darwin. At the wrong index, raw mode would set two unrelated
// control characters and leave reads alone, a terminal that won't
// answer.
pub(crate) const VMIN: usize = 6;
pub(crate) const VTIME: usize = 5;

// `tcsetattr`: apply this now rather than after the output drains.
pub(crate) const TCSANOW: i32 = 0;

// Signal numbers. Four agree with Darwin's and one doesn't: `SIGTSTP`
// is 20 here and 18 there, where 20 is `SIGCHLD`, so stopping the shell
// with this number would reap a child instead.
pub(crate) const SIGHUP: i32 = 1;
pub(crate) const SIGINT: i32 = 2;
pub(crate) const SIGTSTP: i32 = 20;
pub(crate) const SIGTTIN: i32 = 21;
pub(crate) const SIGTTOU: i32 = 22;

// `signal(2)`'s "ignore this" handler, as a plain number because that is
// what the C declaration takes.
pub(crate) const SIG_IGN: usize = 1;

// `ioctl(2)` terminal requests. Linux's are small sequential numbers;
// Darwin packs direction and argument size into them, so none of these
// four agree, and the wrong one fails with ENOTTY at best and asks the
// terminal for something else at worst.
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

// `getrlimit`/`setrlimit` resources and `sysconf`'s clock-tick name.
// Darwin renumbers most and lacks six, so `ulimit`'s flags map per OS
// (`rlimit_number`).
pub(crate) const RLIMIT_STACK: i32 = 3;
// "No limit": every bit of `rlim_t` set.
pub(crate) const RLIM_INFINITY: u64 = u64::MAX;
pub(crate) const SC_CLK_TCK: i32 = 2;

/// Which `RLIMIT_*` a `ulimit` flag asks about, or `None` where this OS
/// has none. Linux's numbers; the six after `NOFILE` are Linux-only,
/// which is why `ulimit -a` lists more here than on a Mac, as bash's
/// does.
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

// The resize signal: 28 on both, unlike job control's. Written in both
// tables, so one file answers which number it is here.
pub(crate) const SIGWINCH: i32 = 28;

// `fcntl(2)` commands beyond `F_SETFD`. `F_DUPFD_CLOEXEC` is 1030 here
// and 67 on Darwin, and the wrong one fails quietly: the duplicate isn't
// close-on-exec and leaks into every child.
pub(crate) const F_DUPFD_CLOEXEC: i32 = 1030;
pub(crate) const F_GETFD: i32 = 1;

// `struct sigaction` as glibc lays it out: the handler, a 128-byte signal
// set, the flags, and a restorer glibc fills in. Darwin's is 16 bytes
// with a 32-bit set and no restorer, so this shape there would be
// nonsense past the handler, with `sigaction` setting a mask from stack
// garbage.
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

/// Name to number for every signal a script can trap. KILL and STOP are
/// absent: neither can be caught or ignored, and bash won't let `trap`
/// name them. Linux's numbers; over a third differ from Darwin's, hence a
/// table per OS.
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
