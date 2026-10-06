//! How a POSIX system does what bish needs.
//!
//! Written once for every Unix: what differs between them is the
//! numbers and layouts, which live in `sys_linux.rs`/`sys_darwin.rs`
//! and reach this file as `sys::*`. A target that is not a Unix at all
//! replaces this file rather than editing it -- see `mod.rs`.

use super::sys;

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

/// `guard + usable` bytes of fresh address space, the lowest `guard`
/// bytes of it unreadable.
///
/// For a stack of bish's own making: stacks grow downward, so the
/// unreadable end is the one an overrun reaches, and it faults there
/// rather than silently corrupting whatever was mapped next. Address
/// space rather than memory -- pages are faulted in as they are
/// touched, so an untouched megabyte costs a page-table entry and
/// nothing else.
pub(crate) fn map_guarded_stack(guard: usize, usable: usize) -> std::io::Result<*mut u8> {
    // POSIX says `(void *) -1`, which every Unix agrees on, so this one
    // is not a per-OS fact.
    const MAP_FAILED: isize = -1;

    let total = guard + usable;
    let base = unsafe { mmap(std::ptr::null_mut(), total, sys::PROT_READ | sys::PROT_WRITE, sys::MAP_PRIVATE | sys::MAP_ANON, -1, 0) };
    if base as isize == MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { mprotect(base, guard, sys::PROT_NONE) } != 0 {
        let failure = std::io::Error::last_os_error();
        unsafe { unmap(base, total) };
        return Err(failure);
    }
    Ok(base)
}

/// Hands `len` bytes mapped by `map_guarded_stack` back to the OS.
///
/// # Safety
///
/// `base` and `len` must be a mapping this module made, and nothing may
/// still be running on it.
pub(crate) unsafe fn unmap(base: *mut u8, len: usize) {
    unsafe { munmap(base, len) };
}

/// A file with no name: created in the temp directory and unlinked
/// before anything can look it up, so the descriptor is the only way to
/// it and nothing is left behind if bish dies holding it.
///
/// `tag` only makes the (very short-lived) name recognisable to anyone
/// watching the directory.
///
/// Here rather than in `darwin.rs`, which is its only caller, for one
/// reason: every line of it is POSIX, so on this side of the port it is
/// compiled and tested on Linux too. Code macOS will run that cannot be
/// run until there is a Mac to hand is the thing most worth avoiding in
/// this directory.
#[allow(dead_code)]
pub(crate) fn unlinked_temp_file(tag: &str) -> Option<std::fs::File> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // `create_new`, so a name that somehow exists already is never
    // opened -- and then a few tries, since a collision is the only way
    // that happens and a different name settles it.
    for _ in 0..8 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("bish-{tag}-{}-{n}", std::process::id()));
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path) else { continue };
        // From here the name is gone and the descriptor is all there is.
        // A failed unlink would leave a file behind under a name nothing
        // reuses, so the caller is told there is no anonymous file to be
        // had rather than being handed one that litters.
        if std::fs::remove_file(&path).is_err() {
            return None;
        }
        return Some(file);
    }
    None
}

/// What a directory looked like at one moment: every name in it, with
/// enough of each entry's identity to tell "changed" from "same file,
/// looked at twice".
///
/// For an OS whose file-change notifications say *that* a directory
/// changed without saying *which* name did -- kqueue, which is macOS's
/// only answer here. Linux's inotify names the entry itself and needs
/// none of this.
///
/// Here rather than in `darwin.rs` for the same reason as
/// `unlinked_temp_file`: it is all `std::fs`, so it is compiled and
/// tested on Linux, where there is a machine to run it on. Dead on
/// Linux at run time for exactly that reason.
#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct DirSnapshot {
    entries: std::collections::HashMap<std::ffi::OsString, Stamp>,
}

/// The parts of an entry worth comparing. Inode included, and
/// deliberately: the case the whole file-watching design exists for is a
/// formatter or a branch switch replacing a file through a rename, which
/// changes the inode and need not change the size or even the
/// modification time.
#[derive(PartialEq, Eq)]
struct Stamp {
    inode: u64,
    size: u64,
    modified: Option<std::time::SystemTime>,
}

/// Whether a name is still there.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryChange {
    /// It appeared, or it is not the same file it was, or it was written
    /// to. All one answer, because every caller's next move is to look.
    Touched,
    /// It is no longer in the directory.
    Gone,
}

#[allow(dead_code)]
impl DirSnapshot {
    /// An unreadable or missing directory snapshots as empty, which
    /// makes its disappearance read as every name in it going -- the
    /// truth, as far as anyone watching a name in it is concerned.
    pub(crate) fn of(dir: &std::path::Path) -> DirSnapshot {
        use std::os::unix::fs::MetadataExt;
        let mut entries = std::collections::HashMap::new();
        let Ok(listing) = std::fs::read_dir(dir) else { return DirSnapshot::default() };
        for entry in listing.flatten() {
            // `symlink_metadata`, so a symlink is the thing being
            // watched rather than whatever it points at: replacing where
            // it points is a change to this directory, and a dangling
            // one still has an entry.
            let Ok(meta) = entry.path().symlink_metadata() else { continue };
            entries.insert(entry.file_name(), Stamp { inode: meta.ino(), size: meta.size(), modified: meta.modified().ok() });
        }
        DirSnapshot { entries }
    }

