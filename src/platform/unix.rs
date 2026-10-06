//! How a POSIX system does what bish needs.
//!
//! Written once for every Unix: what differs between them is the
//! numbers and layouts, which live in `sys_linux.rs`/`sys_darwin.rs`
//! and reach this file as `sys::*`. A target that is not a Unix at all
//! replaces this file rather than editing it -- see `mod.rs`.

use super::sys;

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

/// `guard + usable` bytes of fresh address space, the lowest `guard`
/// bytes of it unreadable.
///
/// For a stack of bish's own making: stacks grow downward, so the
/// unreadable end is the one an overrun reaches, and it faults there
/// rather than silently corrupting whatever was mapped next. Address
/// space rather than memory -- pages are faulted in as they are
/// touched, so an untouched megabyte costs a page-table entry and
/// nothing else.
pub(crate) fn map_guarded_stack(guard: usize, usable: usize) -> std::io::Result<*mut u8> {
    // POSIX says `(void *) -1`, which every Unix agrees on, so this one
    // is not a per-OS fact.
    const MAP_FAILED: isize = -1;

    let total = guard + usable;
    let base = unsafe { mmap(std::ptr::null_mut(), total, sys::PROT_READ | sys::PROT_WRITE, sys::MAP_PRIVATE | sys::MAP_ANON, -1, 0) };
    if base as isize == MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { mprotect(base, guard, sys::PROT_NONE) } != 0 {
        let failure = std::io::Error::last_os_error();
        unsafe { unmap(base, total) };
        return Err(failure);
    }
    Ok(base)
}

/// Hands `len` bytes mapped by `map_guarded_stack` back to the OS.
///
/// # Safety
///
/// `base` and `len` must be a mapping this module made, and nothing may
/// still be running on it.
pub(crate) unsafe fn unmap(base: *mut u8, len: usize) {
    unsafe { munmap(base, len) };
}

/// A file with no name: created in the temp directory and unlinked
/// before anything can look it up, so the descriptor is the only way to
/// it and nothing is left behind if bish dies holding it.
///
/// `tag` only makes the (very short-lived) name recognisable to anyone
/// watching the directory.
///
/// Here rather than in `darwin.rs`, which is its only caller, for one
/// reason: every line of it is POSIX, so on this side of the port it is
/// compiled and tested on Linux too. Code macOS will run that cannot be
/// run until there is a Mac to hand is the thing most worth avoiding in
/// this directory.
#[allow(dead_code)]
pub(crate) fn unlinked_temp_file(tag: &str) -> Option<std::fs::File> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // `create_new`, so a name that somehow exists already is never
    // opened -- and then a few tries, since a collision is the only way
    // that happens and a different name settles it.
    for _ in 0..8 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("bish-{tag}-{}-{n}", std::process::id()));
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path) else { continue };
        // From here the name is gone and the descriptor is all there is.
        // A failed unlink would leave a file behind under a name nothing
        // reuses, so the caller is told there is no anonymous file to be
        // had rather than being handed one that litters.
        if std::fs::remove_file(&path).is_err() {
            return None;
        }
        return Some(file);
    }
    None
}

/// What a directory looked like at one moment: every name in it, with
/// enough of each entry's identity to tell "changed" from "same file,
/// looked at twice".
///
/// For an OS whose file-change notifications say *that* a directory
/// changed without saying *which* name did -- kqueue, which is macOS's
/// only answer here. Linux's inotify names the entry itself and needs
/// none of this.
///
/// Here rather than in `darwin.rs` for the same reason as
/// `unlinked_temp_file`: it is all `std::fs`, so it is compiled and
/// tested on Linux, where there is a machine to run it on. Dead on
/// Linux at run time for exactly that reason.
#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct DirSnapshot {
    entries: std::collections::HashMap<std::ffi::OsString, Stamp>,
}

/// The parts of an entry worth comparing. Inode included, and
/// deliberately: the case the whole file-watching design exists for is a
/// formatter or a branch switch replacing a file through a rename, which
/// changes the inode and need not change the size or even the
/// modification time.
#[derive(PartialEq, Eq)]
struct Stamp {
    inode: u64,
    size: u64,
    modified: Option<std::time::SystemTime>,
}

/// Whether a name is still there.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryChange {
    /// It appeared, or it is not the same file it was, or it was written
    /// to. All one answer, because every caller's next move is to look.
    Touched,
    /// It is no longer in the directory.
    Gone,
}

