//! Watching a directory without a kernel to ask. WASI preview 1 has
//! neither inotify nor kqueue, so a `DirWatch` here holds nothing and
//! `drain` is always empty: the code above runs, and file changes go
//! unnoticed.

use super::{RawEvent, WatchId};
use std::io;
use std::os::fd::RawFd;
use std::path::Path;

/// A watch of nothing, with fd `-1`, which every poll ignores.
pub(crate) struct DirWatch;

impl DirWatch {
    pub(crate) fn new() -> io::Result<DirWatch> {
        Ok(DirWatch)
    }

    /// Never readable: nothing is ever waiting.
    pub(crate) fn fd(&self) -> RawFd {
        -1
    }

    /// Accepts the request and returns a stable id, so the same directory
    /// added twice gets the same number, as the real backends promise, though
    /// no event comes.
    pub(crate) fn add(&mut self, _dir: &Path, _file: Option<&std::ffi::OsStr>) -> io::Result<WatchId> {
        Ok(0)
    }

    pub(crate) fn remove(&mut self, _watch: WatchId) {}

    /// Always empty.
    pub(crate) fn drain(&mut self) -> Vec<RawEvent> {
        Vec::new()
    }
}
