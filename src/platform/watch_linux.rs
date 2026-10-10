//! Watching a directory with inotify(7): one descriptor for everything,
//! and each event already names the directory and entry. `watch.rs` owns
//! the policy above this.

use super::{RawChange, RawEvent, WatchId};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::Path;

unsafe extern "C" {
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, pathname: *const u8, mask: u32) -> i32;
    fn inotify_rm_watch(fd: i32, wd: i32) -> i32;
    fn read(fd: i32, buf: *mut std::ffi::c_void, count: usize) -> isize;
    fn close(fd: i32) -> i32;
}

// `inotify_init1` flags, the `open(2)` ones renamed; private, as for any
// call only this OS has.
const IN_NONBLOCK: i32 = 0o4000;
const IN_CLOEXEC: i32 = 0o2000000;

// The events wanted. `IN_CLOSE_WRITE` rather than `IN_MODIFY` for content:
// a chunked write gives a `MODIFY` per chunk, which would mean reading a
// file mid-write; `CLOSE_WRITE` comes once, after the writer is done.
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

    /// Watches `dir`; the same directory twice gives the same id, the
    /// kernel's behaviour and the macOS side's contract. `file` isn't needed
    /// here: a directory watch reports writes to every file by name, unlike
    /// kqueue (see watch_darwin.rs).
    pub(crate) fn add(&mut self, dir: &Path, _file: Option<&std::ffi::OsStr>) -> io::Result<WatchId> {
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

    /// Everything waiting now. Non-blocking, so empty means nothing yet.
    pub(crate) fn drain(&mut self) -> Vec<RawEvent> {
        // The buffer must hold a whole event with its name, or the read
        // fails with EINVAL. inotify's documented minimum is
        // `sizeof(struct inotify_event) + NAME_MAX + 1`; this is well past
        // it and reads many per call.
        let mut buf = [0u8; 8192];
        let mut out: Vec<RawEvent> = Vec::new();
        loop {
            let n = unsafe { read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut at = 0usize;
            while at + 16 <= n as usize {
                let wd = i32::from_ne_bytes(buf[at..at + 4].try_into().expect("four bytes"));
                let mask = u32::from_ne_bytes(buf[at + 4..at + 8].try_into().expect("four bytes"));
                let len = u32::from_ne_bytes(buf[at + 12..at + 16].try_into().expect("four bytes")) as usize;
                let name_bytes = &buf[at + 16..(at + 16 + len).min(buf.len())];
                // The name is NUL-padded to alignment, so the first NUL
                // ends it.
                let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len());
                let name = std::ffi::OsStr::from_bytes(&name_bytes[..name_end]);
                at += 16 + len;

                let change = match mask {
                    _ if mask & IN_Q_OVERFLOW != 0 => RawChange::Overflowed,
                    // The watch went away: the directory was deleted or
                    // moved.
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
