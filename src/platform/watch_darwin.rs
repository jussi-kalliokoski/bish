//! Watching a directory with kqueue(2). kqueue reports on an open
//! descriptor, and for a directory says only that something changed,
//! never which name. FSEvents is a per-volume log delivered to a
//! run-loop callback through CoreServices, seconds late; kqueue is an
//! ordinary descriptor that goes in a `PollSet` like everything else bish
//! waits on.
//!
//! So each watch keeps a `DirSnapshot` of its directory, and an event
//! rescans and compares to get the names: a `read_dir` per event, not per
//! poll.
//!
//! A directory's events fire when an entry is added, removed or renamed,
//! not when a file in it is written in place, as most saves are. So a
//! named file is watched through a descriptor of its own too, and an
//! event on it means the same rescan. That descriptor follows the inode,
//! so after each rescan a named file with a new inode (renamed over,
//! deleted and recreated) gets a fresh one, and a newly appeared one its
//! first.
//!
//! Two real differences:
//!
//! - One descriptor per watched directory and per named file, where
//!   inotify has one for all. bish watches one file at a time, so it's a
//!   handful. An unnamed file in a watched directory is reported when it
//!   appears, goes or is replaced, but not when written in place:
//!   watching every file would be a descriptor each, without bound.
//! - Two writes within one snapshot interval leaving size, inode and
//!   mtime identical are invisible. bish's caller hashes the file before
//!   saying anything (`TextBuffer::changed_on_disk`), so the result is a
//!   missed notice, not a wrong one, and `O_EVTONLY` with APFS's nanosecond
//!   timestamps makes the window narrow.

use super::unix::{DirSnapshot, EntryChange, c_open};
use super::{RawChange, RawEvent, WatchId};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

unsafe extern "C" {
    fn kqueue() -> i32;
    fn kevent(kq: i32, changes: *const KEvent, nchanges: i32, events: *mut KEvent, nevents: i32, timeout: *const TimeSpec) -> i32;
    fn close(fd: i32) -> i32;
}

/// `struct kevent` on a 64-bit macOS target: 32 bytes, `ident` and `data`
/// pointer-wide, `udata` a pointer bish uses as a number.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KEvent {
    ident: u64,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: i64,
    udata: u64,
}

#[repr(C)]
struct TimeSpec {
    seconds: i64,
    nanoseconds: i64,
}

// kqueue's numbers, private: Linux has no counterpart to pair them with.
const EVFILT_VNODE: i16 = -4;
const EV_ADD: u16 = 0x0001;
const EV_CLEAR: u16 = 0x0020;
const EV_ERROR: u16 = 0x4000;

const NOTE_DELETE: u32 = 0x0000_0001;
const NOTE_WRITE: u32 = 0x0000_0002;
const NOTE_EXTEND: u32 = 0x0000_0004;
const NOTE_ATTRIB: u32 = 0x0000_0008;
const NOTE_LINK: u32 = 0x0000_0010;
const NOTE_RENAME: u32 = 0x0000_0020;
const NOTE_REVOKE: u32 = 0x0000_0040;

const WATCH_FFLAGS: u32 = NOTE_DELETE | NOTE_WRITE | NOTE_EXTEND | NOTE_ATTRIB | NOTE_LINK | NOTE_RENAME | NOTE_REVOKE;

// `open(2)` for events only. Unlike `O_RDONLY` on a directory, it doesn't
// keep the volume from being unmounted.
const O_EVTONLY: i32 = 0x8000;
const O_CLOEXEC: i32 = 0x0100_0000;

/// One watched directory: the descriptor kqueue reports on, and how the
/// directory looked last time.
struct Watched {
    dir: PathBuf,
    fd: RawFd,
    seen: DirSnapshot,
    /// Its named files, each with its watching descriptor and that
    /// descriptor's inode; `None` while the file doesn't exist.
    files: HashMap<OsString, Option<(RawFd, u64)>>,
}

pub(crate) struct DirWatch {
    kq: RawFd,
    watched: HashMap<WatchId, Watched>,
    next: WatchId,
}

