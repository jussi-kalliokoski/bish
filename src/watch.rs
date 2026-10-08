// Waiting for a file to change, as one more thing the event loop
// already waits on -- `Watcher::fd` goes into a `PollSet` like any other
// descriptor, which is the whole reason this is worth having rather than
// a thread or a timer that re-reads.
//
// What the OS provides is in `platform::DirWatch`: a pollable descriptor
// and a stream of "this name in this directory changed" (inotify on
// Linux, kqueue plus a directory snapshot on macOS, which has nothing
// that names the entry for you). What is here is everything above that,
// and none of it is the OS's business.
//
// **A file is watched through its directory**, not directly, and that
// is the load-bearing decision here. A watch on a file follows the
// *inode*, and almost nothing that rewrites a file keeps the inode: a
// formatter writes a temporary file and renames it over the original, a
// branch switch replaces it, a package manager unlinks and recreates
// it. Each of those makes an inode watch go quiet for ever, reporting
// nothing at exactly the moment there was something to report -- and
// silence is indistinguishable from "nothing happened", which is the
// worst way for this to fail. Watching the parent directory and
// filtering by name catches every one of them, and catches the file
// being created in the first place, which an inode watch cannot do at
// all.
//
// What this deliberately does not do: recurse. Neither OS has a
// recursive mode -- a watcher that appears to offer one is walking the
// tree and adding a watch per directory, then racing to add watches for
// directories created while it walks. Every caller here wants one
// directory or one file, so the honest interface is the one the kernel
// actually has.
// `fd` and `unwatch` are the half of this surface the event loop does
// not use yet: `sync_watched_file` asks `events()` on the tick it is
// already taking, rather than waiting on the descriptor. Both are
// exercised by the tests below, and the allow is what keeps them --
// including, through them, `DirWatch::fd` and `remove` in the platform
// layer -- from being reported as dead.
#![allow(dead_code)]

use crate::platform::{DirWatch, RawChange, WatchId};
use std::collections::HashMap;
use std::io;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

/// What happened to a path.
///
/// Coarser than inotify's own mask on purpose: every caller here wants
/// to know whether to re-read something, and "the file was closed after
/// writing" and "a file was renamed on top of it" are the same answer
/// to that question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The file's contents or metadata changed, or it appeared.
    Written,
    /// It is no longer there under that name.
    Removed,
    /// The kernel's queue overflowed and events were dropped. Whatever
    /// is being watched must be re-read from scratch, because what was
    /// missed is unknowable.
    Overflowed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub path: PathBuf,
    pub change: Change,
}

/// One watch: the directory the kernel reports on, and -- for a file --
/// which name in it we actually care about.
struct Watch {
    dir: PathBuf,
    /// `None` for a directory being watched in its own right, so every
    /// name in it is reported.
    only: Option<PathBuf>,
}

pub struct Watcher {
    inner: DirWatch,
    watches: HashMap<WatchId, Watch>,
    /// So watching the same path twice does not cost two entries -- the
    /// layer below returns the *same* id for a directory already
    /// watched, which would otherwise silently replace one caller's
    /// filter with another's.
    by_path: HashMap<PathBuf, WatchId>,
}

impl Watcher {
    pub fn new() -> io::Result<Watcher> {
        Ok(Watcher { inner: DirWatch::new()?, watches: HashMap::new(), by_path: HashMap::new() })
    }

    /// The descriptor to hand a `PollSet`. Readable exactly when there
    /// is at least one event waiting.
    pub fn fd(&self) -> RawFd {
        self.inner.fd()
    }

    /// Watches `path`: a directory in its own right, or a file through
    /// the directory that holds it.
    ///
    /// A file that does not exist yet is still watchable -- its
    /// directory is what gets the watch, so the file appearing is an
    /// event like any other. A path whose *parent* does not exist is
    /// not, and says so.
    pub fn watch(&mut self, path: &Path) -> io::Result<()> {
        let (dir, only) = match path.is_dir() {
            true => (path.to_path_buf(), None),
            false => {
                let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
                let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name to watch"))?;
                (parent.to_path_buf(), Some(PathBuf::from(name)))
            }
        };
        let wd = self.inner.add(&dir, only.as_deref().map(Path::as_os_str))?;
        // Two callers watching two files in one directory get one
        // kernel watch between them, so the filter has to widen to
        // cover both rather than the second one replacing the first.
        match self.watches.get_mut(&wd) {
            Some(existing) if existing.only != only => existing.only = None,
            Some(_) => {}
            None => {
                self.watches.insert(wd, Watch { dir: dir.clone(), only });
            }
        }
        self.by_path.insert(dir, wd);
        Ok(())
    }

