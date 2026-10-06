//! The one place in bish that talks to the operating system.
//!
//! Every `extern "C"` declaration, every `#[repr(C)]` layout the kernel
//! owns, and every constant whose value is the OS's rather than bish's
//! belongs in this directory. `mod os_guard` below fails the build when
//! one appears anywhere else, so the boundary is a test rather than a
//! convention.
//!
//! # Why a layer at all, rather than a `cfg` at each use
//!
//! bish has no external crates, so every one of these is hand-written,
//! and the OSes it runs on differ in two quite different ways.
//!
//! The first kind is loud: `inotify_add_watch` does not exist on macOS,
//! and a build that calls it fails to link. That kind cannot be
//! forgotten.
//!
//! The second kind is silent, and it is the reason for this directory.
//! `TIOCGWINSZ` is `0x5413` on Linux and `0x40087468` on macOS; `VMIN`
//! is index 6 in one `termios` and 16 in the other; `MAP_ANONYMOUS` is
//! `0x20` against `0x1000`; `SC_CLK_TCK` is 2 against 3. Every one of
//! them is an integer literal that compiles anywhere and is simply
//! wrong on the other side -- raw mode that eats keystrokes, a window
//! size of nonsense, CPU times off by a factor. Spelled out at the
//! point of use, as bish spelled them out until now, there is nothing
//! to review and no way to tell a ported value from an unported one.
//! Collected in one table per OS, they can be read side by side.
//!
//! # The kinds of file
//!
//! - `unix.rs` -- how a capability is *done* on a POSIX system, written
//!   once for every such OS. The great majority of bish's OS use is of
//!   this kind: a pty, raw mode, a signal, a process group.
//! - `linux.rs` / `darwin.rs` -- the capabilities where one body cannot
//!   serve both, because the OSes do not offer the same call: `pipe2`
//!   against `pipe` plus two `fcntl`s, `memfd_create` against a file
//!   that is unlinked the moment it exists, inotify against kqueue. A
//!   flag belonging to a call only one of them has stays private in that
//!   file, rather than forcing an invented counterpart into the other's
//!   table.
//! - `sys_linux.rs` / `sys_darwin.rs` -- what that OS's own numbers and
//!   structs *are*. Tables, not logic, and the second kind of
//!   difference above lives here and nowhere else.
//! - this file -- the capabilities themselves, which is all the rest of
//!   bish may call. A new Unix is a new `sys_` table; a target that is
//!   not a Unix at all (a browser, an embedded runtime) replaces
//!   `unix.rs`, and this surface is the contract it has to satisfy.
//!
//! `mod sys_tables` pairs every `<name>_linux.rs` with its
//! `<name>_darwin.rs` and fails the build when one of the two has a name
//! the other does not, so a port cannot half-happen: a constant or a
//! function added for Linux is a failure until macOS has one too.

#[cfg(target_os = "linux")]
#[path = "sys_linux.rs"]
mod sys;

#[cfg(target_os = "macos")]
#[path = "sys_darwin.rs"]
mod sys;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "bish has no table of this OS's own numbers yet -- add src/platform/sys_<os>.rs beside the two that are there, \
     and see src/platform/mod.rs for what belongs in it"
);

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;

#[cfg(target_os = "macos")]
#[path = "darwin.rs"]
mod os;

#[cfg(target_os = "linux")]
#[path = "watch_linux.rs"]
mod watch;

#[cfg(target_os = "macos")]
#[path = "watch_darwin.rs"]
mod watch;

mod unix;
pub(crate) use os::*;
pub(crate) use unix::*;
pub(crate) use watch::DirWatch;
// The signal numbers bish names. Facts about the OS, and two of them
// differ: see either `sys_` table.
pub(crate) use sys::{SIGHUP, SIGINT, SIGTSTP, SIGTTIN, SIGTTOU};

/// One watched directory, as the OS identifies it.
///
/// Opaque: inotify's own watch descriptor on Linux, a number this layer
/// hands out on macOS. The one promise about it is the one both sides
/// keep -- the same directory added twice gives the same id, which is
/// what lets two watched files in one directory share a watch.
pub(crate) type WatchId = i32;

