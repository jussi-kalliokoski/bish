//! How bish's one OS-facing surface is satisfied on a target that is not
//! a Unix at all.
//!
//! `mod.rs` names this case directly: "a target that is not a Unix at
//! all (a browser, an embedded runtime) replaces `unix.rs`, and this
//! surface is the contract it has to satisfy." This is that file, for
//! WASI preview 1 -- the shape a browser build runs through.
//!
//! The capabilities split three ways:
//!
//!   * Real. Files, time, descriptor reads and writes, `access`-style
//!     questions: WASI has these, so they are done with `std` and work.
//!   * Absent but harmless. Signals, process groups, resource limits,
//!     terminal modes: a shell with no child processes and no
//!     controlling terminal never needs them to *do* anything, so they
//!     are no-ops or neutral answers that keep the callers above
//!     running.
//!   * Absent and refused. Ptys, a pipe to a child, a stack to switch
//!     onto: these return an error a caller can report. Nothing on this
//!     platform can spawn the thing that would use them.
//!
//! Everything here is `std`-only; there is not a single `extern "C"`,
//! which is why `mod.rs`'s `os_guard` has nothing to say about it.
#![allow(dead_code)]

use std::fs::File;
use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
use std::path::{Path, PathBuf};

fn unsupported<T>(what: &str) -> std::io::Result<T> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, format!("{what} is not available on this platform")))
}

/// Borrows an fd as a `File` without owning it: the `ManuallyDrop` is
/// what keeps the descriptor open when the borrow ends, so a read or a
/// write through it does not close stdin out from under the shell.
fn borrow_fd(fd: RawFd) -> ManuallyDrop<File> {
    // SAFETY: we never drop it, so no double-close; the fd outlives the
    // borrow by construction.
    ManuallyDrop::new(unsafe { File::from_raw_fd(fd) })
}

// ---------------------------------------------------------------------
// Coroutine stacks. There is no second stack to switch onto here, so the
// coroutine machinery that would call these is itself stubbed out (see
// `coroutine.rs`); these exist only to satisfy the contract.
// ---------------------------------------------------------------------

pub(crate) fn map_guarded_stack(_guard: usize, _usable: usize) -> std::io::Result<*mut u8> {
    unsupported("a guarded stack")
}

pub(crate) unsafe fn unmap(_base: *mut u8, _len: usize) {}

// ---------------------------------------------------------------------
// Temp files. WASI can create files in a preopened directory, so these
// are real -- `None` only when there is no writable directory to use,
// which the callers already handle.
// ---------------------------------------------------------------------

pub(crate) fn unlinked_temp_file(tag: &str) -> Option<File> {
    exclusive_temp_file(tag).map(|(file, path)| {
        // Best effort: on a filesystem that allows it this makes the
        // file nameless as the Unix side does; where it does not, the
        // named file still works.
        let _ = std::fs::remove_file(&path);
        file
    })
}

pub(crate) fn exclusive_temp_file(tag: &str) -> Option<(File, PathBuf)> {
    let dir = std::env::temp_dir();
    for attempt in 0..8u32 {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(attempt as u128);
        let path = dir.join(format!("bish-{tag}-{stamp}-{attempt}"));
        match std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
            Ok(file) => return Some((file, path)),
            Err(_) => continue,
        }
    }
    None
}

// ---------------------------------------------------------------------
// Directory snapshots: the poll-based change detection `watch.rs` falls
// back to. Built from `std` metadata -- no inode here, so a rename that
// keeps size and mtime would be missed, which is the one case the Unix
// inode guards against and the price of having no kernel to ask.
// ---------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct DirSnapshot {
    entries: std::collections::HashMap<std::ffi::OsString, Stamp>,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryChange {
    Touched,
    Gone,
}

