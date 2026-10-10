// Which directories the user has vouched for, and for what.
//
// Some things let a directory's own contents decide what runs: a git
// repository's config can name a filter or textconv that git executes
// while bish only asks it something (see `git::command`), and a hook may
// source a project's env file or run its formatter. Fine in a directory
// you made, a trap in one you downloaded; only the user can tell them
// apart.
//
// `::bish trust` records a directory and a glob of capabilities it's
// trusted for, and everything acting on a directory's say-so asks
// `is_trusted` with the capability it needs. bish's git integration uses
// `"git"`; a hook can use any key via `::bish trust --check <key>`.
//
// Trust descends: trusting a repository root covers everything under
// it. Paths are resolved through symlinks when recorded and when
// checked, so moving a link can't stretch trust to another place.

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

/// `$XDG_CONFIG_HOME/bish/trust`, or `~/.config/bish/trust`, beside the
/// rc, as `gitignore`'s config lookup finds it. `None` when neither
/// variable is set.
pub fn store_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home.map(|h| h.join(".config")))?;
    Some(base.join("bish").join("trust"))
}

/// The real absolute form of a directory, so a relative path, `.`, a
/// trailing slash or a symlink can't make one place look like two. Falls
/// back to the path as given when it can't be resolved.
fn canonical(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// Every entry, in file order. A line is `GLOB\tDIR`, glob first because
/// a path may contain a tab and a glob can't. A line without a tab, or
/// blank, or `#`-commented, is skipped: a person may edit this file, and
/// one bad line shouldn't void the rest.
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

/// Whether `dir` is trusted for `capability`: some entry is `dir` or an
/// ancestor, with a matching glob, both sides resolved to real paths.
///
/// `owned_default` is the `trust_owned` bishopt: capabilities a directory
/// you own gets without an entry. Empty by default: owning a place is
/// weaker than vouching for it (an unpacked download is yours).
pub fn is_trusted(dir: &Path, capability: &str, owned_default: &str) -> bool {
    let dir = canonical(dir);
    let by_entry = load().iter().any(|entry| {
        let root = canonical(&entry.dir);
        (dir == root || dir.starts_with(&root)) && glob::matches(&entry.capabilities, capability)
    });
    by_entry || (!owned_default.is_empty() && glob::matches(owned_default, capability) && owned_by_this_user(&dir))
}

/// Whether `dir` is owned by bish's effective uid, who would act on the
/// trust. Fails closed: an unstattable path isn't owned.
fn owned_by_this_user(dir: &Path) -> bool {
    std::fs::metadata(dir).map(|m| crate::platform::file_owner(&m) == crate::platform::effective_user()).unwrap_or(false)
}

/// Records `dir` as trusted for `capabilities`, replacing any entry for
/// it: trusting twice sets, not adds.
pub fn trust(dir: &Path, capabilities: &str) -> std::io::Result<()> {
    let dir = canonical(dir);
    let mut entries = load();
    entries.retain(|e| canonical(&e.dir) != dir);
    entries.push(Entry { dir, capabilities: capabilities.to_string() });
    save(&entries)
}

/// Removes the entry for exactly `dir`, returning whether there was one,
/// so the caller doesn't claim to have removed something.
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

/// Writes the list back, creating the config directory. The file `0600`
/// and the directory `0700`: where you trust is nobody else's business.
fn save(entries: &[Entry]) -> std::io::Result<()> {
    let path = store_path().ok_or_else(|| std::io::Error::other("no config directory (set HOME or XDG_CONFIG_HOME)"))?;
    if let Some(parent) = path.parent() {
        crate::platform::create_dir_all_with_mode(parent, 0o700)?;
    }
    let mut body = String::new();
    for entry in entries {
        // A path with a newline or tab can't be stored a line at a time, so
        // it's dropped rather than written as something else; a trust not
        // recorded fails safe.
        let dir = entry.dir.to_string_lossy();
        if dir.contains('\n') || dir.contains('\t') {
            continue;
        }
        body.push_str(&entry.capabilities);
        body.push('\t');
        body.push_str(&dir);
        body.push('\n');
    }
    let mut file = crate::platform::create_private_file(&path, false)?;
    file.write_all(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trust store isolated from the real one, with XDG_CONFIG_HOME on a
    // fresh temp dir meanwhile.
    fn with_store(body: impl FnOnce(&Path)) {
        // The store is a file keyed off env vars, so these tests serialize
        // on a lock and restore every variable, as session.rs's do.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = crate::tempdir::TempDir::new("trust");
        let saved = std::env::var_os("XDG_CONFIG_HOME");
        let home = std::env::var_os("HOME");
        // SAFETY: under the env lock, like every env-mutating test.
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

            // The directory and everything under it.
            assert!(is_trusted(&repo, "git", ""));
            assert!(is_trusted(&repo.join("src"), "git", ""));
            assert!(is_trusted(&repo.join("src/inner"), "anything-at-all", ""));
            // Not its parent, nor a sibling.
            assert!(!is_trusted(root, "git", ""));
            std::fs::create_dir_all(root.join("other")).unwrap();
            assert!(!is_trusted(&root.join("other"), "git", ""));

            // A narrower glob trusts only what it matches.
            let tool = root.join("tool");
            std::fs::create_dir_all(&tool).unwrap();
            trust(&tool, "mise:*").unwrap();
            assert!(is_trusted(&tool, "mise:env", ""));
            assert!(!is_trusted(&tool, "git", ""));
        });
    }

    #[test]
    fn a_relative_or_symlinked_path_resolves_to_the_same_place() {
        with_store(|root| {
            let real = root.join("real");
            std::fs::create_dir_all(&real).unwrap();
            trust(&real, "*").unwrap();

            // A symlink to the trusted directory is that directory: trust
            // is about the place, not the name.
            let link = root.join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(is_trusted(&link, "git", ""), "a symlink to a trusted dir is trusted");
            std::fs::create_dir_all(link.join("sub")).unwrap();
            assert!(is_trusted(&link.join("sub"), "git", ""), "...and so is a path reached through it");
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
            assert!(is_trusted(&dir, "git", ""));
            assert!(!is_trusted(&dir, "mise:env", ""), "the second trust replaced the first's `*`");

            assert!(untrust(&dir).unwrap());
            assert!(!is_trusted(&dir, "git", ""));
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

    // `trust_owned`: a directory you own is trusted for what its glob
    // matches, without an entry; off by default.
    #[test]
    fn owning_a_directory_trusts_it_only_when_the_option_is_set() {
        with_store(|root| {
            let mine = root.join("mine");
            std::fs::create_dir_all(&mine).unwrap();
            // Created by this test, so owned by this user.

            // The default (empty) trusts nothing, even what you own.
            assert!(!is_trusted(&mine, "git", ""));
            // `git`: an owned directory is trusted for git, and only git.
            assert!(is_trusted(&mine, "git", "git"));
            assert!(!is_trusted(&mine, "mise:env", "git"));
            // `*` trusts it for everything.
            assert!(is_trusted(&mine, "anything", "*"));
            // It descends like any trust.
            std::fs::create_dir_all(mine.join("sub")).unwrap();
            assert!(is_trusted(&mine.join("sub"), "git", "git"));
            // Someone else's directory isn't trusted whatever the option:
            // `/proc` is root-owned. Skipped where it doesn't exist.
            let other = std::path::Path::new("/proc");
            if other.is_dir() {
                use std::os::unix::fs::MetadataExt;
                if std::fs::metadata(other).map(|m| m.uid() != crate::platform::effective_user()).unwrap_or(false) {
                    assert!(!is_trusted(other, "git", "*"), "ownership, not just the glob, has to hold");
                }
            }
            // An explicit entry works with the option off.
            assert!(!is_trusted(&root.join("elsewhere"), "git", ""));
            std::fs::create_dir_all(root.join("elsewhere")).unwrap();
            trust(&root.join("elsewhere"), "*").unwrap();
            assert!(is_trusted(&root.join("elsewhere"), "git", ""));
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