/// What the OS said happened, before any of bish's own filtering.
///
/// `watch.rs` turns these into the events the rest of bish sees: which
/// names a caller actually asked about, what a change to one means, and
/// one answer per path rather than five.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEvent {
    pub(crate) watch: WatchId,
    /// The entry inside the directory, or `None` for the directory
    /// itself.
    pub(crate) name: Option<std::ffi::OsString>,
    pub(crate) change: RawChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawChange {
    /// It appeared, or it was written to, or it is not the file it was.
    /// One answer, because every caller's next move is the same: look.
    Touched,
    /// It is not there under that name any more.
    Gone,
    /// This watch is finished -- the directory it was on was deleted or
    /// moved. The id is dead and the OS will say nothing more about it.
    Dropped,
    /// Events were lost, and nothing about *which* can be known.
    ///
    /// Linux's, and never reported on macOS: kqueue keeps one
    /// registration per watched directory rather than a queue of
    /// events, so there is nothing there to overflow. Hence the allow --
    /// a variant only one OS ever constructs.
    #[allow(dead_code)]
    Overflowed,
}

#[cfg(test)]
mod os_guard {
    /// `(file, how many C functions it declares, what they reach for)`.
    ///
    /// Not an allow-list in the sense `exec.rs`'s own `spawn_guard` has
    /// one -- every line here is work still to do, and the file it names
    /// should eventually be calling `platform` instead. The counts are
    /// what keeps it honest: a number that no longer matches is a
    /// failure in both directions, so the table cannot drift from the
    /// tree whether a declaration was added or moved out.
    const NOT_MOVED_YET: &[(&str, usize, &str)] = &[
        ("src/builtins/limits.rs", 6, "`ulimit` and `times`: resource limits, clock ticks, the umask"),
        ("src/coroutine.rs", 2, "the context switch itself, and a deliberately failing syscall in its tests"),
        ("src/editor.rs", 1, "reads a key from the terminal"),
        ("src/exec.rs", 41, "the whole of job control: fds, signals, process groups, waiting"),
        ("src/git.rs", 1, "pins the timezone while a commit date is formatted"),
        ("src/poll.rs", 6, "waits on a set of fds"),
        ("src/repl.rs", 1, "reads a key without going through the editor"),
        ("src/scheduler.rs", 3, "hands a coroutine's stage its own fds"),
        ("src/session.rs", 5, "the session socket: who is on the other end, and who is listening"),
        ("src/stackguard.rs", 1, "how much stack this process was given"),
        ("src/time.rs", 3, "the wall clock, and the local timezone it is shown in"),
    ];

    /// How many C functions a source file declares.
    ///
    /// Read line by line rather than parsed: a declaration block holds
    /// one `fn` per line and no braces of its own, so tracking the
    /// block's own braces and counting `fn` inside it is enough.
    /// Comment lines are skipped so that prose about a `{` cannot be
    /// mistaken for one, and `extern "C" fn` -- a Rust function the OS
    /// calls back, which is a definition and not a declaration -- is
    /// deliberately not counted.
    fn c_declarations(source: &str) -> usize {
        let mut total = 0;
        let mut depth = 0usize;
        for line in source.lines() {
            let line = line.trim();
            if line.starts_with("//") {
                continue;
            }
            if depth == 0 {
                let Some(at) = line.find("extern \"C\"") else { continue };
                let rest = &line[at + "extern \"C\"".len()..];
                if rest.contains("fn ") {
                    continue;
                }
                depth = line[at..].matches('{').count() - line[at..].matches('}').count();
                continue;
            }
            total += line.matches("fn ").count();
            depth = depth + line.matches('{').count() - line.matches('}').count();
        }
        total
    }