impl DirSnapshot {
    pub(crate) fn of(dir: &Path) -> DirSnapshot {
        let mut entries = std::collections::HashMap::new();
        let Ok(listing) = std::fs::read_dir(dir) else { return DirSnapshot::default() };
        for entry in listing.flatten() {
            let Ok(meta) = entry.path().symlink_metadata() else { continue };
            entries.insert(entry.file_name(), Stamp { size: meta.len(), modified: meta.modified().ok() });
        }
        DirSnapshot { entries }
    }

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

// ---------------------------------------------------------------------
// Terminal modes. There is no controlling terminal; the type stays
// opaque (as on Unix) but carries nothing, and the mode transforms are
// identities so a caller that saves and restores a mode is harmless.
// ---------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(crate) struct TerminalMode(());

pub(crate) fn terminal_mode(_fd: i32) -> Option<TerminalMode> {
    None
}

pub(crate) fn set_terminal_mode(_fd: i32, _mode: &TerminalMode) -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn is_canonical(_mode: &TerminalMode) -> bool {
    true
}

pub(crate) fn raw_mode(from: &TerminalMode) -> TerminalMode {
    *from
}

pub(crate) fn cooked_mode(from: &TerminalMode) -> TerminalMode {
    *from
}

pub(crate) fn echoless_mode(from: &TerminalMode) -> TerminalMode {
    *from
}

// ---------------------------------------------------------------------
// Descriptor reads and writes. Real: WASI descriptors read and write.
// ---------------------------------------------------------------------

pub(crate) fn raise_signal(_signal_number: i32) {}

pub(crate) fn read_bytes(fd: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    borrow_fd(fd).read(buf)
}

pub(crate) fn write_bytes(fd: RawFd, bytes: &[u8]) -> std::io::Result<usize> {
    borrow_fd(fd).write(bytes)
}

// ---------------------------------------------------------------------
// Ptys and window size. No controlling terminal here.
// ---------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Winsize {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) xpixel: u16,
    pub(crate) ypixel: u16,
}

pub(crate) fn open_pty() -> std::io::Result<(File, String)> {
    unsupported("a pseudo-terminal")
}

pub(crate) fn attach_to_pty_slave(_slave_path: &std::ffi::CStr) -> std::io::Result<()> {
    unsupported("a pseudo-terminal")
}

pub(crate) fn terminal_size(_fd: RawFd) -> std::io::Result<Winsize> {
    // A sensible default rather than an error: callers use this to lay
    // out output, and 80x24 is the conventional fallback everywhere.
    Ok(Winsize { rows: 24, cols: 80, xpixel: 0, ypixel: 0 })
}

pub(crate) fn set_terminal_size(_fd: RawFd, _rows: u16, _cols: u16) -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn set_foreground_pgrp(_fd: RawFd, _pgrp: i32) -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn set_nonblocking(_fd: RawFd) {}

// ---------------------------------------------------------------------
// Access, identity. `access`-style questions are answerable from std
// metadata; identity is a single fixed user, since WASI has no uids.
// ---------------------------------------------------------------------

pub(crate) enum Access {
    Read,
    Write,
    Execute,
}

pub(crate) fn is_accessible(path: &str, mode: Access) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => match mode {
            // Nothing on WASI distinguishes read from execute at this
            // level; existence is the honest answer for both.
            Access::Read | Access::Execute => true,
            Access::Write => !meta.permissions().readonly(),
        },
        Err(_) => false,
    }
}

/// The one user there is. WASI has no uids; bish's ownership checks all
/// compare against this, so a fixed value makes "owned by me" uniformly
/// true, which is the truth in a single-user sandbox.
pub(crate) fn effective_user() -> u32 {
    1000
}

pub(crate) fn effective_group() -> u32 {
    1000
}

pub(crate) fn real_user() -> u32 {
    1000
}

/// The owner of a file, for the trust store's ownership check. There
/// are no owners here, so everything reads as owned by the one user.
pub(crate) fn file_owner(_metadata: &std::fs::Metadata) -> u32 {
    effective_user()
}

pub(crate) fn is_terminal(_fd: RawFd) -> bool {
    false
}

// ---------------------------------------------------------------------
// File locking. Single process, so a lock is uncontended by definition.
// ---------------------------------------------------------------------

pub(crate) fn lock_exclusive(_fd: RawFd) -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn unlock(_fd: RawFd) {}

// ---------------------------------------------------------------------
// Time. `epoch_seconds` is real; `local_time_at` is UTC, there being no
// timezone database in the sandbox.
// ---------------------------------------------------------------------

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

