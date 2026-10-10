//! How a POSIX system does what bish needs, written once for every Unix:
//! the differing numbers and layouts come from `sys_linux.rs` and
//! `sys_darwin.rs` as `sys::*`. A non-Unix target replaces this file (see
//! `mod.rs`).

use super::sys;

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

/// `guard + usable` bytes of new address space, the lowest `guard` bytes
/// unreadable: a stack for bish's own use, growing down, so an overrun
/// faults rather than corrupting the next mapping. Pages fault in as
/// touched, so an untouched megabyte costs a page-table entry.
pub(crate) fn map_guarded_stack(guard: usize, usable: usize) -> std::io::Result<*mut u8> {
    // POSIX's `(void *) -1`, the same on every Unix.
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

/// A file with no name: created in the temp directory and unlinked at
/// once, so the descriptor is the only way to it and nothing is left if
/// bish dies. `tag` makes the brief name recognisable. Here rather than in
/// `darwin.rs`, its only caller, because it's all POSIX and so compiled
/// and tested on Linux too.
#[allow(dead_code)]
pub(crate) fn unlinked_temp_file(tag: &str) -> Option<std::fs::File> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // `create_new`, so an existing name is never opened; a few tries, since
    // only a collision fails and another name settles it.
    for _ in 0..8 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("bish-{tag}-{}-{n}", std::process::id()));
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path) else { continue };
        // From here the descriptor is all there is. A failed unlink would
        // leave a file behind, so the caller is told there's no anonymous
        // file rather than given one that litters.
        if std::fs::remove_file(&path).is_err() {
            return None;
        }
        return Some(file);
    }
    None
}

/// A temp file that keeps its name, created so that no name planted in
/// advance can be opened instead: `create_new` (`O_CREAT|O_EXCL`) fails on
/// an existing name, symlinks included, so a planted
/// `/tmp/bish-<tag>-<pid>-<n>` moves this to the next number. Planting all
/// eight denies the file but never redirects it. The caller removes it.
pub(crate) fn exclusive_temp_file(tag: &str) -> Option<(std::fs::File, std::path::PathBuf)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..8 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("bish-{tag}-{}-{n}", std::process::id()));
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path) else { continue };
        return Some((file, path));
    }
    None
}

/// A directory at one moment: every name in it with enough identity to
/// tell changed from same. For kqueue, which says a directory changed but
/// not which name; inotify needs none of this. Here, all `std::fs`, so
/// it's tested on Linux, where it's unused at run time.
#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct DirSnapshot {
    entries: std::collections::HashMap<std::ffi::OsString, Stamp>,
}

/// The parts of an entry worth comparing, inode included: a formatter or
/// branch switch replaces a file by renaming, changing the inode and
/// maybe not size or mtime.
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
    /// It appeared, isn't the same file, or was written: one answer, since
    /// every caller's next move is to look.
    Touched,
    /// It is no longer in the directory.
    Gone,
}

#[allow(dead_code)]
impl DirSnapshot {
    /// An unreadable or missing directory snapshots as empty, so its
    /// disappearance reads as every name going, which is true for anyone
    /// watching one.
    pub(crate) fn of(dir: &std::path::Path) -> DirSnapshot {
        use std::os::unix::fs::MetadataExt;
        let mut entries = std::collections::HashMap::new();
        let Ok(listing) = std::fs::read_dir(dir) else { return DirSnapshot::default() };
        for entry in listing.flatten() {
            // `symlink_metadata`, so the symlink itself is watched:
            // repointing it changes this directory, and a dangling one
            // still has an entry.
            let Ok(meta) = entry.path().symlink_metadata() else { continue };
            entries.insert(entry.file_name(), Stamp { inode: meta.ino(), size: meta.size(), modified: meta.modified().ok() });
        }
        DirSnapshot { entries }
    }

