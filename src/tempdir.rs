//! A temp directory that removes itself.
//!
//! Tests here name their scratch directories after the process id and
//! delete them on the way out. Two never did at all -- so every run of
//! the suite left one behind for good -- and the rest only delete on
//! the success path, which a panicking test does not take. Six and a
//! half thousand of them had collected under /tmp.
//!
//! Dropping is the fix for both: it runs when the test returns *and*
//! while a panic unwinds, and it is one line at the call site rather
//! than a cleanup that has to be repeated on every early return. A
//! killed process is beyond reach either way.

#![cfg(test)]

use std::path::{Path, PathBuf};

pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// A fresh directory under the system temp dir, named for `tag`,
    /// this process, and a counter.
    ///
    /// The counter is not decoration. Tests run in parallel threads of
    /// one process, so a name built from the tag and the pid alone is
    /// shared by every test using that tag -- and the first one to
    /// finish would delete the directory the others are still working
    /// in. Every guard owns a directory nothing else can name.
    pub(crate) fn new(tag: &str) -> TempDir {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("bish-{}-{}-{}", tag, std::process::id(), n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a scratch directory under the system temp dir");
        TempDir { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `path().join(name)`, for the common case of one file in it.
    pub(crate) fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::TempDir;

    /// The directory goes away when the test *panics*, not only when it
    /// returns -- which is the half that was leaking, and the reason
    /// this is a `Drop` rather than a line at the end of each test.
    #[test]
    fn a_panicking_test_still_loses_its_directory() {
        let path = {
            let dir = TempDir::new("tempdir-panic-check");
            let path = dir.path().to_path_buf();
            assert!(path.is_dir());
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _inner = TempDir::new("tempdir-panic-inner");
                panic!("as a test would");
            }));
            assert!(caught.is_err());
            path
        };
        assert!(!path.exists(), "the guard removes its directory when it goes out of scope");
        let leaked: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("bish-tempdir-panic-inner-"))
            .collect();
        assert!(leaked.is_empty(), "a panic unwound past a guard and left {leaked:?}");
    }
}
