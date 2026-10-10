//! Which of bish's numbers survive on wasm (WASI preview 1), which has no
//! signals, job control or terminal ioctls, so no kernel numbers to get
//! wrong. What's left are the values bish names (the signal table `trap`,
//! `kill` and `$?` use), kept as inert data so those builtins compile and
//! behave the same, though nothing delivers a signal. The numbers are
//! Linux's: a table nobody fires has to say something, and matching a
//! real OS keeps `kill -l` and `trap -l` familiar.
#![allow(dead_code)]

// The job-control and terminal signals `mod.rs` re-exports by name.
pub(crate) const SIGHUP: i32 = 1;
pub(crate) const SIGINT: i32 = 2;
pub(crate) const SIGTSTP: i32 = 20;
pub(crate) const SIGTTIN: i32 = 21;
pub(crate) const SIGTTOU: i32 = 22;
pub(crate) const SIGWINCH: i32 = 28;
pub(crate) const SIGCONT: i32 = 18;
pub(crate) const SIGSTOP: i32 = 19;

/// The highest signal number bish's `trap`/`kill` will accept, matching
/// the table below.
pub(crate) const HIGHEST_SIGNAL: i32 = 64;

/// The real-time range, named `RTMIN+n`. Inert here like the rest.
pub(crate) const REALTIME_SIGNALS: Option<(i32, i32)> = Some((34, 64));

/// Name to number for every signal a script can name. KILL and STOP are
/// absent, as everywhere: `trap` can't name them.
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