    #[test]
    fn every_c_declaration_is_either_in_platform_or_on_the_list() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found: Vec<(String, usize)> = Vec::new();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("src is readable").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let relative = path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                // This directory is where they are supposed to be.
                if relative.starts_with("src/platform/") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("a source file is readable");
                let count = c_declarations(&source);
                if count > 0 {
                    found.push((relative, count));
                }
            }
        }
        found.sort();

        let mut problems: Vec<String> = Vec::new();
        for (file, count) in &found {
            match NOT_MOVED_YET.iter().find(|(f, ..)| f == file) {
                None => problems.push(format!(
                    "{file} declares {count} C function(s) of its own.\n     \
                     The operating system is spoken to in src/platform/ and nowhere else -- add the capability there and call it from here.\n     \
                     See src/platform/mod.rs for what belongs in which file."
                )),
                Some((_, listed, what)) if listed != count => problems.push(format!(
                    "{file} declares {count} C function(s) where the table says {listed} ({what}).\n     \
                     Moving them into src/platform/ is the point, so a smaller number is progress: update the count, or drop the line when it reaches zero.\n     \
                     A larger one means a new declaration went in outside src/platform/."
                )),
                Some(_) => {}
            }
        }
        for (file, _, what) in NOT_MOVED_YET {
            if !found.iter().any(|(f, _)| f == file) {
                problems.push(format!("{file} ({what}) declares nothing any more -- remove its line from the table."));
            }
        }
        assert!(problems.is_empty(), "\n  - {}", problems.join("\n  - "));
    }
}

#[cfg(test)]
mod sys_tables {
    /// Every name a `sys_` table exposes.
    ///
    /// Read from the source text rather than from the compiled module,
    /// because only one of the two is ever compiled: on Linux the
    /// Darwin table is `cfg`'d out, so nothing but reading it as text
    /// can notice that it is missing something. `cargo check --target
    /// aarch64-apple-darwin` is what type-checks the other side.
    fn exported_names(source: &str) -> std::collections::BTreeSet<String> {
        let mut names = std::collections::BTreeSet::new();
        for line in source.lines() {
            let Some(rest) = line.trim().strip_prefix("pub(crate) ") else { continue };
            let rest = rest.strip_prefix("unsafe ").unwrap_or(rest);
            for kind in ["const ", "static ", "fn ", "struct ", "type ", "enum ", "union "] {
                if let Some(name) = rest.strip_prefix(kind) {
                    let end = name.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(name.len());
                    names.insert(name[..end].to_string());
                    break;
                }
            }
        }
        names
    }

    #[test]
    fn linux_and_darwin_describe_the_same_things() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/platform");
        let mut pairs = 0;
        let mut problems: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("src/platform is readable").flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Every Linux-side file, by either spelling: `linux.rs`
            // itself, or `<something>_linux.rs`.
            let Some(stem) = name.strip_suffix("linux.rs") else { continue };
            let counterpart = format!("{stem}darwin.rs");
            let Ok(darwin_source) = std::fs::read_to_string(dir.join(&counterpart)) else {
                problems.push(format!("{name} has no {counterpart} beside it -- every Linux-side file needs the macOS half of itself"));
                continue;
            };
            let linux = exported_names(&std::fs::read_to_string(entry.path()).expect("a platform file is readable"));
            let darwin = exported_names(&darwin_source);
            pairs += 1;
            for (missing, from, source) in [
                (linux.difference(&darwin).collect::<Vec<_>>(), &counterpart, &name),
                (darwin.difference(&linux).collect::<Vec<_>>(), &name, &counterpart),
            ] {
                if !missing.is_empty() {
                    problems.push(format!(
                        "{from} is missing {missing:?}, which {source} has.\n     \
                         Nothing bish calls may exist for one OS and not the other -- give that side the value or the body it uses instead, \
                         or a stub that says it has none."
                    ));
                }
            }
            assert!(!linux.is_empty(), "{name} exposes nothing, so pairing it proves nothing");
        }
        assert!(problems.is_empty(), "\n  - {}", problems.join("\n  - "));
        assert!(pairs >= 2, "only {pairs} pair(s) of platform files found, so this test is barely proving anything");
    }
}