    /// Every name not as it was in `before`; names absent from both or the
    /// same in both aren't mentioned, keeping siblings out of a watched
    /// file's events.
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
    fn write(fd: i32, buf: *const std::ffi::c_void, count: usize) -> isize;
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
    // `open(2)` is variadic, `int open(const char *, int, ...)`, and
    // declared so: rustc refuses a non-variadic view
    // (`invalid_runtime_symbol_definitions`), rightly, since Apple's arm64
    // ABI passes variadic arguments differently. Nothing here passes a
    // mode. One declaration for the whole directory: two views of one C
    // function are a hazard.
    #[link_name = "open"]
    pub(super) fn c_open(path: *const i8, flags: i32, ...) -> i32;
    fn posix_openpt(flags: i32) -> i32;
    fn grantpt(fd: i32) -> i32;
    fn unlockpt(fd: i32) -> i32;
    fn ptsname_r(fd: i32, buf: *mut u8, buflen: usize) -> i32;
    fn setsid() -> i32;
    // Variadic for the same reason: on Apple's arm64 a fixed third argument
    // goes in a register while the callee reads a variadic one off the
    // stack, so `F_SETFL` and every ioctl's pointer would get garbage.
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    pub(super) fn fcntl(fd: i32, command: i32, ...) -> i32;
    fn dup2(from: i32, to: i32) -> i32;
    fn close(fd: i32) -> i32;
    fn tcgetattr(fd: i32, mode: *mut sys::Termios) -> i32;
    fn tcsetattr(fd: i32, actions: i32, mode: *const sys::Termios) -> i32;
    fn raise(signal: i32) -> i32;
    fn signal(signal: i32, handler: usize) -> usize;
    fn read(fd: i32, buf: *mut std::ffi::c_void, count: usize) -> isize;
}

/// How a terminal is behaving: what to restore and what to derive the
/// next mode from. Opaque, since the bits differ per OS and are this
/// layer's business.
#[derive(Clone, Copy)]
pub(crate) struct TerminalMode(sys::Termios);

/// How `fd`'s terminal is behaving right now, or `None` if it is not a
/// terminal at all.
pub(crate) fn terminal_mode(fd: i32) -> Option<TerminalMode> {
    // SAFETY: zeroes are a valid `Termios` to overwrite, and `tcgetattr`
    // writes all of it or fails.
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

/// Whether the line discipline still holds typed bytes until a newline,
/// that is, not raw mode. On a pty both ends share these, so asking the
/// master shows whether the program on the slave took the terminal,
/// which the vim corpus waits for before typing. Its only caller is that
/// test.
#[cfg(test)]
pub(crate) fn is_canonical(mode: &TerminalMode) -> bool {
    mode.0.c_lflag & sys::ICANON != 0
}

/// Raw mode from the current mode: no line buffering (`ICANON` off), no
/// echo (bish draws the line), no signals from control characters (`ISIG`
/// off: the editor reads them as bytes), no output processing (`OPOST`
/// off, so callers write "\r\n").
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

/// Cooked mode from the current mode, not the bit-for-bit inverse of
/// `raw_mode`: POSIX has `IGNCR` discard CR before `ICRNL` acts, so turning
/// both back on would eat every Enter (a `read` that never sees a line
/// end). Cooked mode leaves IGNCR, INLCR, ISTRIP, PARMRK, IGNBRK and BRKINT
/// alone and forces ICANON, ECHO, ISIG, IEXTEN, OPOST, ICRNL and IXON on.
pub(crate) fn cooked_mode(from: &TerminalMode) -> TerminalMode {
    let mut cooked = from.0;
    cooked.c_iflag |= sys::ICRNL | sys::IXON;
    cooked.c_oflag |= sys::OPOST;
    cooked.c_lflag |= sys::ECHO | sys::ICANON | sys::IEXTEN | sys::ISIG;
    TerminalMode(cooked)
}

/// The terminal as is, minus echo: not raw mode, the line discipline
/// still edits a line without showing it. What `read -s` needs.
pub(crate) fn echoless_mode(from: &TerminalMode) -> TerminalMode {
    let mut silent = from.0;
    silent.c_lflag &= !sys::ECHO;
    TerminalMode(silent)
}

/// Sends `signal` to this process.
pub(crate) fn raise_signal(signal_number: i32) {
    unsafe { raise(signal_number) };
}

/// Reads what's there, retrying an interrupted read: `Ok(0)` is end of
/// input, and a read cut short by a signal says nothing about the file.
pub(crate) fn read_bytes(fd: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let n = unsafe { read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let failure = std::io::Error::last_os_error();
        if failure.kind() != std::io::ErrorKind::Interrupted {
            return Err(failure);
        }
    }
}

/// A terminal's size in cells and pixels. `struct winsize` is four
/// unsigned shorts in order on every Unix bish targets, so one layout
/// serves; the `ioctl` numbers differ.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Winsize {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) xpixel: u16,
    pub(crate) ypixel: u16,
}

