//! The one place in bish that talks to the operating system.
//!
//! Every `extern "C"` declaration, every `#[repr(C)]` layout the kernel
//! owns, and every constant whose value is the OS's belongs in this
//! directory; `mod os_guard` fails the build when one appears elsewhere.
//!
//! # Why a layer, rather than a `cfg` at each use
//!
//! bish has no external crates, so all of this is hand-written, and the
//! OSes differ in two ways. Loudly: `inotify_add_watch` doesn't exist on
//! macOS, and the build fails to link. And silently: `TIOCGWINSZ` is
//! `0x5413` on Linux and `0x40087468` on macOS, `VMIN` is index 6 in one
//! `termios` and 16 in the other, `MAP_ANONYMOUS` is `0x20` against
//! `0x1000`, `SC_CLK_TCK` 2 against 3. Each compiles anywhere and is wrong
//! on the other side: raw mode eating keys, a nonsense window size, CPU
//! times off by a factor. Collected in one table per OS, they can be read
//! side by side.
//!
//! # The kinds of file
//!
//! - `unix.rs`: how a capability is done on POSIX, written once. Most of
//!   bish's OS use: a pty, raw mode, a signal, a process group.
//! - `linux.rs` / `darwin.rs`: capabilities where the OSes offer different
//!   calls: `pipe2` against `pipe` and two `fcntl`s, `memfd_create`
//!   against an unlinked file, inotify against kqueue. A flag for a call
//!   only one has stays private to that file.
//! - `sys_linux.rs` / `sys_darwin.rs`: the OS's numbers and structs.
//!   Tables, not logic; the silent differences live here only.
//! - this file: the capabilities, all the rest of bish may call. A new
//!   Unix is a new `sys_` table; a target that isn't a Unix (a browser, an
//!   embedded runtime) replaces `unix.rs`, and this surface is its
//!   contract.
//!
//! `mod sys_tables` pairs every `<name>_linux.rs` with its
//! `<name>_darwin.rs` and fails the build when one has a name the other
//! lacks, so a port can't half-happen.

#[cfg(target_os = "linux")]
#[path = "sys_linux.rs"]
mod sys;

#[cfg(target_os = "macos")]
#[path = "sys_darwin.rs"]
mod sys;

// A target that isn't a Unix (WASI, and browser builds on it) replaces
// `unix.rs` with `web.rs` and has its own `sys`, `os` and `watch`, none of
// which talk to a kernel.
#[cfg(target_family = "wasm")]
#[path = "sys_wasm.rs"]
mod sys;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_family = "wasm")))]
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

#[cfg(target_family = "wasm")]
#[path = "wasm.rs"]
mod os;

#[cfg(target_os = "linux")]
#[path = "watch_linux.rs"]
mod watch;

#[cfg(target_os = "macos")]
#[path = "watch_darwin.rs"]
mod watch;

#[cfg(target_family = "wasm")]
#[path = "watch_wasm.rs"]
mod watch;

#[cfg(unix)]
mod unix;

#[cfg(target_family = "wasm")]
#[path = "web.rs"]
mod unix;

pub(crate) use os::*;
pub(crate) use unix::*;
pub(crate) use watch::DirWatch;
// The signal numbers bish names; two differ between OSes (see the `sys_`
// tables).
pub(crate) use sys::{HIGHEST_SIGNAL, REALTIME_SIGNALS, SIGCONT, SIGHUP, SIGINT, SIGNAL_NAMES, SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU, SIGWINCH};

/// One watched directory, as the OS identifies it: inotify's watch
/// descriptor on Linux, a number this layer issues on macOS. Both promise
/// that the same directory added twice gives the same id, so two watched
/// files in a directory share a watch.
pub(crate) type WatchId = i32;

/// What the OS said happened, before bish filters it: `watch.rs` turns
/// these into events for the names asked about, one per path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEvent {
    pub(crate) watch: WatchId,
    /// The entry inside the directory, or `None` for the directory
    /// itself.
    pub(crate) name: Option<std::ffi::OsString>,
    pub(crate) change: RawChange,
}

// Never constructed where the watch reports nothing (`watch_wasm`), but
// the type must exist.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawChange {
    /// It appeared, was written, or is no longer the same file: one answer,
    /// since every caller's next move is to look.
    Touched,
    /// It is not there under that name any more.
    Gone,
    /// The watch is finished: its directory was deleted or moved, and the id
    /// is dead.
    Dropped,
    /// Events were lost, unknown which. Linux only: kqueue keeps one
    /// registration per directory rather than a queue, so nothing overflows.
    /// Hence the allow.
    #[allow(dead_code)]
    Overflowed,
}

#[cfg(test)]
mod os_guard {
    /// `(file, how many C functions it declares, what they reach for)`.
    /// Empty: every OS call is made from this directory. Kept as the
    /// mechanism, so a declaration elsewhere needs a line here saying why.
    const NOT_MOVED_YET: &[(&str, usize, &str)] = &[];

    /// `(file, how many, why it isn't the operating system)`: an
    /// `extern "C"` block that isn't an OS call. One: bish's own context
    /// switch, defined by this crate's `global_asm!`, where the C ABI only
    /// tells Rust how to call it.
    const NOT_THE_OS: &[(&str, usize, &str)] = &[("src/coroutine.rs", 1, "`bish_switch_context`, hand-written assembly this crate defines itself")];

    /// How many C functions a source file declares, read line by line: a
    /// declaration block holds one `fn` per line and no braces of its own.
    /// Comment lines are skipped, so prose about `{` can't count, and
    /// `extern "C" fn` (a Rust callback, a definition) isn't counted.
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
                // This directory is where they belong.
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
            match NOT_MOVED_YET.iter().chain(NOT_THE_OS).find(|(f, ..)| f == file) {
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
        for (file, _, what) in NOT_MOVED_YET.iter().chain(NOT_THE_OS) {
            if !found.iter().any(|(f, _)| f == file) {
                problems.push(format!("{file} ({what}) declares nothing any more -- remove its line from the table."));
            }
        }
        assert!(problems.is_empty(), "\n  - {}", problems.join("\n  - "));
    }
}

#[cfg(test)]
mod sys_tables {
    /// Every name a `sys_` table exposes, read from the source text: only one
    /// table is compiled, so only reading it as text sees what the other
    /// lacks. `cargo check --target aarch64-apple-darwin` type-checks the
    /// other side.
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
            // Every Linux-side file: `linux.rs` or `<something>_linux.rs`.
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
