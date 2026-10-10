//! Watching a directory, where there is no kernel to ask.
//!
//! WASI preview 1 has neither inotify nor kqueue, so a `DirWatch` here
//! holds nothing and reports nothing: it constructs so the code above
//! it runs, and `drain` is forever empty. `watch.rs`'s snapshot-diff
//! fallback (`DirSnapshot`, in `web.rs`) is what actually notices a
//! change on this platform, polled rather than pushed.

use super::{RawEvent, WatchId};
use std::io;
use std::os::fd::RawFd;
use std::path::Path;

/// A watch that watches nothing. It still owns a descriptor number so
/// the event loop above has something to poll on -- `-1`, which every
/// poll ignores.
pub(crate) struct DirWatch;

impl DirWatch {
    pub(crate) fn new() -> io::Result<DirWatch> {
        Ok(DirWatch)
    }

    /// Never readable: nothing is ever waiting.
    pub(crate) fn fd(&self) -> RawFd {
        -1
    }

    /// Accepts the request and hands back a stable id, so a caller that
    /// adds the same directory twice gets the same number -- the one
    /// promise the real backends keep -- even though no event will come.
    pub(crate) fn add(&mut self, _dir: &Path, _file: Option<&std::ffi::OsStr>) -> io::Result<WatchId> {
        Ok(0)
    }

    pub(crate) fn remove(&mut self, _watch: WatchId) {}

    /// Always empty: on this platform a change is found by looking, not
    /// by being told.
    pub(crate) fn drain(&mut self) -> Vec<RawEvent> {
        Vec::new()
    }
}