    /// Every name that is not as it was in `before`.
    ///
    /// Names neither snapshot has, and names both have identically, are
    /// not mentioned -- which is what keeps a sibling's business out of
    /// a watched file's events.
    pub(crate) fn changes_since(&self, before: &DirSnapshot) -> Vec<(std::ffi::OsString, EntryChange)> {
        let mut out = Vec::new();
        for (name, stamp) in &self.entries {
            match before.entries.get(name) {
                Some(was) if was == stamp => {}
                _ => out.push((name.clone(), EntryChange::Touched)),
            }
        }
        for name in before.entries.keys() {
            if !self.entries.contains_key(name) {
                out.push((name.clone(), EntryChange::Gone));
            }
        }
        out
    }
}

unsafe extern "C" {
    fn access(path: *const std::ffi::c_char, mode: i32) -> i32;
    fn dup(fd: i32) -> i32;
    fn mkdir(path: *const i8, mode: u32) -> i32;
    fn sigaction(signal: i32, action: *const sys::SigAction, previous: *mut sys::SigAction) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn getpgrp() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn getppid() -> i32;
    fn getuid() -> u32;
    fn ttyname_r(fd: i32, buf: *mut u8, len: usize) -> i32;
    fn gethostname(buf: *mut u8, len: usize) -> i32;
    fn poll(fds: *mut PollFd, count: sys::NFds, timeout_ms: i32) -> i32;
    fn pipe(fds: *mut i32) -> i32;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn getrlimit(resource: i32, limit: *mut ResourceLimit) -> i32;
    fn setrlimit(resource: i32, limit: *const ResourceLimit) -> i32;
    fn sysconf(name: i32) -> i64;
    fn times(buf: *mut Tms) -> i64;
    fn umask(mask: u32) -> u32;
    fn time(t: *mut i64) -> i64;
    fn localtime_r(t: *const i64, result: *mut CTm) -> *mut CTm;
    // Only `reload_timezone`, which is only a test, calls this.
    #[cfg(test)]
    fn tzset();
    fn flock(fd: i32, operation: i32) -> i32;
    fn geteuid() -> u32;
    fn getegid() -> u32;
    // `open(2)` is variadic (`int open(const char *, int, ...)`), and one
    // declaration of it for the whole directory rather than one per
    // caller: two non-variadic views of the same C function with
    // different argument counts is a hazard, and the compiler says so.
    // The mode is read only for `O_CREAT`, which nothing here passes.
    #[link_name = "open"]
    pub(super) fn c_open(path: *const i8, flags: i32, mode: i32) -> i32;
    fn posix_openpt(flags: i32) -> i32;
    fn grantpt(fd: i32) -> i32;
    fn unlockpt(fd: i32) -> i32;
    fn ptsname_r(fd: i32, buf: *mut u8, buflen: usize) -> i32;
    fn setsid() -> i32;
    fn ioctl(fd: i32, request: u64, arg: usize) -> i32;
    fn fcntl(fd: i32, command: i32, argument: i32) -> i32;
    fn dup2(from: i32, to: i32) -> i32;
    fn close(fd: i32) -> i32;
    fn tcgetattr(fd: i32, mode: *mut sys::Termios) -> i32;
    fn tcsetattr(fd: i32, actions: i32, mode: *const sys::Termios) -> i32;
    fn raise(signal: i32) -> i32;
    fn signal(signal: i32, handler: usize) -> usize;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
}

/// How a terminal is behaving: what to put back, and what to derive the
/// next mode from.
///
/// Opaque on purpose. Which bits mean what is this layer's business, and
/// a caller that could reach the flags would be a caller with a reason
/// to know a number that differs per OS.
#[derive(Clone, Copy)]
pub(crate) struct TerminalMode(sys::Termios);

/// How `fd`'s terminal is behaving right now, or `None` if it is not a
/// terminal at all.
pub(crate) fn terminal_mode(fd: i32) -> Option<TerminalMode> {
    // SAFETY: a `Termios` of zeroes is a valid one to be written over,
    // and `tcgetattr` writes the whole of it or fails.
    let mut mode: sys::Termios = unsafe { std::mem::zeroed() };
    match unsafe { tcgetattr(fd, &mut mode) } {
        0 => Some(TerminalMode(mode)),
        _ => None,
    }
}

