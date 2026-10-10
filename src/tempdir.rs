//! A temp directory that removes itself on drop: when a test returns and
//! when a panic unwinds, which cleanup at the end of a test misses.

#![cfg(test)]

use std::path::{Path, PathBuf};

pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// A fresh directory under the system temp dir, named for `tag`, this
    /// process and a counter. Tests run as threads of one process, so tag
    /// and pid alone would be shared, and the first to finish would delete
    /// the others' directory.
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

    /// `path().join(name)`, for one file in it.
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

    /// The directory goes even when the test panics.
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