pub(crate) fn epoch_seconds() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `epoch_seconds` broken into the UTC calendar, by the civil-from-days
/// algorithm (Howard Hinnant's) -- self-contained arithmetic, no C
/// library and no timezone, which is all a sandbox has to offer.
pub(crate) fn local_time_at(epoch_seconds: i64) -> CTm {
    let days = epoch_seconds.div_euclid(86_400);
    let secs = epoch_seconds.rem_euclid(86_400);

    let (tm_hour, tm_min, tm_sec) = ((secs / 3600) as i32, ((secs % 3600) / 60) as i32, (secs % 60) as i32);

    // Days since 1970-01-01 to civil date.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], Mar=0
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    // Day of week: 1970-01-01 was a Thursday (4).
    let tm_wday = (days.rem_euclid(7) + 4).rem_euclid(7) as i32;

    // Day of year, with Jan 1 == 0.
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let cumulative = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let mut tm_yday = cumulative[(m - 1) as usize] + (d - 1);
    if leap && m > 2 {
        tm_yday += 1;
    }

    CTm {
        tm_sec,
        tm_min,
        tm_hour,
        tm_mday: d as i32,
        tm_mon: (m - 1) as i32,
        tm_year: (year - 1900) as i32,
        tm_wday,
        tm_yday: tm_yday as i32,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    }
}

#[cfg(test)]
pub(crate) fn reload_timezone() {}

// ---------------------------------------------------------------------
// Resource limits. None here; everything is unlimited, and no `ulimit`
// flag maps to a real resource.
// ---------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct ResourceLimit {
    pub(crate) soft: u64,
    pub(crate) hard: u64,
}

pub(crate) const UNLIMITED: u64 = u64::MAX;

pub(crate) fn resource_for_flag(_flag: char) -> Option<i32> {
    None
}

pub(crate) fn resource_limit(_resource: i32) -> ResourceLimit {
    ResourceLimit { soft: UNLIMITED, hard: UNLIMITED }
}

pub(crate) fn set_resource_limit(_resource: i32, _limit: &ResourceLimit) -> std::io::Result<()> {
    unsupported("setting a resource limit")
}

pub(crate) fn stack_limit() -> Option<u64> {
    None
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Tms {
    pub(crate) utime: i64,
    pub(crate) stime: i64,
    pub(crate) cutime: i64,
    pub(crate) cstime: i64,
}

pub(crate) fn cpu_seconds() -> Option<(f64, f64, f64, f64)> {
    None
}

// ---------------------------------------------------------------------
// umask. WASI has no umask; report the conventional default and accept
// a set without doing anything.
// ---------------------------------------------------------------------

pub(crate) fn current_umask() -> u32 {
    0o022
}

pub(crate) fn set_umask(_mask: u32) -> u32 {
    0o022
}

// ---------------------------------------------------------------------
// Polling. The event loop asks this to sleep until a descriptor is
// ready; with nothing async to wait on, every wait is a timeout.
// ---------------------------------------------------------------------

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
    pub(crate) fn watching(fd: RawFd, events: i16) -> PollFd {
        PollFd { fd, events, revents: 0 }
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.fd
    }

    pub(crate) fn reported(&self) -> i16 {
        self.revents
    }

    pub(crate) fn clear(&mut self) {
        self.revents = 0;
    }
}

pub(crate) fn wait_for_ready(_fds: &mut [PollFd], _timeout_ms: i32) -> std::io::Result<usize> {
    Ok(0)
}

pub(crate) fn wake_pipe() -> std::io::Result<(RawFd, RawFd)> {
    unsupported("a wake pipe")
}

pub(crate) fn wake(_fd: RawFd) {}

// ---------------------------------------------------------------------
// Descriptor plumbing. Close and duplicate are real (std does them);
// the nonblocking/cloexec flag twiddling is a no-op the callers tolerate.
// ---------------------------------------------------------------------

pub(crate) fn close_fd(fd: RawFd) {
    if fd >= 0 {
        // SAFETY: taking ownership to drop it, which closes it once.
        drop(unsafe { File::from_raw_fd(fd) });
    }
}

