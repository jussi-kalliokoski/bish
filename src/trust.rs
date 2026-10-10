// Which directories the user has vouched for, and for what.
//
// bish does a handful of things that let a directory's *own* contents
// decide what code runs: a git repository's config and attributes can
// name a filter or a textconv command that git executes while bish is
// only asking it a question (see `git::command`), and a toolchain hook
// may want to source a project-local env file or run a project-local
// formatter. All of that is useful in a directory you put there and a
// trap in one you downloaded -- the difference is not in the files, it
// is in whether you trust them, which is a thing only you can say.
//
// So this is where you say it. `::bish trust` records a directory and a
// glob of *capabilities* it is trusted for; everything that would act on
// a directory's own say-so asks `is_trusted` first, naming the
// capability it needs. bish's own git integration uses the capability
// `"git"`; a hook can invent any key it likes and gate itself on
// `::bish trust --check <key>`, so the mechanism is the shell's and the
// policy is the tool's.
//
// Trust descends: vouching for a repository root vouches for everything
// under it, because that is where you stand when you run the command and
// where the subdirectories you work in live. It is per real path --
// resolved through symlinks at the moment it is recorded and at the
// moment it is checked -- so a trusted name cannot be made to cover an
// untrusted place by moving a link.

use crate::glob;
use std::io::Write;
use std::path::{Path, PathBuf};

/// One line of the trust file: a directory, and the capabilities it is
/// trusted for as a single glob (`*` for all of them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub dir: PathBuf,
    pub capabilities: String,
}

/// `$XDG_CONFIG_HOME/bish/trust`, or `~/.config/bish/trust`.
///
/// The same place `gitignore`'s own config lookup already treats as
/// bish's config root, so trust sits beside the rc rather than inventing
/// a directory of its own. `None` when neither variable is set, which is
/// the same "then there is nowhere to read or write" every other
/// config-file path here reaches.
pub fn store_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home.map(|h| h.join(".config")))?;
    Some(base.join("bish").join("trust"))
}