impl DirWatch {
    pub(crate) fn new() -> io::Result<DirWatch> {
        let kq = unsafe { kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(DirWatch { kq, watched: HashMap::new(), next: 1 })
    }

    /// Readable when at least one event waits; a kqueue polls like any fd.
    pub(crate) fn fd(&self) -> RawFd {
        self.kq
    }

    /// Watches `dir`, returning the same id for a directory already watched,
    /// as inotify does, which callers rely on to merge two files in one
    /// directory. `file` is the entry the caller wants, watched separately,
    /// since writing it in place changes nothing about the directory.
    pub(crate) fn add(&mut self, dir: &Path, file: Option<&OsStr>) -> io::Result<WatchId> {
        let id = match self.watched.iter().find(|(_, w)| w.dir == dir) {
            Some((id, _)) => *id,
            None => {
                let fd = self.register(dir, self.next)?;
                let id = self.next;
                self.next += 1;
                self.watched.insert(id, Watched { dir: dir.to_path_buf(), fd, seen: DirSnapshot::of(dir), files: HashMap::new() });
                id
            }
        };
        if let Some(name) = file
            && let Some(watched) = self.watched.get_mut(&id)
        {
            watched.files.entry(name.to_os_string()).or_insert(None);
            self.refresh_files(id);
        }
        Ok(id)
    }

    /// Opens `path` for events only and registers it with the queue,
    /// reporting as `id`.
    fn register(&self, path: &Path, id: WatchId) -> io::Result<RawFd> {
        let mut c_path: Vec<u8> = path.as_os_str().as_bytes().to_vec();
        c_path.push(0);
        let fd = unsafe { c_open(c_path.as_ptr() as *const i8, O_EVTONLY | O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // `EV_CLEAR`, so an event is reported once, not on every read until
        // reset.
        let change = KEvent { ident: fd as u64, filter: EVFILT_VNODE, flags: EV_ADD | EV_CLEAR, fflags: WATCH_FFLAGS, data: 0, udata: id as u64 };
        let registered = unsafe { kevent(self.kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if registered < 0 {
            let failure = io::Error::last_os_error();
            unsafe { close(fd) };
            return Err(failure);
        }
        Ok(fd)
    }

    /// Points each named file's descriptor at what's under that name now:
    /// new for a file that appeared or was replaced, none for one gone.
    fn refresh_files(&mut self, id: WatchId) {
        use std::os::unix::fs::MetadataExt;
        let Some(watched) = self.watched.get(&id) else { return };
        let mut updates = Vec::new();
        for (name, current) in &watched.files {
            let path = watched.dir.join(name);
            let inode = std::fs::symlink_metadata(&path).ok().map(|m| m.ino());
            if inode == current.map(|(_, ino)| ino) {
                continue;
            }
            let fresh = match inode {
                Some(ino) => self.register(&path, id).ok().map(|fd| (fd, ino)),
                None => None,
            };
            updates.push((name.clone(), *current, fresh));
        }
        let Some(watched) = self.watched.get_mut(&id) else { return };
        for (name, stale, fresh) in updates {
            if let Some((fd, _)) = stale {
                unsafe { close(fd) };
            }
            watched.files.insert(name, fresh);
        }
    }

    /// Closing the descriptor deregisters it.
    pub(crate) fn remove(&mut self, watch: WatchId) {
        if let Some(gone) = self.watched.remove(&watch) {
            close_all(&gone);
        }
    }

    /// Everything waiting now; never blocks, a zero timeout making `kevent`
    /// a poll.
    pub(crate) fn drain(&mut self) -> Vec<RawEvent> {
        let mut out: Vec<RawEvent> = Vec::new();
        loop {
            let mut events = [KEvent::default(); 32];
            let nothing = TimeSpec { seconds: 0, nanoseconds: 0 };
            let n = unsafe { kevent(self.kq, std::ptr::null(), 0, events.as_mut_ptr(), events.len() as i32, &nothing) };
            if n <= 0 {
                break;
            }
            for event in &events[..n as usize] {
                if event.flags & EV_ERROR != 0 {
                    continue;
                }
                let id = event.udata as WatchId;
                let on_directory = self.watched.get(&id).is_some_and(|w| w.fd as u64 == event.ident);
                // The directory is gone, so the registration is finished:
                // inotify's `IN_IGNORED`. The same flags on a named file
                // only mean the file went, which the rescan reports.
                if on_directory && event.fflags & (NOTE_DELETE | NOTE_RENAME | NOTE_REVOKE) != 0 {
                    out.push(RawEvent { watch: id, name: None, change: RawChange::Dropped });
                    self.remove(id);
                    continue;
                }
                let Some(watched) = self.watched.get_mut(&id) else { continue };
                // Where the names come from: comparing the directory with
                // what it was.
                let now = DirSnapshot::of(&watched.dir);
                for (name, change) in now.changes_since(&watched.seen) {
                    let change = match change {
                        EntryChange::Touched => RawChange::Touched,
                        EntryChange::Gone => RawChange::Gone,
                    };
                    out.push(RawEvent { watch: id, name: Some(name), change });
                }
                watched.seen = now;
                self.refresh_files(id);
            }
            if (n as usize) < events.len() {
                break;
            }
        }
        out
    }
}

fn close_all(watched: &Watched) {
    for (fd, _) in watched.files.values().flatten() {
        unsafe { close(*fd) };
    }
    unsafe { close(watched.fd) };
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        for watched in self.watched.values() {
            close_all(watched);
        }
        unsafe { close(self.kq) };
    }
}