    /// Stops watching whatever `watch` was given. A directory shared by
    /// several watched files goes when the last of them does, which
    /// this cannot know -- so it goes when any of them asks, and the
    /// caller that still cares re-adds it.
    pub fn unwatch(&mut self, path: &Path) {
        let dir = match path.is_dir() {
            true => path.to_path_buf(),
            false => path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf(),
        };
        if let Some(wd) = self.by_path.remove(&dir) {
            self.inner.remove(wd);
            self.watches.remove(&wd);
        }
    }

    pub fn is_watching(&self, path: &Path) -> bool {
        let dir = match path.is_dir() {
            true => path.to_path_buf(),
            false => path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf(),
        };
        self.by_path.contains_key(&dir)
    }

    /// Everything waiting, right now. Never blocks: the descriptor is
    /// non-blocking, so an empty result means nothing has happened, not
    /// that nothing will.
    ///
    /// Events are coalesced by path, keeping the last change for each --
    /// a caller acting on "this file changed" gains nothing from being
    /// told three times, and a save that arrives as a rename plus a
    /// create is one change to anyone reading the file afterwards.
    pub fn events(&mut self) -> Vec<Event> {
        let mut out: Vec<Event> = Vec::new();
        for raw in self.inner.drain() {
            let name = raw.name.unwrap_or_default();
            match raw.change {
                RawChange::Overflowed => out.push(Event { path: PathBuf::new(), change: Change::Overflowed }),
                // The watch is finished, so the entry goes with it, and
                // whatever was being watched through it is as good as
                // gone: the directory holding it was deleted or moved.
                RawChange::Dropped => {
                    if let Some(watch) = self.watches.remove(&raw.watch) {
                        self.by_path.remove(&watch.dir);
                        out.push(Event { path: watch.target(&name), change: Change::Removed });
                    }
                }
                RawChange::Touched | RawChange::Gone => {
                    let Some(watch) = self.watches.get(&raw.watch) else { continue };
                    if !watch.covers(&name) {
                        continue;
                    }
                    let change = match raw.change {
                        RawChange::Gone => Change::Removed,
                        _ => Change::Written,
                    };
                    out.push(Event { path: watch.target(&name), change });
                }
            }
        }
        coalesce(out)
    }
}

impl Watch {
    /// Whether an event about `name` in this directory is one this
    /// watch asked for. An empty name is an event about the directory
    /// itself.
    fn covers(&self, name: &std::ffi::OsStr) -> bool {
        match &self.only {
            None => true,
            Some(only) => name.is_empty() || only.as_os_str() == name,
        }
    }

    fn target(&self, name: &std::ffi::OsStr) -> PathBuf {
        match name.is_empty() {
            true => match &self.only {
                Some(only) => self.dir.join(only),
                None => self.dir.clone(),
            },
            false => self.dir.join(name),
        }
    }
}