pub(crate) fn duplicate_onto(_from: RawFd, _to: RawFd) -> bool {
    false
}

pub(crate) fn on_signal(_signal_number: i32, _handler: extern "C" fn(i32)) {}

pub(crate) fn duplicate(fd: RawFd) -> Option<RawFd> {
    duplicate_as_file(fd).map(|file| file.into_raw_fd())
}

pub(crate) fn duplicate_as_file(fd: RawFd) -> Option<File> {
    borrow_fd(fd).try_clone().ok()
}

pub(crate) fn duplicate_above(fd: RawFd, _floor: i32) -> Option<RawFd> {
    duplicate(fd)
}

pub(crate) fn is_open(fd: RawFd) -> bool {
    fd >= 0
}

pub(crate) fn make_nonblocking(_fd: RawFd) -> Option<i32> {
    None
}

pub(crate) fn restore_flags(_fd: RawFd, _flags: i32) {}

pub(crate) fn clear_cloexec(_fd: RawFd) {}

// ---------------------------------------------------------------------
// Process identity and signals. No processes, no signals: neutral
// answers all the way down.
// ---------------------------------------------------------------------

pub(crate) fn parent_process_id() -> i32 {
    0
}

pub(crate) fn terminal_name(_fd: RawFd) -> Option<String> {
    None
}

pub(crate) fn hostname() -> Option<String> {
    None
}

pub(crate) fn handle_signal(_signal_number: i32, _handler: extern "C" fn(i32)) {}

pub(crate) fn default_signal(_signal_number: i32) {}

pub(crate) fn ignore_signal(_signal_number: i32) {}

pub(crate) fn is_signal_ignored(_signal_number: i32) -> bool {
    false
}

pub(crate) fn send_signal(_pid: i32, _signal_number: i32) -> bool {
    false
}

pub(crate) fn send_signal_to_group(_pgid: i32, _signal_number: i32) -> bool {
    false
}

pub(crate) fn set_process_group(_pid: i32, _pgid: i32) -> bool {
    false
}

pub(crate) fn process_group() -> i32 {
    0
}

// ---------------------------------------------------------------------
// Children. Nothing can spawn one, so there is never anything to wait
// for.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Waited {
    Exited(i32),
    Killed(i32),
    Stopped(i32),
    Running,
    Gone,
}

pub(crate) fn wait_for_child(_pid: i32, _block: bool) -> Waited {
    Waited::Gone
}

// ---------------------------------------------------------------------
// Directories. `create_dir_with_mode` ignores the mode -- WASI has no
// permission bits -- but still creates the directory.
// ---------------------------------------------------------------------

pub(crate) fn create_dir_with_mode(path: &Path, _mode: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

pub(crate) fn create_dir_all_with_mode(path: &Path, _mode: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// A file for writing. There are no permission bits to set here, so it
/// is simply opened; the privacy the mode buys on Unix is the sandbox's
/// own problem, not something bish can enforce.
pub(crate) fn create_private_file(path: &Path, append: bool) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    options.open(path)
}

/// No permission bits to set here.
pub(crate) fn set_file_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Nothing can be signalled here, so nothing was killed by one.
pub(crate) fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// There is one process and it is this one; `std::process::id` aborts on
/// WASI, so bish's `$$`/`$BASHPID` get a fixed, stable value instead.
pub(crate) fn process_id() -> u32 {
    1
}

/// No permission bits here, so nothing reads as executable.
pub(crate) fn has_execute_bit(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// None of these file kinds exist in the sandbox, so `test` answers
/// `false` for all of them -- which is the truth here.
pub(crate) fn is_fifo(_file_type: &std::fs::FileType) -> bool {
    false
}

pub(crate) fn is_socket(_file_type: &std::fs::FileType) -> bool {
    false
}

pub(crate) fn is_block_device(_file_type: &std::fs::FileType) -> bool {
    false
}

pub(crate) fn is_char_device(_file_type: &std::fs::FileType) -> bool {
    false
}

/// Nothing to restrict: no modes on this platform.
pub(crate) fn restrict_to_owner(_path: &Path) {}
