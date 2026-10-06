//! Watching a directory with inotify(7).
//!
//! One descriptor for the whole set, and the kernel names the entry that
//! changed, so there is nothing to work out: an event arrives already
//! saying which directory and which name in it. `watch.rs` owns the
//! policy above this -- which names a caller asked about, and what to
//! make of an event about one.

use super::{RawChange, RawEvent, WatchId};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::Path;

unsafe extern "C" {
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, pathname: *const u8, mask: u32) -> i32;
    fn inotify_rm_watch(fd: i32, wd: i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn close(fd: i32) -> i32;
}

// `inotify_init1` flags, which are the `open(2)` ones under different
// names. Private, like every constant belonging to a call only this OS
// has.
const IN_NONBLOCK: i32 = 0o4000;
const IN_CLOEXEC: i32 = 0o2000000;

// The events worth asking for. `IN_CLOSE_WRITE` rather than `IN_MODIFY`
// for content: a program writing a file in chunks produces a `MODIFY`
// per chunk, so acting on those means reading a file mid-write, while
// `CLOSE_WRITE` arrives once and after the writer let go.
const IN_ATTRIB: u32 = 0x0000_0004;
const IN_CLOSE_WRITE: u32 = 0x0000_0008;
const IN_MOVED_FROM: u32 = 0x0000_0040;
const IN_MOVED_TO: u32 = 0x0000_0080;
const IN_CREATE: u32 = 0x0000_0100;
const IN_DELETE: u32 = 0x0000_0200;
const IN_DELETE_SELF: u32 = 0x0000_0400;
const IN_MOVE_SELF: u32 = 0x0000_0800;
const IN_Q_OVERFLOW: u32 = 0x0000_4000;
const IN_IGNORED: u32 = 0x0000_8000;

const WATCH_MASK: u32 = IN_ATTRIB | IN_CLOSE_WRITE | IN_MOVED_FROM | IN_MOVED_TO | IN_CREATE | IN_DELETE | IN_DELETE_SELF | IN_MOVE_SELF;

/// Every directory being watched, behind one pollable descriptor.
pub(crate) struct DirWatch {
    fd: RawFd,
}

impl DirWatch {
    pub(crate) fn new() -> io::Result<DirWatch> {
        let fd = unsafe { inotify_init1(IN_NONBLOCK | IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(DirWatch { fd })
    }

    /// Readable exactly when there is at least one event waiting.
    pub(crate) fn fd(&self) -> RawFd {
        self.fd
    }

    /// Watches `dir`. The same directory added twice gives the same id,
    /// which is the kernel's own behaviour here and the contract the
    /// macOS side has to keep too.
    pub(crate) fn add(&mut self, dir: &Path) -> io::Result<WatchId> {
        let mut c_path: Vec<u8> = dir.as_os_str().as_bytes().to_vec();
        c_path.push(0);
        let wd = unsafe { inotify_add_watch(self.fd, c_path.as_ptr(), WATCH_MASK) };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(wd)
    }

    pub(crate) fn remove(&mut self, watch: WatchId) {
        unsafe { inotify_rm_watch(self.fd, watch) };
    }

    /// Everything waiting, right now. Never blocks: the descriptor is
    /// non-blocking, so an empty answer means nothing has happened, not
    /// that nothing will.
    pub(crate) fn drain(&mut self) -> Vec<RawEvent> {
        // The buffer has to hold at least one whole event, name
        // included, or the read fails with EINVAL rather than returning
        // a partial one. inotify's own documented minimum is
        // `sizeof(struct inotify_event) + NAME_MAX + 1`; this is
        // comfortably past it and reads many events per call.
        let mut buf = [0u8; 8192];
        let mut out: Vec<RawEvent> = Vec::new();
        loop {
            let n = unsafe { read(self.fd, buf.as_mut_ptr(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut at = 0usize;
            while at + 16 <= n as usize {
                let wd = i32::from_ne_bytes(buf[at..at + 4].try_into().expect("four bytes"));
                let mask = u32::from_ne_bytes(buf[at + 4..at + 8].try_into().expect("four bytes"));
                let len = u32::from_ne_bytes(buf[at + 12..at + 16].try_into().expect("four bytes")) as usize;
                let name_bytes = &buf[at + 16..(at + 16 + len).min(buf.len())];
                // The name is NUL-padded to an alignment boundary, so
                // the first NUL ends it.
                let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len());
                let name = std::ffi::OsStr::from_bytes(&name_bytes[..name_end]);
                at += 16 + len;

                let change = match mask {
                    _ if mask & IN_Q_OVERFLOW != 0 => RawChange::Overflowed,
                    // The watch itself went away: the directory was
                    // deleted or moved, and the descriptor is dead.
                    _ if mask & IN_IGNORED != 0 => RawChange::Dropped,
                    _ if mask & (IN_DELETE | IN_MOVED_FROM | IN_DELETE_SELF | IN_MOVE_SELF) != 0 => RawChange::Gone,
                    _ => RawChange::Touched,
                };
                let named = (!name.is_empty()).then(|| name.to_os_string());
                out.push(RawEvent { watch: wd, name: named, change });
            }
        }
        out
    }
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        unsafe { close(self.fd) };
    }
}