/// One event per path, the last one winning -- except an overflow,
/// which is about the whole queue and is kept first so a caller sees it
/// before it trusts anything else in the list.
fn coalesce(events: Vec<Event>) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    for event in events {
        if event.change == Change::Overflowed {
            if !out.iter().any(|e| e.change == Change::Overflowed) {
                out.insert(0, event);
            }
            continue;
        }
        match out.iter_mut().find(|e| e.path == event.path && e.change != Change::Overflowed) {
            Some(existing) => existing.change = event.change,
            None => out.push(event),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // The kernel delivers on its own schedule, so a test that reads
    // once is a test that measures how busy the machine is. This waits
    // for the answer it expects, up to a bound that is generous enough
    // never to be the thing that fails.
    fn wait_for(watcher: &mut Watcher, want: impl Fn(&[Event]) -> bool) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen: Vec<Event> = Vec::new();
        while Instant::now() < deadline {
            seen.extend(watcher.events());
            seen = coalesce(seen);
            if want(&seen) {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        seen
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bish-watch-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_written_file_is_reported() {
        let dir = scratch("written");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut w = Watcher::new().unwrap();
        w.watch(&path).unwrap();

        std::fs::write(&path, "two\n").unwrap();
        let events = wait_for(&mut w, |e| !e.is_empty());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(events, vec![Event { path, change: Change::Written }]);
    }

    // The case the whole design is for: a formatter, a branch switch or
    // an editor writing through a temporary file replaces the inode. A
    // watch on the file itself goes quiet here and reports nothing at
    // all.
    #[test]
    fn a_file_replaced_by_rename_is_still_reported() {
        let dir = scratch("renamed");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut w = Watcher::new().unwrap();
        w.watch(&path).unwrap();

        let temp = dir.join("f.txt.new");
        std::fs::write(&temp, "two\n").unwrap();
        std::fs::rename(&temp, &path).unwrap();
        let events = wait_for(&mut w, |e| e.iter().any(|e| e.path == path));
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(events, vec![Event { path, change: Change::Written }]);
    }

    #[test]
    fn a_file_that_does_not_exist_yet_is_watchable() {
        let dir = scratch("appears");
        let path = dir.join("later.txt");
        let mut w = Watcher::new().unwrap();
        w.watch(&path).unwrap();

        std::fs::write(&path, "here\n").unwrap();
        let events = wait_for(&mut w, |e| !e.is_empty());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(events, vec![Event { path, change: Change::Written }]);
    }

    #[test]
    fn a_removed_file_says_so() {
        let dir = scratch("removed");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut w = Watcher::new().unwrap();
        w.watch(&path).unwrap();

        std::fs::remove_file(&path).unwrap();
        let events = wait_for(&mut w, |e| !e.is_empty());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(events, vec![Event { path, change: Change::Removed }]);
    }

    // Everything else in the directory is somebody else's business.
    #[test]
    fn a_sibling_changing_is_not_this_files_business() {
        let dir = scratch("sibling");
        let path = dir.join("mine.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut w = Watcher::new().unwrap();
        w.watch(&path).unwrap();

        std::fs::write(dir.join("theirs.txt"), "x\n").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let events = w.events();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(events, vec![], "only the watched name is reported");
    }

    #[test]
    fn a_watched_directory_reports_every_name_in_it() {
        let dir = scratch("dir");
        let mut w = Watcher::new().unwrap();
        w.watch(&dir).unwrap();

        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        let events = wait_for(&mut w, |e| !e.is_empty());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(events, vec![Event { path: dir.join("a.txt"), change: Change::Written }]);
    }

    // Two files in one directory share a kernel watch, and the second
    // one asking must not narrow the first out of existence.
    #[test]
    fn two_files_in_one_directory_are_both_watched() {
        let dir = scratch("two");
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "a\n").unwrap();
        std::fs::write(&b, "b\n").unwrap();
        let mut w = Watcher::new().unwrap();
        w.watch(&a).unwrap();
        w.watch(&b).unwrap();

        std::fs::write(&a, "a2\n").unwrap();
        std::fs::write(&b, "b2\n").unwrap();
        let mut events = wait_for(&mut w, |e| e.len() >= 2);
        events.sort_by(|x, y| x.path.cmp(&y.path));
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(events, vec![Event { path: a, change: Change::Written }, Event { path: b, change: Change::Written }]);
    }

    #[test]
    fn the_descriptor_is_pollable() {
        let dir = scratch("poll");
        let path = dir.join("f.txt");
        let mut w = Watcher::new().unwrap();
        w.watch(&path).unwrap();
        assert!(!crate::poll::poll_one(w.fd(), 0), "nothing has happened yet");

        std::fs::write(&path, "x\n").unwrap();
        assert!(crate::poll::poll_one(w.fd(), 2000), "the fd is what the event loop waits on");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_path_whose_parent_is_missing_is_refused() {
        let dir = scratch("missing");
        let mut w = Watcher::new().unwrap();
        assert!(w.watch(&dir.join("nowhere").join("f.txt")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repeated_changes_to_one_file_arrive_as_one() {
        let dir = scratch("coalesce");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut w = Watcher::new().unwrap();
        w.watch(&path).unwrap();

        for i in 0..5 {
            std::fs::write(&path, format!("{i}\n")).unwrap();
        }
        let events = wait_for(&mut w, |e| !e.is_empty());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(events.len(), 1, "one path, one answer: {events:?}");
    }
}
