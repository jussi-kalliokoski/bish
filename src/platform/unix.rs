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

unsafe extern "C" {
    fn tcgetattr(fd: i32, mode: *mut sys::Termios) -> i32;
    fn tcsetattr(fd: i32, actions: i32, mode: *const sys::Termios) -> i32;
    fn raise(signal: i32) -> i32;
    fn signal(signal: i32, handler: usize) -> usize;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
}

/// How a terminal is behaving: what to put back, and what to derive the
/// next mode from.
///
/// Opaque on purpose. Which bits mean what is this layer's business, and
/// a caller that could reach the flags would be a caller with a reason
/// to know a number that differs per OS.
#[derive(Clone, Copy)]
pub(crate) struct TerminalMode(sys::Termios);

/// How `fd`'s terminal is behaving right now, or `None` if it is not a
/// terminal at all.
pub(crate) fn terminal_mode(fd: i32) -> Option<TerminalMode> {
    // SAFETY: a `Termios` of zeroes is a valid one to be written over,
    // and `tcgetattr` writes the whole of it or fails.
    let mut mode: sys::Termios = unsafe { std::mem::zeroed() };
    match unsafe { tcgetattr(fd, &mut mode) } {
        0 => Some(TerminalMode(mode)),
        _ => None,
    }
}

/// Makes `fd`'s terminal behave as `mode` says, at once rather than
/// after whatever is queued has drained.
pub(crate) fn set_terminal_mode(fd: i32, mode: &TerminalMode) -> std::io::Result<()> {
    match unsafe { tcsetattr(fd, sys::TCSANOW, &mode.0) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Whether the line discipline is still holding typed bytes until a
/// newline -- that is, whether the terminal is *not* in raw mode.
///
/// `ICANON` is the bit that decides it. On a pty the two ends share one
/// set of these, so asking this of the *master* is how the side driving
/// it can tell that the program on the slave has taken the terminal --
/// which is what the vim corpus waits for before it types anything, and
/// its only caller. A test, therefore, so a release build does not
/// report code that is not dead but is not part of the shell either.
#[cfg(test)]
pub(crate) fn is_canonical(mode: &TerminalMode) -> bool {
    mode.0.c_lflag & sys::ICANON != 0
}

/// Raw mode, derived from whatever the terminal is doing now: no line
/// buffering (`ICANON` off), no echo (bish draws the line itself), no
/// signals from control characters (`ISIG` off -- the editor reads those
/// as plain bytes and decides for itself), and no output post-processing
/// (`OPOST` off, so a caller writes "\r\n" rather than relying on the
/// terminal to translate).
pub(crate) fn raw_mode(from: &TerminalMode) -> TerminalMode {
    let mut raw = from.0;
    raw.c_iflag &= !(sys::IGNBRK | sys::BRKINT | sys::PARMRK | sys::ISTRIP | sys::INLCR | sys::IGNCR | sys::ICRNL | sys::IXON);
    raw.c_oflag &= !sys::OPOST;
    raw.c_lflag &= !(sys::ECHO | sys::ICANON | sys::IEXTEN | sys::ISIG);
    raw.c_cflag &= !sys::CSIZE;
    raw.c_cflag |= sys::CS8;
    raw.c_cc[sys::VMIN] = 1;
    raw.c_cc[sys::VTIME] = 0;
    TerminalMode(raw)
}

/// Cooked mode, derived the same way -- and deliberately *not* the
/// bit-for-bit inverse of `raw_mode`.
///
/// Raw mode clears `IGNCR` alongside `ICRNL` (with both off a bare CR
/// passes through untranslated either way), but the two are not
/// independent: POSIX has `IGNCR` take priority and discard CR entirely
/// before `ICRNL` gets a say. Turning *both* back on -- the naive
/// symmetric inverse, tried first and caught interactively -- silently
/// ate every Enter: a script's `read` echoed what was typed and never
/// saw a line terminator, hanging on something that looked like it
/// should work. Real cooked mode leaves IGNCR/INLCR/ISTRIP/PARMRK/
/// IGNBRK/BRKINT alone; only ICANON/ECHO/ISIG/IEXTEN, OPOST and
/// ICRNL/IXON have to be forced back on for kernel-driven line editing
/// and echo to behave.
pub(crate) fn cooked_mode(from: &TerminalMode) -> TerminalMode {
    let mut cooked = from.0;
    cooked.c_iflag |= sys::ICRNL | sys::IXON;
    cooked.c_oflag |= sys::OPOST;
    cooked.c_lflag |= sys::ECHO | sys::ICANON | sys::IEXTEN | sys::ISIG;
    TerminalMode(cooked)
}

/// The terminal exactly as it is, minus the echo -- not raw mode: the
/// line discipline still edits and buffers a line, it just does not show
/// it. `read -s`'s own contract.
pub(crate) fn echoless_mode(from: &TerminalMode) -> TerminalMode {
    let mut silent = from.0;
    silent.c_lflag &= !sys::ECHO;
    TerminalMode(silent)
}

/// Makes this process ignore `signal` for the rest of its life.
///
/// The disposition is inherited by every forked child *and* survives
/// exec: POSIX resets a real handler function to the default across an
/// exec, and explicitly leaves "ignore" in place. So every site that
/// forks a real child has to put it back by hand, or that child would
/// silently ignore it too.
pub(crate) fn ignore_signal(signal_number: i32) {
    unsafe { signal(signal_number, sys::SIG_IGN) };
}

/// Sends `signal` to this process.
pub(crate) fn raise_signal(signal_number: i32) {
    unsafe { raise(signal_number) };
}

/// Reads whatever is there, retrying an interrupted read.
///
/// `Ok(0)` is end of input. A read cut short by a signal is not an
/// answer about the file at all, so it is taken again rather than
/// reported -- which is what every caller here would do with it.
pub(crate) fn read_bytes(fd: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let n = unsafe { read(fd, buf.as_mut_ptr(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let failure = std::io::Error::last_os_error();
        if failure.kind() != std::io::ErrorKind::Interrupted {
            return Err(failure);
        }
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
