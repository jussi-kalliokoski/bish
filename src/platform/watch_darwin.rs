//! Watching a directory with kqueue(2).
//!
//! The awkward one. kqueue reports on an open descriptor, and for a
//! directory it says only that *something* in it changed -- never which
//! name, which is the one thing inotify is asked for here. There is no
//! second interface that does: FSEvents, the other macOS answer, is a
//! per-volume log delivered to a callback on a run loop, needs
//! CoreServices, and reports on a delay measured in seconds. kqueue is
//! an ordinary descriptor that goes into a `PollSet` like every other
//! thing bish waits on, which is the whole reason this is worth having.
//!
//! So each watch keeps a `DirSnapshot` of its directory, and an event
//! makes it list the directory again and compare: that is where the
//! names come from. The cost is a `read_dir` per event -- not per poll,
//! and not per tick -- and above this nothing can tell the difference.
//!
//! A directory's own events are not enough on their own, either: one
//! fires when an entry is added, removed or renamed, and *not* when a
//! file already in it is written in place -- which is how most saves
//! happen. So a file the caller named is watched as well, through a
//! descriptor of its own, and an event on it means the same rescan. That
//! descriptor follows the inode, which is exactly what the directory
//! watch exists to get past; so after every rescan a named file whose
//! inode has changed (a rename over it, a delete and recreate) gets a
//! fresh descriptor, and one that has only just appeared gets its first.
//!
//! Two differences that are real and not papered over:
//!
//! - One open descriptor per watched directory and per named file in it,
//!   where inotify has one for the whole set. Both callers in bish watch
//!   one file at a time, so this is a handful of descriptors. A file in
//!   a directory watched in its own right, and named by nobody, is
//!   reported when it appears, goes or is replaced, but not when it is
//!   written in place: watching every file in an arbitrary directory is
//!   a descriptor per file, without bound.
//! - Two writes inside one snapshot interval that leave the size,
//!   inode and modification time identical are invisible here. bish's
//!   own caller hashes the file before it says anything (see
//!   `TextBuffer::changed_on_disk`), so the consequence is a notice that
//!   is not shown, not a wrong one -- and `O_EVTONLY` plus APFS's
//!   nanosecond timestamps make it a narrow window to begin with.

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

/// `struct kevent` as macOS lays it out on a 64-bit target: 32 bytes,
/// `ident` and `data` pointer-wide, `udata` a pointer bish uses as a
/// plain number.
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

// kqueue's own numbers, private here: they belong to a call this OS has
// and Linux does not, so there is no counterpart for them to pair with.
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

// `open(2)`: a descriptor for being told about the file and nothing
// else. It does not count as a reference that keeps a volume from being
// unmounted, which an ordinary `O_RDONLY` on a directory would.
const O_EVTONLY: i32 = 0x8000;
const O_CLOEXEC: i32 = 0x0100_0000;

/// One watched directory: the descriptor kqueue reports on, and what the
/// directory looked like last time anyone asked.
struct Watched {
    dir: PathBuf,
    fd: RawFd,
    seen: DirSnapshot,
    /// The files in it someone named, each with the descriptor watching
    /// it and the inode that descriptor is on -- `None` while the file
    /// does not exist.
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

    /// Readable exactly when there is at least one event waiting -- a
    /// kqueue descriptor polls like any other.
    pub(crate) fn fd(&self) -> RawFd {
        self.kq
    }

    /// Watches `dir`, giving back the same id for a directory already
    /// watched -- inotify's own behaviour, which callers above rely on to
    /// merge two files in one directory into one watch.
    ///
    /// `file` is the entry in it the caller is after, if any -- watched
    /// on its own as well, since nothing about the directory changes
    /// when that file is written in place.
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
        // EV_CLEAR, so an event is reported once rather than every time
        // the queue is read until something resets it.
        let change = KEvent { ident: fd as u64, filter: EVFILT_VNODE, flags: EV_ADD | EV_CLEAR, fflags: WATCH_FFLAGS, data: 0, udata: id as u64 };
        let registered = unsafe { kevent(self.kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if registered < 0 {
            let failure = io::Error::last_os_error();
            unsafe { close(fd) };
            return Err(failure);
        }
        Ok(fd)
    }

    /// Points each named file's descriptor at whatever is under that
    /// name now: a fresh one for a file that appeared or was replaced,
    /// none for one that is gone.
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

    /// Closing the descriptor is what deregisters it: kqueue drops a
    /// registration when its file closes.
    pub(crate) fn remove(&mut self, watch: WatchId) {
        if let Some(gone) = self.watched.remove(&watch) {
            close_all(&gone);
        }
    }

    /// Everything waiting, right now. Never blocks -- a zero timeout is
    /// what makes `kevent` a poll.
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
                // The directory itself is gone, so the registration is
                // finished with it -- the same end inotify reports as
                // `IN_IGNORED`. The same flags on a named file only mean
                // that file went, which the rescan below reports.
                if on_directory && event.fflags & (NOTE_DELETE | NOTE_RENAME | NOTE_REVOKE) != 0 {
                    out.push(RawEvent { watch: id, name: None, change: RawChange::Dropped });
                    self.remove(id);
                    continue;
                }
                let Some(watched) = self.watched.get_mut(&id) else { continue };
                // Where the names come from: kqueue said this directory
                // changed, and comparing it with what it was says which
                // entries did.
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