/// A new pty: the master and the slave's path. Never an open slave fd,
/// which would leak into every later child; the master is close-on-exec
/// for the same reason.
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
        // SAFETY: a fresh descriptor from `posix_openpt`, owned by nothing
        // else.
        Ok(path) => Ok((unsafe { std::fs::File::from_raw_fd(master) }, path)),
        Err(failure) => {
            unsafe { close(master) };
            Err(failure)
        }
    }
}

/// Makes `slave_path` this process's controlling terminal and fds 0-2.
/// `setsid` first: `TIOCSCTTY` works only for a session leader without a
/// controlling terminal. The explicit `ioctl` rather than relying on the
/// first tty opened becoming it, which is Linux behaviour. Used by a
/// forked child before `exec`, and by a session daemon attaching itself to
/// a pty so bish's terminal assumptions hold.
pub(crate) fn attach_to_pty_slave(slave_path: &std::ffi::CStr) -> std::io::Result<()> {
    unsafe {
        if setsid() < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let slave = c_open(slave_path.as_ptr(), sys::O_RDWR);
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

/// Tells the terminal on `fd` its size, for the pty whose far end bish
/// owns.
pub(crate) fn set_terminal_size(fd: std::os::unix::io::RawFd, rows: u16, cols: u16) -> std::io::Result<()> {
    let size = Winsize { rows, cols, xpixel: 0, ypixel: 0 };
    match unsafe { ioctl(fd, sys::TIOCSWINSZ, &size as *const Winsize as usize) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Gives the terminal on `fd` to process group `pgrp`: with `setpgid`
/// putting a job in its own group, this makes the terminal driver send
/// Ctrl-C and Ctrl-Z to that group rather than bish.
pub(crate) fn set_foreground_pgrp(fd: std::os::unix::io::RawFd, pgrp: i32) -> std::io::Result<()> {
    match unsafe { ioctl(fd, sys::TIOCSPGRP, &pgrp as *const i32 as usize) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Makes reads on `fd` non-blocking, for a loop draining what arrived.
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

/// Whether this process could read, write or execute `path`, asked of the
/// OS rather than worked out from mode bits, which depends on the
/// effective user, groups and filesystem. `R_OK`/`W_OK`/`X_OK` are 4, 2 and
/// 1 on every Unix bish targets.
pub(crate) fn is_accessible(path: &str, mode: Access) -> bool {
    let mode = match mode {
        Access::Read => 4,
        Access::Write => 2,
        Access::Execute => 1,
    };
    let Ok(path) = std::ffi::CString::new(path) else { return false };
    unsafe { access(path.as_ptr(), mode) == 0 }
}

/// Who this process acts as, which decides "may I", not who started it.
pub(crate) fn effective_user() -> u32 {
    unsafe { geteuid() }
}

/// A file's owner from its metadata: bish's one place reading a uid off a
/// file, through this layer so a non-Unix target answers its own way
/// (`web.rs`).
pub(crate) fn file_owner(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.uid()
}

pub(crate) fn effective_group() -> u32 {
    unsafe { getegid() }
}

/// Whether `fd` is a terminal, through `std::io::IsTerminal` (the same
/// call as `isatty`, one fewer declaration), borrowing the fd only for the
/// question.
pub(crate) fn is_terminal(fd: std::os::unix::io::RawFd) -> bool {
    use std::io::IsTerminal;
    // SAFETY: borrowed, not owned; `is_terminal` doesn't close it.
    unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }.is_terminal()
}

/// Takes an exclusive advisory lock on `fd`, never waiting: a caller
/// without the lock skips the compaction another bish is doing, or
/// refuses to be a second daemon of the same name. The lock goes when the
/// last descriptor closes, so it stands for "still alive" across a crash.
/// `LOCK_EX`, `LOCK_NB`, `LOCK_UN` are 2, 4, 8 everywhere bish runs.
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

/// C's `struct tm`: POSIX's nine fields plus the `tm_gmtoff`/`tm_zone`
/// extension glibc and the BSDs both have, in the same order, so one
/// layout serves. It must match in size, since `localtime_r` writes a
/// whole one.
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

/// `epoch_seconds` in the local calendar, with the C library's offset and
/// abbreviation: where bish's dates come from.
pub(crate) fn local_time_at(epoch_seconds: i64) -> CTm {
    let mut broken_down = CTm::default();
    unsafe { localtime_r(&epoch_seconds as *const i64, &mut broken_down as *mut CTm) };
    broken_down
}

/// Re-reads `TZ`, which the C library caches. Only tests pinning a date to
/// UTC change `TZ`.
#[cfg(test)]
pub(crate) fn reload_timezone() {
    unsafe { tzset() };
}

/// One resource limit: the current value and the most it may be raised
/// to. `rlim_t` is 64-bit on both OSes; resource numbers and
/// `RLIM_INFINITY` differ (the tables).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct ResourceLimit {
    pub(crate) soft: u64,
    pub(crate) hard: u64,
}

/// "No limit", as this OS spells it.
pub(crate) const UNLIMITED: u64 = sys::RLIM_INFINITY;

/// The `ulimit` flags this OS has limits for, and their numbers. `None`:
/// `ulimit` leaves it out of `-a` and refuses to set it, as bash there
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

/// How much stack this process was given, or `None` with no limit.
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

/// CPU used so far in seconds, this shell's and its reaped children's.
/// Seconds, since the divisor is the OS's: `sysconf`'s tick-rate name is
/// 2 on Linux and 3 on macOS, and the wrong one asks something else.
pub(crate) fn cpu_seconds() -> Option<(f64, f64, f64, f64)> {
    let mut spent = Tms::default();
    if unsafe { times(&mut spent as *mut Tms) } == -1 {
        return None;
    }
    let ticks = unsafe { sysconf(sys::SC_CLK_TCK) }.max(1) as f64;
    Some((spent.utime as f64 / ticks, spent.stime as f64 / ticks, spent.cutime as f64 / ticks, spent.cstime as f64 / ticks))
}

/// The current file-creation mask. POSIX has no read-only query
/// (`umask(new)` returns the old one), so this sets it back at once.
pub(crate) fn current_umask() -> u32 {
    let current = unsafe { umask(0) };
    unsafe { umask(current) };
    current
}

/// Sets the file-creation mask, giving back what it was.
pub(crate) fn set_umask(mask: u32) -> u32 {
    unsafe { umask(mask) }
}

/// One descriptor to wait on and what the OS reported. `struct pollfd`
/// and its flags are the same on both OSes; `poll`'s count width differs
/// (the tables).
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

/// Waits until one of `fds` is ready or `timeout_ms` passes (negative
/// waits forever). `Ok(0)` for a timeout or a signal: the same to every
/// caller, since a signal worth acting on arrives through a descriptor in
/// the set.
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
/// handler to write a byte to. Non-blocking (a handler mustn't block, and
/// the reader stops at empty) and close-on-exec (no child should hold
/// either).
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

/// Writes one byte to a wake pipe. Called from real signal handlers, so
/// exactly one `write(2)`, async-signal-safe per POSIX: no allocation,
/// error formatting or locks. A full pipe already holds a wake-up.
pub(crate) fn wake(fd: std::os::unix::io::RawFd) {
    unsafe { write(fd, [0u8].as_ptr().cast(), 1) };
}

/// Closes a raw descriptor this layer handed out.
pub(crate) fn close_fd(fd: std::os::unix::io::RawFd) {
    unsafe { close(fd) };
}

/// Points `to` at what `from` refers to, as a child does on its way to
/// being a pipeline stage. The new descriptor isn't close-on-exec, which
/// is how something is handed to a child deliberately.
pub(crate) fn duplicate_onto(from: std::os::unix::io::RawFd, to: std::os::unix::io::RawFd) -> bool {
    unsafe { dup2(from, to) >= 0 }
}

/// Installs `handler` for `signal_number`; only an async-signal-safe one
/// (see `wake`).
pub(crate) fn on_signal(signal_number: i32, handler: extern "C" fn(i32)) {
    unsafe { signal(signal_number, handler as *const () as usize) };
}

/// A second descriptor for `fd` at the lowest free number. `None` for a
/// closed or invalid one, which callers treat as nothing to save.
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

/// A close-on-exec duplicate of `fd` numbered at or above `floor`, out of
/// the way of redirects. Close-on-exec because the copy is only ever a
/// later `dup2`'s source. The command is 1030 on Linux, 67 on macOS.
pub(crate) fn duplicate_above(fd: std::os::unix::io::RawFd, floor: i32) -> Option<std::os::unix::io::RawFd> {
    match unsafe { fcntl(fd, sys::F_DUPFD_CLOEXEC, floor) } {
        copy if copy < 0 => None,
        copy => Some(copy),
    }
}

/// Whether `fd` is open: reading an unopened descriptor fails with
/// EBADF, which looks like no input, and `read -u 9` must tell them
/// apart.
pub(crate) fn is_open(fd: std::os::unix::io::RawFd) -> bool {
    unsafe { fcntl(fd, sys::F_GETFD, 0) != -1 }
}

/// Makes `fd` non-blocking, returning its old flags to restore; `None`
/// when they couldn't be read, with nothing changed.
pub(crate) fn make_nonblocking(fd: std::os::unix::io::RawFd) -> Option<i32> {
    let flags = unsafe { fcntl(fd, sys::F_GETFL, 0) };
    if flags < 0 {
        return None;
    }
    unsafe { fcntl(fd, sys::F_SETFL, flags | sys::O_NONBLOCK) };
    Some(flags)
}

/// Restores flags from `make_nonblocking`.
pub(crate) fn restore_flags(fd: std::os::unix::io::RawFd, flags: i32) {
    unsafe { fcntl(fd, sys::F_SETFL, flags) };
}

/// Writes what it can, once, retrying nothing: `Ok(0)`, `WouldBlock` and
/// `Interrupted` mean different things to a pipeline stage (park, drain
/// the pane, go round again), and only the caller knows which.
pub(crate) fn write_bytes(fd: std::os::unix::io::RawFd, bytes: &[u8]) -> std::io::Result<usize> {
    let written = unsafe { write(fd, bytes.as_ptr().cast(), bytes.len()) };
    match written {
        n if n >= 0 => Ok(n as usize),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Clears close-on-exec so a child inherits `fd`. Needed even after a
/// `dup2`, which clears it only on a new descriptor: `dup2(fd, fd)` is a
/// no-op, so a redirect source already on its target number would be
/// closed by the exec.
pub(crate) fn clear_cloexec(fd: std::os::unix::io::RawFd) {
    unsafe { fcntl(fd, sys::F_SETFD, 0) };
}

/// The process that started this one: `$PPID`.
pub(crate) fn parent_process_id() -> i32 {
    unsafe { getppid() }
}

/// Who started this process, not who it acts as: `$UID` against `$EUID`.
pub(crate) fn real_user() -> u32 {
    unsafe { getuid() }
}

/// The device name of the terminal on `fd`, if any; `None` for a
/// non-terminal or a name too long.
pub(crate) fn terminal_name(fd: std::os::unix::io::RawFd) -> Option<String> {
    let mut buf = [0u8; 256];
    if unsafe { ttyname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// This machine's name, from `gethostname(2)` (macOS has no
/// `/proc/sys/kernel/hostname`).
pub(crate) fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    if unsafe { gethostname(buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

// `signal(2)`'s two special handlers as numbers, as the C declaration
// takes them; the same on every Unix.
const HANDLER_DEFAULT: usize = 0;
const HANDLER_IGNORE: usize = 1;

/// Installs `handler` for `signal_number` through `sigaction`, which
/// defines what the handler inherits and reports the old disposition.
/// Only an async-signal-safe handler: bish's set a flag and return.
pub(crate) fn handle_signal(signal_number: i32, handler: extern "C" fn(i32)) {
    set_disposition(signal_number, handler as *const () as usize);
}

/// Puts `signal_number` back to what the OS would do with it.
pub(crate) fn default_signal(signal_number: i32) {
    set_disposition(signal_number, HANDLER_DEFAULT);
}

/// Makes this process ignore `signal_number`, inherited by forked
/// children and kept across `exec` (POSIX resets real handlers but keeps
/// "ignore"), so every real spawn restores it with `default_signal`, or a
/// job won't answer Ctrl-C.
pub(crate) fn ignore_signal(signal_number: i32) {
    set_disposition(signal_number, HANDLER_IGNORE);
}

fn set_disposition(signal_number: i32, handler: usize) {
    let action = sys::SigAction { handler, ..sys::SigAction::default() };
    unsafe { sigaction(signal_number, &action, std::ptr::null_mut()) };
}

/// Whether `signal_number` is ignored now. POSIX: a signal ignored on
/// entry to a shell can't be trapped or reset, being immunity granted
/// from outside (`nohup`, a backgrounded script's shield from Ctrl-C).
pub(crate) fn is_signal_ignored(signal_number: i32) -> bool {
    let mut previous = sys::SigAction::default();
    let asked = unsafe { sigaction(signal_number, std::ptr::null(), &mut previous) } == 0;
    asked && previous.handler == HANDLER_IGNORE
}

/// Sends `signal_number` to one process.
pub(crate) fn send_signal(pid: i32, signal_number: i32) -> bool {
    unsafe { kill(pid, signal_number) == 0 }
}

/// Sends `signal_number` to a whole process group, as the terminal driver
/// does for Ctrl-C.
pub(crate) fn send_signal_to_group(pgid: i32, signal_number: i32) -> bool {
    unsafe { kill(-pgid, signal_number) == 0 }
}

/// Puts `pid` in process group `pgid`, half of job control;
/// `set_foreground_pgrp` is the other.
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
    /// Killed by a signal: the number, not bash's 128 + n.
    Killed(i32),
    /// Stopped and still there, which `std::process::Child` can't observe:
    /// a Ctrl-Z'd child would look like it's running forever.
    Stopped(i32),
    /// Nothing changed yet (only when not waiting).
    Running,
    /// Nothing to wait for: reaped, never ours, gone.
    Gone,
}

/// Waits for one child, reporting a stop as well as an exit; `block`
/// false asks without waiting, and an interrupted wait is retried. The
/// status encoding (low seven bits a signal, 0x7f stopped, the next eight
/// the code) is the same on every Unix bish targets.
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

/// Creates `path` with exactly `mode`, whatever the umask: `create_dir`
/// uses 0o777 minus the umask, and a 0700 directory holding a private
/// socket mustn't depend on that. `AlreadyExists` is the caller's to
/// judge: something being there isn't the directory meant being there.
pub(crate) fn create_dir_with_mode(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    match unsafe { mkdir(c_path.as_ptr(), mode) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// `path` and its missing parents, created `mode`, for a private config
/// directory.
pub(crate) fn create_dir_all_with_mode(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(mode).create(path)
}

/// A fresh file only its owner can read or write (0600); `append` keeps
/// the contents, otherwise truncated. bish's one way of writing a private
/// file (history, the trust store), so a target without permission bits
/// (`web.rs`) just opens it.
pub(crate) fn create_private_file(path: &std::path::Path, append: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).mode(0o600);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    options.open(path)
}

/// This process's id, here because `std::process::id` aborts on a target
/// with no processes (`web.rs`), which answers a fixed value.
pub(crate) fn process_id() -> u32 {
    std::process::id()
}

/// The signal that killed a process, if one did. `ExitStatusExt` is
/// Unix-only; `web.rs` always answers `None`.
pub(crate) fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

/// Forces `path` to exactly `mode`; `web.rs`, with no bits, does nothing.
pub(crate) fn set_file_mode(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Whether a mode has an execute bit; `web.rs`, with no notion of an
/// executable, answers `false`.
pub(crate) fn has_execute_bit(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

/// The special file kinds `test` asks about (`-p -S -b -c`), here since
/// `FileTypeExt` is Unix-only; `web.rs` answers `false`, right where they
/// can't exist.
pub(crate) fn is_fifo(file_type: &std::fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    file_type.is_fifo()
}

pub(crate) fn is_socket(file_type: &std::fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    file_type.is_socket()
}

pub(crate) fn is_block_device(file_type: &std::fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    file_type.is_block_device()
}

pub(crate) fn is_char_device(file_type: &std::fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    file_type.is_char_device()
}

/// Takes group and other off an existing file, if set. Failure is
/// ignored: a filesystem that can't represent the mode still has the
/// file, and `web.rs`, without modes, does nothing.
pub(crate) fn restrict_to_owner(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else { return };
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o700));
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    // What `darwin.rs`'s `anonymous_file` is, on a machine that isn't a
    // Mac: a readable, writable file with no name.
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

        // Nothing of it is in the directory, even before the descriptor
        // drops.
        let left: Vec<std::path::PathBuf> = std::fs::read_dir(std::env::temp_dir())
            .expect("the temp directory is readable")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains("bish-capture-test-"))
            .collect();
        assert!(left.is_empty(), "the name should be gone while the fd is still open: {left:?}");
    }

    // `DirSnapshot` is what macOS must work out itself (kqueue doesn't say
    // which name changed), checked here on a machine that isn't a Mac.
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
        // A rename over it with content of the same length: only the inode
        // says anything happened.
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