#[allow(dead_code)]
impl DirSnapshot {
    /// An unreadable or missing directory snapshots as empty, which
    /// makes its disappearance read as every name in it going -- the
    /// truth, as far as anyone watching a name in it is concerned.
    pub(crate) fn of(dir: &std::path::Path) -> DirSnapshot {
        use std::os::unix::fs::MetadataExt;
        let mut entries = std::collections::HashMap::new();
        let Ok(listing) = std::fs::read_dir(dir) else { return DirSnapshot::default() };
        for entry in listing.flatten() {
            // `symlink_metadata`, so a symlink is the thing being
            // watched rather than whatever it points at: replacing where
            // it points is a change to this directory, and a dangling
            // one still has an entry.
            let Ok(meta) = entry.path().symlink_metadata() else { continue };
            entries.insert(entry.file_name(), Stamp { inode: meta.ino(), size: meta.size(), modified: meta.modified().ok() });
        }
        DirSnapshot { entries }
    }

    /// Every name that is not as it was in `before`.
    ///
    /// Names neither snapshot has, and names both have identically, are
    /// not mentioned -- which is what keeps a sibling's business out of
    /// a watched file's events.
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    // Covers what `darwin.rs`'s `anonymous_file` is, on a machine that
    // is not a Mac: a readable, writable file that no longer has a name.
    #[test]
    fn an_unlinked_temp_file_reads_back_what_was_written_and_has_no_path_left() {
        let before: Vec<std::path::PathBuf> = std::fs::read_dir(std::env::temp_dir())
            .expect("the temp directory is readable")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains("bish-capture-test-"))
            .collect();
        assert!(before.is_empty(), "a previous run left these behind: {before:?}");

        let mut file = super::unlinked_temp_file("capture-test").expect("an anonymous file");
        file.write_all(b"captured output").expect("write");
        file.seek(SeekFrom::Start(0)).expect("rewind");
        let mut read_back = String::new();
        file.read_to_string(&mut read_back).expect("read");
        assert_eq!(read_back, "captured output");

        // Nothing of it is in the directory it was made in, before the
        // descriptor is even dropped.
        let left: Vec<std::path::PathBuf> = std::fs::read_dir(std::env::temp_dir())
            .expect("the temp directory is readable")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains("bish-capture-test-"))
            .collect();
        assert!(left.is_empty(), "the name should be gone while the fd is still open: {left:?}");
    }

    // `DirSnapshot` is the whole of what macOS has to work out for
    // itself, since kqueue says a directory changed without saying which
    // name in it did. Everything it has to get right is checked here, on
    // a machine that is not a Mac.
    #[test]
    fn a_directory_snapshot_notices_what_changed_and_nothing_else() {
        use super::{DirSnapshot, EntryChange};
        let dir = crate::tempdir::TempDir::new("platform-snapshot");
        let dir = dir.path();
        std::fs::write(dir.join("kept.txt"), "same").unwrap();
        std::fs::write(dir.join("edited.txt"), "before").unwrap();
        std::fs::write(dir.join("replaced.txt"), "abcdef").unwrap();
        std::fs::write(dir.join("removed.txt"), "bye").unwrap();
        let before = DirSnapshot::of(dir);

        std::fs::write(dir.join("appeared.txt"), "new").unwrap();
        std::fs::write(dir.join("edited.txt"), "after it grew").unwrap();
        // A rename over the top: the case an inode watch cannot see. The
        // content is the same length as what it replaces, so the inode is
        // the only thing that says anything happened.
        std::fs::write(dir.join("tmp"), "ABCDEF").unwrap();
        std::fs::rename(dir.join("tmp"), dir.join("replaced.txt")).unwrap();
        std::fs::remove_file(dir.join("removed.txt")).unwrap();

        let mut changes = DirSnapshot::of(dir).changes_since(&before);
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        let named: Vec<(String, EntryChange)> = changes.into_iter().map(|(n, c)| (n.to_string_lossy().into_owned(), c)).collect();
        assert_eq!(
            named,
            vec![
                ("appeared.txt".to_string(), EntryChange::Touched),
                ("edited.txt".to_string(), EntryChange::Touched),
                ("removed.txt".to_string(), EntryChange::Gone),
                ("replaced.txt".to_string(), EntryChange::Touched),
            ],
            "`kept.txt` and the vanished `tmp` are the ones that must not appear"
        );
    }

    #[test]
    fn a_directory_that_is_gone_reads_as_everything_in_it_going() {
        use super::{DirSnapshot, EntryChange};
        let dir = crate::tempdir::TempDir::new("platform-snapshot-gone");
        let inner = dir.path().join("sub");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(inner.join("f.txt"), "x").unwrap();
        let before = DirSnapshot::of(&inner);
        std::fs::remove_dir_all(&inner).unwrap();
        let changes = DirSnapshot::of(&inner).changes_since(&before);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].1, EntryChange::Gone);
    }
}
