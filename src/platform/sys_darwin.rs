//! What macOS's numbers and structs are: tables, not logic (see this
//! directory's `mod.rs`). Values are from the macOS SDK headers named for
//! their calls; the `sys_tables` test keeps this and `sys_linux.rs`
//! exposing the same names.

// A table entry is a fact about this OS, and some are used only by the
// other OS's implementation (`F_SETFD` is how Darwin sets close-on-exec,
// where Linux passes `O_CLOEXEC` to `pipe2`). Both tables keep them, since
// the pairing test treats a name missing from one as a half-done port,
// which matters; an unused constant doesn't.
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

// `termios` as macOS lays it out: flag words and speeds are
// `unsigned long`, there's no line-discipline byte, and `c_cc` has 20
// entries, not 32. Linux's shape given to `tcsetattr` here would be read
// as nonsense from the second field.
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

// From <sys/termios.h>, the BSD numbers, not Linux's: `ISIG` 0x80 against
// 1, `ICANON` 0x100 against 2, `IEXTEN` 0x400 against 0o100000. `ECHO`
// agrees at 8, the kind of coincidence that makes a half-ported table
// look right.
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

// Signal numbers. BSD renumbered job control: `SIGTSTP` is 18 here where
// Linux has 20 (here `SIGCHLD`), and `SIGSTOP`/`SIGCONT`/`SIGCHLD` are
// shuffled likewise. The ones that agree (HUP, INT, TTIN, TTOU) are
// written out too, so the table needn't be read alongside the other.
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

// `O_NOCTTY` and `O_NONBLOCK` differ here: `O_NONBLOCK` is 4 against
// Linux's 2048, near Linux's `O_NOCTTY`. Setting one and getting the
// other makes a pty hang instead of fail.
pub(crate) const O_RDWR: i32 = 0x0002;
pub(crate) const O_NOCTTY: i32 = 0x0002_0000;
pub(crate) const O_NONBLOCK: i32 = 0x0004;
pub(crate) const F_GETFL: i32 = 3;
pub(crate) const F_SETFL: i32 = 4;

pub(crate) const SIG_DFL: usize = 0;

// `getrlimit`/`setrlimit` resources and `sysconf`'s clock-tick name.
// `SC_CLK_TCK` is 3 here where Linux has 2, and the wrong one returns a
// different property: CPU times divided by the wrong number.
pub(crate) const RLIMIT_STACK: i32 = 3;
// "No limit" keeps the top bit clear here, `(1 << 63) - 1`, not Linux's
// all-ones, so a Linux value read back would print as nonsense and
// setting it would be refused.
pub(crate) const RLIM_INFINITY: u64 = (1 << 63) - 1;
pub(crate) const SC_CLK_TCK: i32 = 3;

/// Which `RLIMIT_*` a `ulimit` flag asks about, or `None` where this OS
/// has none. Darwin agrees with Linux up to `CORE`, then `MEMLOCK`,
/// `NPROC` and `NOFILE` are 6, 7 and 8 against Linux's 8, 6 and 7. The six
/// Linux-only limits (file locks, pending signals, message queues, nice,
/// the two real-time ones) don't exist here, so `ulimit -a` omits them
/// and refuses to set them, as bash here does. `-v` is `RLIMIT_RSS`,
/// which is what Darwin's `RLIMIT_AS` is defined as.
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

// `fcntl(2)` commands. `F_DUPFD_CLOEXEC` is 67 here against Linux's
// 1030; see that table for what the wrong one costs.
pub(crate) const F_DUPFD_CLOEXEC: i32 = 67;
pub(crate) const F_GETFD: i32 = 1;

// `struct sigaction` as macOS lays it out: a handler, a 32-bit signal
// set and the flags, sixteen bytes against glibc's 152, no restorer. See
// the Linux table for what crossing them would do.
#[repr(C)]
#[derive(Default)]
pub(crate) struct SigAction {
    pub(crate) handler: usize,
    pub(crate) mask: u32,
    pub(crate) flags: i32,
}

// BSD's numbers: `SIGSTOP` 17 where Linux has 19, `SIGCONT` 19 where it
// has 18. The two are each other's opposite, so crossing them would stop
// a job that was asked to continue.
pub(crate) const SIGCONT: i32 = 19;
pub(crate) const SIGSTOP: i32 = 17;

/// The highest signal number this OS has. No real-time range, so the
/// named signals are all there is.
pub(crate) const HIGHEST_SIGNAL: i32 = 31;

/// macOS has no real-time signals at all.
pub(crate) const REALTIME_SIGNALS: Option<(i32, i32)> = None;

/// Name to number for every signal a script can trap here.
///
/// BSD's numbering, which differs from Linux's in eleven of these:
/// `BUS` is 10 against 7, `USR1`/`USR2` are 30 and 31 against 10 and 12,
/// `SYS` is 12 against 31, `URG` 16 against 23, and the job-control four
/// are shuffled. `STKFLT`, `PWR` and Linux's `IO`-as-29 do not exist
/// here; `EMT` and `INFO` do and Linux has no equivalent. KILL and STOP
/// are absent for the same reason as in the other table.
pub(crate) const SIGNAL_NAMES: &[(&str, i32)] = &[
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("EMT", 7),
    ("FPE", 8),
    ("BUS", 10),
    ("SEGV", 11),
    ("SYS", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("URG", 16),
    ("TSTP", 18),
    ("CONT", 19),
    ("CHLD", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("IO", 23),
    ("XCPU", 24),
    ("XFSZ", 25),
    ("VTALRM", 26),
    ("PROF", 27),
    ("WINCH", 28),
    ("INFO", 29),
    ("USR1", 30),
    ("USR2", 31),
];