/// The real, absolute form of a directory -- what both recording and
/// checking compare, so a relative path, a `.`, a trailing slash or a
/// symlink in the middle can never make two names for one place look
/// like two places. Falls back to the path as given when it cannot be
/// resolved (it does not exist yet, say), which still compares equal to
/// itself.
fn canonical(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// Every recorded entry, in file order. A line is `GLOB\tDIR`; the glob
/// comes first because a glob never contains a tab and a path might, so
/// the directory is the whole of the rest of the line. A line without a
/// tab, or an empty or `#`-commented one, is skipped rather than
/// refused -- a trust file is something a person may edit, and one bad
/// line should not void the rest.
pub fn load() -> Vec<Entry> {
    let Some(path) = store_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    text.lines()
        .filter(|l| !l.trim_start().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|line| {
            let (caps, dir) = line.split_once('\t')?;
            Some(Entry { dir: PathBuf::from(dir), capabilities: caps.to_string() })
        })
        .collect()
}

/// Whether `dir` is trusted for `capability` -- the one question
/// everything that acts on a directory's own say-so asks.
///
/// True when some recorded entry covers `dir` -- is it, or an ancestor
/// of it -- and that entry's capability glob matches the one asked for.
/// Both sides are resolved to real paths first, so neither a relative
/// query nor a symlinked tree escapes the comparison.
pub fn is_trusted(dir: &Path, capability: &str) -> bool {
    let dir = canonical(dir);
    load().iter().any(|entry| {
        let root = canonical(&entry.dir);
        (dir == root || dir.starts_with(&root)) && glob::matches(&entry.capabilities, capability)
    })
}

/// Records `dir` as trusted for `capabilities`, replacing any entry for
/// the same directory rather than stacking a second one -- trusting a
/// place twice is setting what it is trusted for, not adding to it.
pub fn trust(dir: &Path, capabilities: &str) -> std::io::Result<()> {
    let dir = canonical(dir);
    let mut entries = load();
    entries.retain(|e| canonical(&e.dir) != dir);
    entries.push(Entry { dir, capabilities: capabilities.to_string() });
    save(&entries)
}

/// Removes the entry for exactly `dir`, if there is one. Returns whether
/// there was: the caller says "nothing was trusted there" rather than
/// claiming it removed something it did not.
pub fn untrust(dir: &Path) -> std::io::Result<bool> {
    let dir = canonical(dir);
    let mut entries = load();
    let before = entries.len();
    entries.retain(|e| canonical(&e.dir) != dir);
    let removed = entries.len() != before;
    if removed {
        save(&entries)?;
    }
    Ok(removed)
}

/// Writes the whole list back, creating the config directory if it is
/// not there yet. `0600`, because the list of places you trust is not
/// anybody else's business -- and the directory `0700` for the same
/// reason, matching how the session code treats its own.
fn save(entries: &[Entry]) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let path = store_path().ok_or_else(|| std::io::Error::other("no config directory (set HOME or XDG_CONFIG_HOME)"))?;
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    let mut body = String::new();
    for entry in entries {
        // A path with a newline or a tab in it cannot be represented a
        // line at a time and is dropped rather than written as a line
        // that would read back as something else. Both are pathological
        // in a directory name; a dropped entry is a trust not recorded,
        // which fails safe.
        let dir = entry.dir.to_string_lossy();
        if dir.contains('\n') || dir.contains('\t') {
            continue;
        }
        body.push_str(&entry.capabilities);
        body.push('\t');
        body.push_str(&dir);
        body.push('\n');
    }
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
    file.write_all(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A whole trust store in one place, isolated from the real one:
    // point XDG_CONFIG_HOME at a fresh temp dir for the duration. The
    // store is process-global state (a file), so these take the env
    // lock the rest of the suite uses for the same reason.
    fn with_store(body: impl FnOnce(&Path)) {
        // The store is a file keyed off env vars, so these serialize on
        // their own lock and put every var back -- the same shape
        // session.rs's own env-mutating tests use.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = crate::tempdir::TempDir::new("trust");
        let saved = std::env::var_os("XDG_CONFIG_HOME");
        let home = std::env::var_os("HOME");
        // SAFETY: under the env lock, like every other env-mutating test.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            std::env::remove_var("HOME");
        }
        body(dir.path());
        unsafe {
            match saved {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn trust_descends_to_subdirectories_and_is_scoped_by_capability() {
        with_store(|root| {
            let repo = root.join("repo");
            std::fs::create_dir_all(repo.join("src/inner")).unwrap();
            trust(&repo, "*").unwrap();

            // The directory itself and everything under it.
            assert!(is_trusted(&repo, "git"));
            assert!(is_trusted(&repo.join("src"), "git"));
            assert!(is_trusted(&repo.join("src/inner"), "anything-at-all"));
            // Not its parent, and not a sibling.
            assert!(!is_trusted(root, "git"));
            std::fs::create_dir_all(root.join("other")).unwrap();
            assert!(!is_trusted(&root.join("other"), "git"));

            // A narrower glob trusts only what it matches.
            let tool = root.join("tool");
            std::fs::create_dir_all(&tool).unwrap();
            trust(&tool, "mise:*").unwrap();
            assert!(is_trusted(&tool, "mise:env"));
            assert!(!is_trusted(&tool, "git"));
        });
    }

    #[test]
    fn a_relative_or_symlinked_path_resolves_to_the_same_place() {
        with_store(|root| {
            let real = root.join("real");
            std::fs::create_dir_all(&real).unwrap();
            trust(&real, "*").unwrap();

            // A symlink to the trusted directory is the trusted
            // directory: trust is about the place, not the name.
            let link = root.join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(is_trusted(&link, "git"), "a symlink to a trusted dir is trusted");
            std::fs::create_dir_all(link.join("sub")).unwrap();
            assert!(is_trusted(&link.join("sub"), "git"), "...and so is a path reached through it");
        });
    }

    #[test]
    fn trusting_a_place_twice_sets_rather_than_stacks() {
        with_store(|root| {
            let dir = root.join("d");
            std::fs::create_dir_all(&dir).unwrap();
            trust(&dir, "*").unwrap();
            trust(&dir, "git").unwrap();
            assert_eq!(load().iter().filter(|e| canonical(&e.dir) == canonical(&dir)).count(), 1, "one entry per directory");
            assert!(is_trusted(&dir, "git"));
            assert!(!is_trusted(&dir, "mise:env"), "the second trust replaced the first's `*`");

            assert!(untrust(&dir).unwrap());
            assert!(!is_trusted(&dir, "git"));
            assert!(!untrust(&dir).unwrap(), "nothing left to remove");
        });
    }

    #[test]
    fn the_store_is_private() {
        use std::os::unix::fs::PermissionsExt;
        with_store(|root| {
            trust(&root.join("d"), "*").unwrap();
            let path = store_path().unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        });
    }

    #[test]
    fn a_malformed_line_is_skipped_not_fatal() {
        with_store(|_root| {
            let path = store_path().unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "# a comment\n\nno-tab-here\n*\t/trusted/place\n").unwrap();
            let entries = load();
            assert_eq!(entries.len(), 1, "only the one well-formed line");
            assert_eq!(entries[0].dir, PathBuf::from("/trusted/place"));
        });
    }
}