/// Makes `fd`'s terminal behave as `mode` says, at once rather than
/// after whatever is queued has drained.
pub(crate) fn set_terminal_mode(fd: i32, mode: &TerminalMode) -> std::io::Result<()> {
    match unsafe { tcsetattr(fd, sys::TCSANOW, &mode.0) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Whether the line discipline is still holding typed bytes until a
/// newline -- that is, whether the terminal is *not* in raw mode.
///
/// `ICANON` is the bit that decides it. On a pty the two ends share one
/// set of these, so asking this of the *master* is how the side driving
/// it can tell that the program on the slave has taken the terminal --
/// which is what the vim corpus waits for before it types anything, and
/// its only caller. A test, therefore, so a release build does not
/// report code that is not dead but is not part of the shell either.
#[cfg(test)]
pub(crate) fn is_canonical(mode: &TerminalMode) -> bool {
    mode.0.c_lflag & sys::ICANON != 0
}

/// Raw mode, derived from whatever the terminal is doing now: no line
/// buffering (`ICANON` off), no echo (bish draws the line itself), no
/// signals from control characters (`ISIG` off -- the editor reads those
/// as plain bytes and decides for itself), and no output post-processing
/// (`OPOST` off, so a caller writes "\r\n" rather than relying on the
/// terminal to translate).
pub(crate) fn raw_mode(from: &TerminalMode) -> TerminalMode {
    let mut raw = from.0;
    raw.c_iflag &= !(sys::IGNBRK | sys::BRKINT | sys::PARMRK | sys::ISTRIP | sys::INLCR | sys::IGNCR | sys::ICRNL | sys::IXON);
    raw.c_oflag &= !sys::OPOST;
    raw.c_lflag &= !(sys::ECHO | sys::ICANON | sys::IEXTEN | sys::ISIG);
    raw.c_cflag &= !sys::CSIZE;
    raw.c_cflag |= sys::CS8;
    raw.c_cc[sys::VMIN] = 1;
    raw.c_cc[sys::VTIME] = 0;
    TerminalMode(raw)
}

/// Cooked mode, derived the same way -- and deliberately *not* the
/// bit-for-bit inverse of `raw_mode`.
///
/// Raw mode clears `IGNCR` alongside `ICRNL` (with both off a bare CR
/// passes through untranslated either way), but the two are not
/// independent: POSIX has `IGNCR` take priority and discard CR entirely
/// before `ICRNL` gets a say. Turning *both* back on -- the naive
/// symmetric inverse, tried first and caught interactively -- silently
/// ate every Enter: a script's `read` echoed what was typed and never
/// saw a line terminator, hanging on something that looked like it
/// should work. Real cooked mode leaves IGNCR/INLCR/ISTRIP/PARMRK/
/// IGNBRK/BRKINT alone; only ICANON/ECHO/ISIG/IEXTEN, OPOST and
/// ICRNL/IXON have to be forced back on for kernel-driven line editing
/// and echo to behave.
pub(crate) fn cooked_mode(from: &TerminalMode) -> TerminalMode {
    let mut cooked = from.0;
    cooked.c_iflag |= sys::ICRNL | sys::IXON;
    cooked.c_oflag |= sys::OPOST;
    cooked.c_lflag |= sys::ECHO | sys::ICANON | sys::IEXTEN | sys::ISIG;
    TerminalMode(cooked)
}

/// The terminal exactly as it is, minus the echo -- not raw mode: the
/// line discipline still edits and buffers a line, it just does not show
/// it. `read -s`'s own contract.
pub(crate) fn echoless_mode(from: &TerminalMode) -> TerminalMode {
    let mut silent = from.0;
    silent.c_lflag &= !sys::ECHO;
    TerminalMode(silent)
}

/// Sends `signal` to this process.
pub(crate) fn raise_signal(signal_number: i32) {
    unsafe { raise(signal_number) };
}

/// Reads whatever is there, retrying an interrupted read.
///
/// `Ok(0)` is end of input. A read cut short by a signal is not an
/// answer about the file at all, so it is taken again rather than
/// reported -- which is what every caller here would do with it.
pub(crate) fn read_bytes(fd: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let n = unsafe { read(fd, buf.as_mut_ptr(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let failure = std::io::Error::last_os_error();
        if failure.kind() != std::io::ErrorKind::Interrupted {
            return Err(failure);
        }
    }
}

/// A terminal's size, in both cells and pixels.
///
/// `struct winsize` is four unsigned shorts in that order on every Unix
/// bish targets, so unlike `Termios` this one layout serves both -- the
/// `ioctl` numbers that carry it are what differ.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Winsize {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) xpixel: u16,
    pub(crate) ypixel: u16,
}

/// A fresh pty: the master end, and the path of the slave end.
///
/// Only the path, never an open slave fd -- holding one would leak it
/// into every child spawned from this process afterwards. The master is
/// close-on-exec for the same reason: a child that was not deliberately
/// attached to this pty has no business inheriting either end.
pub(crate) fn open_pty() -> std::io::Result<(std::fs::File, String)> {
    use std::os::fd::FromRawFd;

    let master = unsafe { posix_openpt(sys::O_RDWR | sys::O_NOCTTY) };
    if master < 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe { fcntl(master, sys::F_SETFD, sys::FD_CLOEXEC) };

    let name = || -> std::io::Result<String> {
        if unsafe { grantpt(master) } != 0 || unsafe { unlockpt(master) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = [0u8; 64];
        if unsafe { ptsname_r(master, buf.as_mut_ptr(), buf.len()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
        Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
    };
    match name() {
        // SAFETY: a fresh descriptor from posix_openpt, owned by nothing
        // else.
        Ok(path) => Ok((unsafe { std::fs::File::from_raw_fd(master) }, path)),
        Err(failure) => {
            unsafe { close(master) };
            Err(failure)
        }
    }
}

/// Makes `slave_path` this process's controlling terminal and its fd
/// 0/1/2.
///
/// `setsid` first, and it is a precondition rather than tidiness:
/// `TIOCSCTTY` only takes for a session leader with no controlling
/// terminal yet. The `ioctl` is then explicit rather than relying on the
/// first tty a session leader opens becoming its controlling terminal by
/// itself -- true on Linux, and not something to depend on.
///
/// Called in two places: from a freshly forked child on its way to
/// `exec`, and by a session daemon attaching *itself* to a pty of its own
/// so that everything in bish which assumes a real terminal on 0/1/2
/// keeps working untouched.
pub(crate) fn attach_to_pty_slave(slave_path: &std::ffi::CStr) -> std::io::Result<()> {
    unsafe {
        if setsid() < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let slave = c_open(slave_path.as_ptr(), sys::O_RDWR, 0);
        if slave < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if ioctl(slave, sys::TIOCSCTTY, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for target in 0..3 {
            if dup2(slave, target) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        if slave > 2 {
            close(slave);
        }
    }
    Ok(())
}

/// How big the terminal on `fd` is.
pub(crate) fn terminal_size(fd: std::os::unix::io::RawFd) -> std::io::Result<Winsize> {
    let mut size = Winsize::default();
    match unsafe { ioctl(fd, sys::TIOCGWINSZ, &mut size as *mut Winsize as usize) } {
        0 => Ok(size),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Tells the terminal on `fd` how big it now is -- for the pty bish owns
/// the far end of, whose size is whatever bish says it is.
pub(crate) fn set_terminal_size(fd: std::os::unix::io::RawFd, rows: u16, cols: u16) -> std::io::Result<()> {
    let size = Winsize { rows, cols, xpixel: 0, ypixel: 0 };
    match unsafe { ioctl(fd, sys::TIOCSWINSZ, &size as *const Winsize as usize) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Hands the terminal on `fd` to process group `pgrp`.
///
/// The other half of job control: `setpgid` puts a job in its own group,
/// and this is what makes the terminal driver send its own signals --
/// Ctrl-C, Ctrl-Z -- to that group rather than to bish.
pub(crate) fn set_foreground_pgrp(fd: std::os::unix::io::RawFd, pgrp: i32) -> std::io::Result<()> {
    match unsafe { ioctl(fd, sys::TIOCSPGRP, &pgrp as *const i32 as usize) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Stops reads on `fd` from blocking -- for a loop that drains whatever
/// has arrived and gets on with the rest of its tick.
pub(crate) fn set_nonblocking(fd: std::os::unix::io::RawFd) {
    unsafe {
        let flags = fcntl(fd, sys::F_GETFL, 0);
        fcntl(fd, sys::F_SETFL, flags | sys::O_NONBLOCK);
    }
}

/// Which permission `is_accessible` is asking about.
pub(crate) enum Access {
    Read,
    Write,
    Execute,
}

/// Whether this process could read, write or execute `path` -- asked of
/// the OS rather than worked out from the mode bits, because the answer
/// depends on the effective user, the groups it is in, and whatever the
/// filesystem thinks on top of that.
///
/// `R_OK`/`W_OK`/`X_OK` are 4, 2 and 1 on every Unix bish targets, so
/// they are here rather than in the tables.
pub(crate) fn is_accessible(path: &str, mode: Access) -> bool {
    let mode = match mode {
        Access::Read => 4,
        Access::Write => 2,
        Access::Execute => 1,
    };
    let Ok(path) = std::ffi::CString::new(path) else { return false };
    unsafe { access(path.as_ptr(), mode) == 0 }
}

/// Who this process is acting as, which is the question that matters for
/// "may I" -- not who started it.
pub(crate) fn effective_user() -> u32 {
    unsafe { geteuid() }
}

pub(crate) fn effective_group() -> u32 {
    unsafe { getegid() }
}

/// Whether `fd` is a terminal.
///
/// `std::io::IsTerminal` rather than `isatty` directly: it is the same
/// call underneath, and one fewer C declaration to keep right. The
/// borrow is for the length of the question only -- nothing here takes
/// ownership of the descriptor or closes it.
pub(crate) fn is_terminal(fd: std::os::unix::io::RawFd) -> bool {
    use std::io::IsTerminal;
    // SAFETY: borrowed, not owned: the `BorrowedFd` is dropped at the end
    // of this expression and `is_terminal` does not close what it is
    // asked about.
    unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }.is_terminal()
}

/// Takes an exclusive advisory lock on `fd`, or says it could not.
///
/// Never waits: a caller that cannot have the lock now has something
/// better to do than queue -- skip the compaction another bish is
/// already doing, or refuse to be the second daemon of the same name.
/// The lock lives with the open file and goes when the last descriptor
/// for it closes, which is what lets a lock stand for "this process is
/// still alive" across a crash.
///
/// `LOCK_EX`, `LOCK_NB` and `LOCK_UN` are 2, 4 and 8 on every Unix bish
/// targets, so they are here rather than in the per-OS tables.
pub(crate) fn lock_exclusive(fd: std::os::unix::io::RawFd) -> std::io::Result<()> {
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    match unsafe { flock(fd, LOCK_EX | LOCK_NB) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Gives a lock back without waiting for the file to close.
pub(crate) fn unlock(fd: std::os::unix::io::RawFd) {
    const LOCK_UN: i32 = 8;
    unsafe { flock(fd, LOCK_UN) };
}

/// C's `struct tm`: POSIX's nine fields, plus the `tm_gmtoff`/`tm_zone`
/// extension glibc and the BSDs both have, in the order they both have
/// it.
///
/// One of the few kernel-adjacent layouts that is genuinely the same on
/// Linux and macOS, so it is here rather than in the per-OS tables. It
/// has to match the real thing size for size whatever bish reads of it:
/// `localtime_r` writes a whole `struct tm` into the pointer it is given.
#[repr(C)]
pub(crate) struct CTm {
    pub(crate) tm_sec: i32,
    pub(crate) tm_min: i32,
    pub(crate) tm_hour: i32,
    pub(crate) tm_mday: i32,
    pub(crate) tm_mon: i32,
    pub(crate) tm_year: i32,
    pub(crate) tm_wday: i32,
    pub(crate) tm_yday: i32,
    pub(crate) tm_isdst: i32,
    pub(crate) tm_gmtoff: i64,
    pub(crate) tm_zone: *const i8,
}

impl Default for CTm {
    fn default() -> CTm {
        CTm {
            tm_sec: 0,
            tm_min: 0,
            tm_hour: 0,
            tm_mday: 0,
            tm_mon: 0,
            tm_year: 0,
            tm_wday: 0,
            tm_yday: 0,
            tm_isdst: 0,
            tm_gmtoff: 0,
            tm_zone: std::ptr::null(),
        }
    }
}

/// Now, in seconds since the epoch.
pub(crate) fn epoch_seconds() -> i64 {
    let mut now: i64 = 0;
    unsafe { time(&mut now as *mut i64) };
    now
}

/// `epoch_seconds` broken down into the local calendar, with the zone
/// offset and abbreviation the C library has for it -- which is where
/// bish's dates come from, there being no date crate.
pub(crate) fn local_time_at(epoch_seconds: i64) -> CTm {
    let mut broken_down = CTm::default();
    unsafe { localtime_r(&epoch_seconds as *const i64, &mut broken_down as *mut CTm) };
    broken_down
}

/// Re-reads `TZ`.
///
/// The C library caches the timezone, so a process that changes `TZ`
/// after it has formatted one date is otherwise still in the old zone.
/// A test pinning a date to UTC is the only reason bish ever changes
/// `TZ`, so this is a test too -- a release build would otherwise report
/// it as dead.
#[cfg(test)]
pub(crate) fn reload_timezone() {
    unsafe { tzset() };
}

/// One resource limit: what this process may have now, and the most it
/// may raise itself to.
///
/// `rlim_t` is 64-bit on both OSes and `RLIM_INFINITY` is its maximum on
/// both, so this one layout serves -- it is the resource *numbers* that
/// differ, and those are in the tables.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct ResourceLimit {
    pub(crate) soft: u64,
    pub(crate) hard: u64,
}

/// "No limit", as both OSes spell it.
pub(crate) const UNLIMITED: u64 = u64::MAX;

/// Which `ulimit` flags this OS has a limit for, and which number each
/// one is. `None` means this OS has no such limit, and `ulimit` leaves
/// it out of `-a` and refuses to set it -- which is what bash on that OS
/// does.
pub(crate) fn resource_for_flag(flag: char) -> Option<i32> {
    sys::rlimit_number(flag)
}

/// How much of `resource` this process may use.
pub(crate) fn resource_limit(resource: i32) -> ResourceLimit {
    let mut limit = ResourceLimit::default();
    unsafe { getrlimit(resource, &mut limit) };
    limit
}

/// How much of `resource` this process may use from now on.
pub(crate) fn set_resource_limit(resource: i32, limit: &ResourceLimit) -> std::io::Result<()> {
    match unsafe { setrlimit(resource, limit) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// How much stack this process was given, or `None` when there is no
/// limit on it to find.
pub(crate) fn stack_limit() -> Option<u64> {
    let limit = resource_limit(sys::RLIMIT_STACK);
    (limit.soft != UNLIMITED && limit.soft != 0).then_some(limit.soft)
}

/// `struct tms`: four clock-tick counts, this process's and its reaped
/// children's, user and system. `clock_t` is 64-bit on both OSes.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Tms {
    pub(crate) utime: i64,
    pub(crate) stime: i64,
    pub(crate) cutime: i64,
    pub(crate) cstime: i64,
}

/// CPU used so far, in seconds: this shell's own, and everything it has
/// waited for.
///
/// Seconds rather than ticks, because the divisor is the OS's business:
/// `sysconf`'s name for the tick rate is 2 on Linux and 3 on macOS, and
/// asking for the wrong one answers about something else entirely.
pub(crate) fn cpu_seconds() -> Option<(f64, f64, f64, f64)> {
    let mut spent = Tms::default();
    if unsafe { times(&mut spent as *mut Tms) } == -1 {
        return None;
    }
    let ticks = unsafe { sysconf(sys::SC_CLK_TCK) }.max(1) as f64;
    Some((spent.utime as f64 / ticks, spent.stime as f64 / ticks, spent.cutime as f64 / ticks, spent.cstime as f64 / ticks))
}

/// The current file-creation mask.
///
/// POSIX has no query-only read -- `umask(new)` returns the previous one
/// and that is the only primitive -- so this sets it back immediately and
/// leaves nothing observable behind.
pub(crate) fn current_umask() -> u32 {
    let current = unsafe { umask(0) };
    unsafe { umask(current) };
    current
}

/// Sets the file-creation mask, giving back what it was.
pub(crate) fn set_umask(mask: u32) -> u32 {
    unsafe { umask(mask) }
}

/// One descriptor to wait on, and what the OS reported about it.
///
/// `struct pollfd` and the flag values are the same on Linux and macOS;
/// the count `poll` takes is not the same width, which is in the tables.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

pub(crate) const POLL_READABLE: i16 = 0x0001;
pub(crate) const POLL_WRITABLE: i16 = 0x0004;
pub(crate) const POLL_ERROR: i16 = 0x0008;
pub(crate) const POLL_HUNG_UP: i16 = 0x0010;

impl PollFd {
    pub(crate) fn watching(fd: std::os::unix::io::RawFd, events: i16) -> PollFd {
        PollFd { fd, events, revents: 0 }
    }

    pub(crate) fn fd(&self) -> std::os::unix::io::RawFd {
        self.fd
    }

    /// What the OS said about it, as a mask of the `POLL_*` above.
    pub(crate) fn reported(&self) -> i16 {
        self.revents
    }

    /// Forgets the last answer, before asking again.
    pub(crate) fn clear(&mut self) {
        self.revents = 0;
    }
}

/// Waits until one of `fds` has something to say, or `timeout_ms` passes
/// (negative waits indefinitely).
///
/// `Ok(0)` is "the timeout passed, or a signal interrupted the wait" --
/// the two are the same answer to every caller here, since a signal worth
/// acting on arrives through a descriptor in this set rather than through
/// this call's return value.
pub(crate) fn wait_for_ready(fds: &mut [PollFd], timeout_ms: i32) -> std::io::Result<usize> {
    let ready = unsafe { poll(fds.as_mut_ptr(), fds.len() as sys::NFds, timeout_ms) };
    if ready >= 0 {
        return Ok(ready as usize);
    }
    let failure = std::io::Error::last_os_error();
    match failure.kind() {
        std::io::ErrorKind::Interrupted => Ok(0),
        _ => Err(failure),
    }
}

/// A pipe for waking a wait: one end to register, one for a signal
/// handler to write a byte to.
///
/// Both ends non-blocking -- the handler must never block, and the
/// draining side wants to stop at "nothing left" rather than wait for
/// more -- and both close-on-exec, since no child has any business
/// holding either.
pub(crate) fn wake_pipe() -> std::io::Result<(std::os::unix::io::RawFd, std::os::unix::io::RawFd)> {
    let mut ends = [0i32; 2];
    if unsafe { pipe(ends.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    for end in ends {
        set_nonblocking(end);
        unsafe { fcntl(end, sys::F_SETFD, sys::FD_CLOEXEC) };
    }
    Ok((ends[0], ends[1]))
}

/// Writes one byte to a wake pipe.
///
/// The only thing in this layer meant to be called from inside a real
/// signal handler, so it does exactly one `write(2)` -- which POSIX lists
/// as async-signal-safe -- and nothing else: no allocation, no error
/// formatting, no locks. A full pipe means a wake-up is already queued,
/// which is as good as having written another.
pub(crate) fn wake(fd: std::os::unix::io::RawFd) {
    unsafe { write(fd, [0u8].as_ptr(), 1) };
}

/// Closes a raw descriptor this layer handed out.
pub(crate) fn close_fd(fd: std::os::unix::io::RawFd) {
    unsafe { close(fd) };
}

/// Points `to` at whatever `from` refers to, as a child does on its way
/// to becoming a pipeline stage. The new descriptor is *not*
/// close-on-exec, whatever `from` was -- which is what makes this the way
/// to hand something to a child deliberately.
pub(crate) fn duplicate_onto(from: std::os::unix::io::RawFd, to: std::os::unix::io::RawFd) -> bool {
    unsafe { dup2(from, to) >= 0 }
}

/// Installs `handler` for `signal_number`.
///
/// Only a handler that is itself async-signal-safe belongs here -- see
/// `wake`, which is what bish's handlers do.
pub(crate) fn on_signal(signal_number: i32, handler: extern "C" fn(i32)) {
    unsafe { signal(signal_number, handler as *const () as usize) };
}

/// A second descriptor for whatever `fd` refers to, at the lowest free
/// number. `None` on a closed or invalid one, which every caller here
/// treats as "nothing to save".
pub(crate) fn duplicate(fd: std::os::unix::io::RawFd) -> Option<std::os::unix::io::RawFd> {
    match unsafe { dup(fd) } {
        copy if copy < 0 => None,
        copy => Some(copy),
    }
}

/// `duplicate`, as something that closes itself.
pub(crate) fn duplicate_as_file(fd: std::os::unix::io::RawFd) -> Option<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: a fresh descriptor from `dup`, owned by nothing else.
    duplicate(fd).map(|copy| unsafe { std::fs::File::from_raw_fd(copy) })
}

/// A close-on-exec duplicate of `fd` numbered at or above `floor` --
/// for putting a descriptor somewhere a redirect will not land on it.
///
/// Close-on-exec is the point as much as the floor is: the copy only ever
/// exists to be the *source* of a later `dup2`, and nothing started in
/// between has any business inheriting it. The command for that is 1030
/// on Linux and 67 on macOS.
pub(crate) fn duplicate_above(fd: std::os::unix::io::RawFd, floor: i32) -> Option<std::os::unix::io::RawFd> {
    match unsafe { fcntl(fd, sys::F_DUPFD_CLOEXEC, floor) } {
        copy if copy < 0 => None,
        copy => Some(copy),
    }
}

/// Whether `fd` is open at all.
///
/// Asked rather than inferred: reading a descriptor nothing opened fails
/// with EBADF, which from a caller's side is indistinguishable from "no
/// input yet" -- and `read -u 9` has to tell those apart.
pub(crate) fn is_open(fd: std::os::unix::io::RawFd) -> bool {
    unsafe { fcntl(fd, sys::F_GETFD, 0) != -1 }
}

/// Makes `fd` non-blocking, handing back what its flags were so a caller
/// can put them back. `None` when the flags could not be read, in which
/// case nothing was changed.
pub(crate) fn make_nonblocking(fd: std::os::unix::io::RawFd) -> Option<i32> {
    let flags = unsafe { fcntl(fd, sys::F_GETFL, 0) };
    if flags < 0 {
        return None;
    }
    unsafe { fcntl(fd, sys::F_SETFL, flags | sys::O_NONBLOCK) };
    Some(flags)
}

/// Puts back flags `make_nonblocking` handed over.
pub(crate) fn restore_flags(fd: std::os::unix::io::RawFd, flags: i32) {
    unsafe { fcntl(fd, sys::F_SETFL, flags) };
}

/// Writes what it can, once.
///
/// Deliberately not a loop and deliberately not retrying anything:
/// `Ok(0)`, `WouldBlock` and `Interrupted` each mean something different
/// to a pipeline stage -- park, drain the pane, or simply go round again
/// -- and only the caller knows which.
pub(crate) fn write_bytes(fd: std::os::unix::io::RawFd, bytes: &[u8]) -> std::io::Result<usize> {
    let written = unsafe { write(fd, bytes.as_ptr(), bytes.len()) };
    match written {
        n if n >= 0 => Ok(n as usize),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Clears close-on-exec, so a child *does* inherit `fd`.
///
/// Needed even right after a `dup2`, which clears it on the new
/// descriptor: `dup2(fd, fd)` is a no-op per POSIX and does *not* clear
/// it, so a redirect whose source already sits on the target number
/// would otherwise be closed by the exec it was opened for.
pub(crate) fn clear_cloexec(fd: std::os::unix::io::RawFd) {
    unsafe { fcntl(fd, sys::F_SETFD, 0) };
}

/// The process that started this one -- `$PPID`.
pub(crate) fn parent_process_id() -> i32 {
    unsafe { getppid() }
}

/// Who started this process, as opposed to who it is acting as -- `$UID`
/// against `$EUID`.
pub(crate) fn real_user() -> u32 {
    unsafe { getuid() }
}

/// The device name of the terminal on `fd`, if it has one.
///
/// Best effort: a descriptor that is not a terminal, or a name that will
/// not fit, gives `None` -- which is all a prompt can do with it anyway.
pub(crate) fn terminal_name(fd: std::os::unix::io::RawFd) -> Option<String> {
    let mut buf = [0u8; 256];
    if unsafe { ttyname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// This machine's name.
///
/// `gethostname(2)` rather than a file: bish used to read
/// `/proc/sys/kernel/hostname`, which macOS does not have at all -- so
/// `\h` in a prompt there would have been empty but for `$HOSTNAME`
/// happening to be set.
pub(crate) fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    if unsafe { gethostname(buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

// `signal(2)`'s two special handlers, as plain numbers because that is
// what the C declaration takes. The same on every Unix.
const HANDLER_DEFAULT: usize = 0;
const HANDLER_IGNORE: usize = 1;

/// Installs `handler` for `signal_number`, through `sigaction` rather
/// than `signal`: it is the one with defined behaviour about what the
/// handler inherits, and the one that can also report what the
/// disposition was.
///
/// Only an async-signal-safe handler belongs here. bish's set a flag and
/// return.
pub(crate) fn handle_signal(signal_number: i32, handler: extern "C" fn(i32)) {
    set_disposition(signal_number, handler as *const () as usize);
}

/// Puts `signal_number` back to what the OS would do with it.
pub(crate) fn default_signal(signal_number: i32) {
    set_disposition(signal_number, HANDLER_DEFAULT);
}

/// Makes this process ignore `signal_number`.
///
/// Inherited by every forked child *and* carried through an `exec`:
/// POSIX resets a real handler to the default across one and explicitly
/// leaves "ignore" in place. So every site that spawns a real child has
/// to put it back with `default_signal`, or that child silently ignores
/// it too -- which is how a job comes to not answer Ctrl-C.
pub(crate) fn ignore_signal(signal_number: i32) {
    set_disposition(signal_number, HANDLER_IGNORE);
}

fn set_disposition(signal_number: i32, handler: usize) {
    let action = sys::SigAction { handler, ..sys::SigAction::default() };
    unsafe { sigaction(signal_number, &action, std::ptr::null_mut()) };
}

/// Whether `signal_number` is being ignored right now.
///
/// POSIX: a signal ignored on entry to a shell cannot be trapped or
/// reset, because the immunity was granted from outside -- `nohup`'s
/// promise, and a backgrounded script's protection from a Ctrl-C aimed at
/// the terminal. This is how bish finds out which those were.
pub(crate) fn is_signal_ignored(signal_number: i32) -> bool {
    let mut previous = sys::SigAction::default();
    let asked = unsafe { sigaction(signal_number, std::ptr::null(), &mut previous) } == 0;
    asked && previous.handler == HANDLER_IGNORE
}

/// Sends `signal_number` to one process.
pub(crate) fn send_signal(pid: i32, signal_number: i32) -> bool {
    unsafe { kill(pid, signal_number) == 0 }
}

/// Sends `signal_number` to a whole process group, which is what the
/// terminal driver itself does for a Ctrl-C.
pub(crate) fn send_signal_to_group(pgid: i32, signal_number: i32) -> bool {
    unsafe { kill(-pgid, signal_number) == 0 }
}

/// Puts `pid` into process group `pgid` -- half of job control; handing
/// the terminal to that group (`set_foreground_pgrp`) is the other.
pub(crate) fn set_process_group(pid: i32, pgid: i32) -> bool {
    unsafe { setpgid(pid, pgid) == 0 }
}

/// This process's own group.
pub(crate) fn process_group() -> i32 {
    unsafe { getpgrp() }
}

/// What became of a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Waited {
    Exited(i32),
    /// Killed by a signal -- the number, not bash's 128 + n, which is the
    /// shell's own convention and not the OS's.
    Killed(i32),
    /// Stopped and still there, which `std::process::Child` cannot
    /// observe at all: it has no way to ask for it, so a Ctrl-Z'd child
    /// looks like one that is still running, for ever.
    Stopped(i32),
    /// Nothing has changed yet (only ever answered when not waiting).
    Running,
    /// There is nothing to wait for: already reaped, never ours, gone.
    Gone,
}

/// Waits for one child, reporting a stop as well as an exit.
///
/// `block` of false asks without waiting. An interrupted wait is retried,
/// since a signal arriving is not news about the child.
///
/// The status encoding -- the low seven bits for a signal, 0x7f for
/// "stopped", the next eight for the code -- is the same on every Unix
/// bish targets, so the decoding is here rather than in the tables.
pub(crate) fn wait_for_child(pid: i32, block: bool) -> Waited {
    const WNOHANG: i32 = 1;
    const WUNTRACED: i32 = 2;

    let options = if block { WUNTRACED } else { WNOHANG | WUNTRACED };
    loop {
        let mut status: i32 = 0;
        let answer = unsafe { waitpid(pid, &mut status, options) };
        if answer == 0 {
            return Waited::Running;
        }
        if answer < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Waited::Gone;
        }
        if status & 0xff == 0x7f {
            return Waited::Stopped((status >> 8) & 0xff);
        }
        return match status & 0x7f {
            0 => Waited::Exited((status >> 8) & 0xff),
            signal_number => Waited::Killed(signal_number),
        };
    }
}

/// Creates `path` with exactly `mode`, whatever the umask says.
///
/// `std::fs::create_dir` cannot: it creates with 0o777 and lets the umask
/// subtract, so a directory that must be 0700 -- one holding a socket only
/// this user may connect to -- would depend on whatever umask happened to
/// be in effect. `AlreadyExists` is left for the caller to judge, because
/// "it is already there" is not the same as "the directory I meant is
/// already there".
pub(crate) fn create_dir_with_mode(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    match unsafe { mkdir(c_path.as_ptr(), mode) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    // Covers what `darwin.rs`'s `anonymous_file` is, on a machine that
    // is not a Mac: a readable, writable file that no longer has a name.
    #[test]
    fn an_unlinked_temp_file_reads_back_what_was_written_and_has_no_path_left() {
        let before: Vec<std::path::PathBuf> = std::fs::read_dir(std::env::temp_dir())
            .expect("the temp directory is readable")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains("bish-capture-test-"))
            .collect();
        assert!(before.is_empty(), "a previous run left these behind: {before:?}");

        let mut file = super::unlinked_temp_file("capture-test").expect("an anonymous file");
        file.write_all(b"captured output").expect("write");
        file.seek(SeekFrom::Start(0)).expect("rewind");
        let mut read_back = String::new();
        file.read_to_string(&mut read_back).expect("read");
        assert_eq!(read_back, "captured output");

        // Nothing of it is in the directory it was made in, before the
        // descriptor is even dropped.
        let left: Vec<std::path::PathBuf> = std::fs::read_dir(std::env::temp_dir())
            .expect("the temp directory is readable")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains("bish-capture-test-"))
            .collect();
        assert!(left.is_empty(), "the name should be gone while the fd is still open: {left:?}");
    }

    // `DirSnapshot` is the whole of what macOS has to work out for
    // itself, since kqueue says a directory changed without saying which
    // name in it did. Everything it has to get right is checked here, on
    // a machine that is not a Mac.
    #[test]
    fn a_directory_snapshot_notices_what_changed_and_nothing_else() {
        use super::{DirSnapshot, EntryChange};
        let dir = crate::tempdir::TempDir::new("platform-snapshot");
        let dir = dir.path();
        std::fs::write(dir.join("kept.txt"), "same").unwrap();
        std::fs::write(dir.join("edited.txt"), "before").unwrap();
        std::fs::write(dir.join("replaced.txt"), "abcdef").unwrap();
        std::fs::write(dir.join("removed.txt"), "bye").unwrap();
        let before = DirSnapshot::of(dir);

        std::fs::write(dir.join("appeared.txt"), "new").unwrap();
        std::fs::write(dir.join("edited.txt"), "after it grew").unwrap();
        // A rename over the top: the case an inode watch cannot see. The
        // content is the same length as what it replaces, so the inode is
        // the only thing that says anything happened.
        std::fs::write(dir.join("tmp"), "ABCDEF").unwrap();
        std::fs::rename(dir.join("tmp"), dir.join("replaced.txt")).unwrap();
        std::fs::remove_file(dir.join("removed.txt")).unwrap();

        let mut changes = DirSnapshot::of(dir).changes_since(&before);
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        let named: Vec<(String, EntryChange)> = changes.into_iter().map(|(n, c)| (n.to_string_lossy().into_owned(), c)).collect();
        assert_eq!(
            named,
            vec![
                ("appeared.txt".to_string(), EntryChange::Touched),
                ("edited.txt".to_string(), EntryChange::Touched),
                ("removed.txt".to_string(), EntryChange::Gone),
                ("replaced.txt".to_string(), EntryChange::Touched),
            ],
            "`kept.txt` and the vanished `tmp` are the ones that must not appear"
        );
    }

    #[test]
    fn a_directory_that_is_gone_reads_as_everything_in_it_going() {
        use super::{DirSnapshot, EntryChange};
        let dir = crate::tempdir::TempDir::new("platform-snapshot-gone");
        let inner = dir.path().join("sub");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(inner.join("f.txt"), "x").unwrap();
        let before = DirSnapshot::of(&inner);
        std::fs::remove_dir_all(&inner).unwrap();
        let changes = DirSnapshot::of(&inner).changes_since(&before);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].1, EntryChange::Gone);
    }
}
