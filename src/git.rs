// Editor-side git integration (`:git ...` in command mode; see repl.rs's
// `run_command_mode`). Every call runs the user's `git`; `available()`
// makes all of it optional, so without git these features just don't
// work.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// Every `git` this module runs, built here so its output doesn't depend
// on the machine's configuration. bish parses that output, and settings
// change it: `log.showSignature` puts gpg lines before a commit,
// `color.ui = always` adds escapes, `log.date` re-renders `%ad`,
// `core.quotepath` escapes non-ASCII paths. `GIT_DIR`, `GIT_WORK_TREE`
// and `GIT_INDEX_FILE` are cleared, since a git hook's children inherit
// them and would answer about another repository. Aliases, `includeIf`,
// `safe.directory` and mailmaps are left to the repository.
//
// The config keys whose values are commands are overridden: bish runs
// git on its own (every prompt, Ctrl+T, the blame gutter), so a
// repository's config would be code execution on `cd`.
// `core.fsmonitor = "touch /tmp/x; false"` ran on entering a directory.
// `safe.directory` doesn't cover it: it refuses repositories owned by
// other users, and this arrives as an archive you extracted.
//
// Still open: drivers the repository names itself
// (`filter.<name>.clean` via `.gitattributes`, `diff.<name>.textconv`).
// Subsections don't glob, so there's no `-c` for them, and pointing
// `GIT_ATTR_SOURCE` at an empty tree stops them but also drops
// `text=auto` and the eol rules, making clean repositories look dirty.
fn command(dir: &Path) -> Command {
    let mut git = Command::new("git");
    git.current_dir(dir)
        .stdin(Stdio::null())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        // `-c` beats every config file, including the repository's own.
        .args([
            "-c",
            "color.ui=false",
            "-c",
            "color.status=false",
            "-c",
            "color.diff=false",
            "-c",
            "log.showSignature=false",
            "-c",
            "log.date=default",
            "-c",
            "core.quotepath=false",
            // Each of these is a command git would run; bish only reads,
            // never fetches or pages.
            "-c",
            "core.fsmonitor=",
            "-c",
            "core.alternateRefsCommand=",
            "-c",
            "diff.external=",
            "-c",
            "core.sshCommand=",
            "-c",
            "credential.helper=",
            "-c",
            "core.askPass=",
            "-c",
            "core.gitProxy=",
            "-c",
            "uploadpack.packObjectsHook=",
        ]);
    git
}

// Checked on every `:git` command, not cached: a spawn is cheap here, and
// installing git mid-session is picked up.
pub fn available() -> bool {
    command(Path::new(".")).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

// The prompt's git segment: the checked-out branch, or the short hash
// when detached, and whether `git status` shows anything.
pub struct HeadStatus {
    pub branch: String,
    pub dirty: bool,
}

// One `git status --porcelain=v2 --branch` gives both the branch
// (`# branch.head`) and dirtiness (any other line), one spawn per
// prompt. `None` when git is missing or this isn't a repository; the
// prompt treats both alike.
pub fn head_status(dir: &Path, trusted: bool) -> Option<HeadStatus> {
    // In a directory you haven't vouched for, no git runs in the working
    // tree: deciding dirtiness runs the repository's `clean` filter (see
    // `command`). The branch is read from `.git/HEAD`, with no dirty marker
    // until `::bish trust`.
    if !trusted {
        return branch_from_head(dir).map(|branch| HeadStatus { branch, dirty: false });
    }
    let output = command(dir).arg("status").arg("--porcelain=v2").arg("--branch").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut branch = None;
    let mut dirty = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            // Detached: the short hash below, as `git status` would say.
            if rest != "(detached)" {
                branch = Some(rest.to_string());
            }
        } else if !line.starts_with('#') {
            dirty = true;
        }
    }
    let branch = match branch {
        Some(b) => b,
        None => short_head(dir)?,
    };
    Some(HeadStatus { branch, dirty })
}

fn short_head(dir: &Path) -> Option<String> {
    let output = command(dir).arg("rev-parse").arg("--short").arg("HEAD").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The branch from `.git/HEAD`, read as a file, for untrusted
/// `head_status`. `None` outside a repository; a detached HEAD gives the
/// short hash. HEAD is always a loose file (git never packs it): a
/// `ref: refs/heads/NAME` line or a bare object id.
fn branch_from_head(dir: &Path) -> Option<String> {
    let git_dir = find_git_dir(dir)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        // `ref: refs/heads/main` gives `main`; other refs show their last
        // component.
        Some(target) => target.rsplit('/').next().map(str::to_string).filter(|s| !s.is_empty()),
        // Detached: a bare object id, abbreviated as the trusted path does.
        None if head.chars().all(|c| c.is_ascii_hexdigit()) && head.len() >= 7 => Some(head[..head.len().min(8)].to_string()),
        None => None,
    }
}

/// The git directory for `dir`, walking up for `.git`. Usually a
/// directory; in a linked worktree or submodule a file holding
/// `gitdir: <path>`, possibly relative to that file.
fn find_git_dir(dir: &Path) -> Option<PathBuf> {
    let start = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    for ancestor in start.ancestors() {
        let dot_git = ancestor.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file()
            && let Ok(contents) = std::fs::read_to_string(&dot_git)
            && let Some(rest) = contents.trim().strip_prefix("gitdir:")
        {
            let target = Path::new(rest.trim());
            return Some(match target.is_absolute() {
                true => target.to_path_buf(),
                false => ancestor.join(target),
            });
        }
    }
    None
}

// One line's blame, as much as a gutter cell needs: the 8-digit commit
// (git's default abbreviation) and the author date as `YYYY-MM-DD`. An
// uncommitted line has `00000000` and "Not Committed Yet", git's own
// convention.
#[derive(Clone, Debug, PartialEq)]
pub struct BlameLine {
    pub short_commit: String,
    pub author: String,
    pub date: String,
}

// `git blame --line-porcelain` on `path`, one `BlameLine` per line. Run
// from the file's directory with just its name, so bish's cwd doesn't
// matter. `Err` for git failing or unexpected output.
pub fn blame(path: &Path, rev: Option<&str>, trusted: bool) -> Result<Vec<BlameLine>, String> {
    // Blame runs the `clean` filter and `textconv`, so it's gated on the
    // `git` capability, as the prompt is.
    if !trusted {
        return Err("blame: this directory is not trusted (`::bish trust` to allow)".to_string());
    }
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let filename = path.file_name().ok_or_else(|| "no filename".to_string())?;
    let mut blame = command(dir);
    blame.arg("blame").arg("--line-porcelain");
    if let Some(rev) = rev {
        // A revision shaped like an option is refused here:
        // `--end-of-options` (as `log` and `show` use) breaks blame's `--`
        // separator, reading the path as a second revision. The separator
        // matters more, since a branch and a file can share a name.
        if rev.starts_with('-') {
            return Err(format!("bad revision '{rev}'"));
        }
        blame.arg(rev);
    }
    // `--` before the path, so a branch and a file of the same name aren't
    // confused.
    let output = blame.arg("--").arg(filename).output().map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        return Err(first_stderr_line(&output.stderr, "git blame failed"));
    }
    parse_line_porcelain(&String::from_utf8_lossy(&output.stdout))
}

// `path`'s content at `rev`, or in the index for `None` (as a plain
// `git diff` compares against), so blame and diff can be aligned with a
// modified buffer (`align_to`). `Ok(None)` when the file isn't there at
// that revision: every line is then new. No repository and an unknown
// revision are checked first with their own calls, since `git show`
// reports all three as "ambiguous argument", which would make a typo'd
// revision look like an absent file.
pub fn file_at_rev(path: &Path, rev: Option<&str>) -> Result<Option<String>, String> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let filename = path.file_name().ok_or_else(|| "no filename".to_string())?;

    let in_repo = command(dir).args(["rev-parse", "--is-inside-work-tree"]).output().map_err(|e| format!("git: {e}"))?;
    if !in_repo.status.success() {
        return Err(first_stderr_line(&in_repo.stderr, "not a git repository"));
    }
    if let Some(rev) = rev {
        let resolved = command(dir)
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .arg(rev)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("git: {e}"))?;
        if !resolved.success() {
            // `--quiet`: git printed nothing to relay.
            return Err(format!("unknown revision '{rev}'"));
        }
    }

    // `./` makes the path cwd-relative, not repo-root-relative.
    let mut spec = std::ffi::OsString::from(rev.unwrap_or(""));
    spec.push(":./");
    spec.push(filename);
    let output = command(dir).arg("show").arg(&spec).output().map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        // The real failures are ruled out, so the path isn't there.
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

// One `git` call from `dir`: stdout on success, the first line of its
// complaint on failure.
fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = command(dir).args(args).output().map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        return Err(first_stderr_line(&output.stderr, "git failed"));
    }
    Ok(output.stdout)
}

/// One file a commit touched. A rename or copy has both paths; an added
/// file has only a new one and a deleted file only an old one.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangedFile {
    /// git's own letter for what happened: `M`, `A`, `D`, `R`, `C`, `T`.
    pub status: char,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
}

/// A commit, as much of it as `:git show` puts on screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    /// The repository's top level, which every path below is relative to.
    pub root: std::path::PathBuf,
    pub hash: String,
    /// What the diff is against: the first parent, or none for a root
    /// commit, whose every file is new.
    pub parent: Option<String>,
    pub merge: bool,
    /// Hash, author, date and message, laid out the way `git show` does.
    pub header: Vec<String>,
    pub files: Vec<ChangedFile>,
}

/// What `rev` is, from inside the repository at `dir`: its header,
/// parent and the files it changed. A merge is shown against its first
/// parent (`merge` says so), not git's combined diff. Renames are followed
/// (`-M`).
pub fn show(dir: &Path, rev: &str) -> Result<Commit, String> {
    let root = std::path::PathBuf::from(String::from_utf8_lossy(&git(dir, &["rev-parse", "--show-toplevel"])?).trim());
    let hash = git(&root, &["rev-parse", "--verify", "--quiet", "--end-of-options", &format!("{rev}^{{commit}}")])
        .map_err(|_| format!("unknown revision '{rev}'"))?;
    let hash = String::from_utf8_lossy(&hash).trim().to_string();
    let parents: Vec<String> =
        String::from_utf8_lossy(&git(&root, &["rev-list", "--parents", "-n", "1", &hash])?).split_whitespace().skip(1).map(str::to_string).collect();
    let header = String::from_utf8_lossy(&git(&root, &["show", "-s", "--format=commit %H%nAuthor: %an <%ae>%nDate:   %ad%n%n%w(0,4,4)%B", &hash])?)
        .trim_end()
        .lines()
        .map(str::to_string)
        .collect();
    let listing = match parents.first() {
        Some(parent) => git(&root, &["diff-tree", "-r", "-M", "-z", "--no-commit-id", "--name-status", parent, &hash])?,
        None => git(&root, &["diff-tree", "-r", "-M", "-z", "--root", "--no-commit-id", "--name-status", &hash])?,
    };
    Ok(Commit { root, hash, merge: parents.len() > 1, parent: parents.into_iter().next(), header, files: parse_name_status_z(&listing) })
}

/// One commit in a history, as `:git log` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub hash: String,
    /// The author date, `YYYY-MM-DD`.
    pub date: String,
    pub author: String,
    pub subject: String,
}

/// Every tracked file at or below `dir`, relative to `dir` as
/// `git ls-files` gives them from a subdirectory: the paths someone there
/// would type. `.gitignore` doesn't come into it.
pub fn tracked_files(dir: &Path) -> Result<Vec<String>, String> {
    // `-z`: a filename may contain a newline, and git's quoting would
    // break the path.
    Ok(git(dir, &["ls-files", "-z"])?.split(|b| *b == 0).filter(|f| !f.is_empty()).map(|f| String::from_utf8_lossy(f).into_owned()).collect())
}

/// The directories holding those files, nearest the top first. Derived
/// from the files, since git doesn't track directories.
pub fn tracked_directories(files: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    for file in files {
        let mut path = std::path::Path::new(file);
        while let Some(parent) = path.parent() {
            if parent.as_os_str().is_empty() {
                break;
            }
            seen.insert(parent.to_string_lossy().into_owned());
            path = parent;
        }
    }
    seen.into_iter().collect()
}

/// The commits reachable from `rev` (HEAD without one), newest first:
/// every one that touched `path`, followed back through its renames, or
/// every one there is without a path.
pub fn log(dir: &Path, path: Option<&Path>, rev: Option<&str>) -> Result<Vec<LogEntry>, String> {
    let root = std::path::PathBuf::from(String::from_utf8_lossy(&git(dir, &["rev-parse", "--show-toplevel"])?).trim());
    let rev = rev.unwrap_or("HEAD");
    git(&root, &["rev-parse", "--verify", "--quiet", "--end-of-options", &format!("{rev}^{{commit}}")])
        .map_err(|_| format!("unknown revision '{rev}'"))?;
    // Absolute, so it names the same file from the top level.
    let file = path.map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().into_owned());
    let mut args = vec!["log", "--date=short", "--format=%H%x1f%ad%x1f%an%x1f%s%x1e"];
    if file.is_some() {
        args.push("--follow");
    }
    args.extend(["--end-of-options", rev, "--"]);
    if let Some(file) = &file {
        args.push(file);
    }
    Ok(parse_log(&git(&root, &args)?))
}

// Parses `log`'s format: commits ended by 0x1e, fields split by 0x1f,
// bytes no field uses, unlike tab or newline.
pub(crate) fn parse_log(bytes: &[u8]) -> Vec<LogEntry> {
    String::from_utf8_lossy(bytes)
        .split('\u{1e}')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').split('\u{1f}');
            Some(LogEntry {
                hash: fields.next()?.to_string(),
                date: fields.next()?.to_string(),
                author: fields.next()?.to_string(),
                subject: fields.next()?.to_string(),
            })
        })
        .collect()
}

/// The bytes of `path` (relative to the repository's top level) at `rev`.
pub fn blob(root: &Path, rev: &str, path: &str) -> Result<Vec<u8>, String> {
    git(root, &["cat-file", "blob", &format!("{rev}:{path}")])
}

// `git diff-tree --name-status -z`: a status, then one path (two for a
// rename or copy), each ended by NUL, so paths may hold tabs or
// newlines.
pub(crate) fn parse_name_status_z(bytes: &[u8]) -> Vec<ChangedFile> {
    let fields: Vec<String> = bytes.split(|b| *b == 0).filter(|f| !f.is_empty()).map(|f| String::from_utf8_lossy(f).into_owned()).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < fields.len() {
        let status = fields[i].chars().next().unwrap_or('M');
        let path = |k: usize| Some(fields[i + k].clone());
        match status {
            'R' | 'C' if i + 2 < fields.len() => {
                out.push(ChangedFile { status, old_path: path(1), new_path: path(2) });
                i += 3;
                continue;
            }
            'A' => out.push(ChangedFile { status, old_path: None, new_path: path(1) }),
            'D' => out.push(ChangedFile { status, old_path: path(1), new_path: None }),
            _ => out.push(ChangedFile { status, old_path: path(1), new_path: path(1) }),
        }
        i += 2;
    }
    out
}

// Lines up per-line results computed against `old` with the buffer's
// `new` lines by diffing them: entry `i` is the `old` line buffer line
// `i` came from, or `None`. Blame describes a committed version, and
// matching by line number breaks as soon as anything above shifts.
pub(crate) fn align_to(old: &[&str], new: &[&str]) -> Vec<Option<usize>> {
    let mut out = vec![None; new.len()];
    for op in crate::diff::diff(old, new) {
        if let crate::diff::DiffOp::Equal { a, b, len } = op {
            for k in 0..len {
                if let Some(slot) = out.get_mut(b + k) {
                    *slot = Some(a + k);
                }
            }
        }
    }
    out
}

// Parses `--line-porcelain`, which repeats a commit's metadata for every
// line, so each group stands alone: a header (`<sha> <orig-line>
// <final-line> [<count>]`), `key value` lines, then the content line,
// a tab and the text, which ends the group.
fn parse_line_porcelain(text: &str) -> Result<Vec<BlameLine>, String> {
    let mut result = Vec::new();
    let mut lines = text.lines();
    while let Some(header) = lines.next() {
        let sha = header.split_whitespace().next().ok_or("malformed blame output: empty header")?;
        let mut author = String::new();
        let mut author_time: i64 = 0;
        loop {
            let line = lines.next().ok_or("truncated blame output")?;
            if line.starts_with('\t') {
                break;
            } else if let Some(rest) = line.strip_prefix("author ") {
                author = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("author-time ") {
                author_time = rest.trim().parse().unwrap_or(0);
            }
            // Other keys are skipped.
        }
        result.push(BlameLine { short_commit: sha.chars().take(8).collect(), author, date: format_unix_date(author_time) });
    }
    Ok(result)
}

// `:git diff`'s marker for a buffer line. `Removed` marks the nearest
// surviving line beside a deletion (see `diff`), since the removed lines
// are gone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffMark {
    Added,
    Changed,
    Removed,
}

// Every diff gutter's marks, from `crate::diff::diff` rather than
// parsing `git diff`: a `DiffMark` per new-side line. Callers pass two
// slices of lines (the buffer and the disk, or the buffer and a
// revision), so the diffing needs no repository and an unsaved buffer
// diffs correctly.
//
// A Delete directly followed by an Insert is one changed hunk; an
// unpaired Delete marks the nearest surviving line (where the next op
// resumes, or the last line) as Removed, the line `git diff -U0` picks
// (see `marks_from_diff_places_a_deletion_at_the_same_line_git_itself_does`).
pub(crate) fn marks_from_diff(old: &[&str], new: &[&str]) -> HashMap<usize, DiffMark> {
    let ops = crate::diff::diff(old, new);
    let mut marks = HashMap::new();
    let mut i = 0;
    while i < ops.len() {
        match ops[i] {
            crate::diff::DiffOp::Equal { .. } => i += 1,
            crate::diff::DiffOp::Insert { b, len } => {
                for line in b..b + len {
                    marks.insert(line, DiffMark::Added);
                }
                i += 1;
            }
            crate::diff::DiffOp::Delete { .. } => {
                if let Some(crate::diff::DiffOp::Insert { b, len }) = ops.get(i + 1).copied() {
                    for line in b..b + len {
                        marks.insert(line, DiffMark::Changed);
                    }
                    i += 2;
                } else {
                    let new_line = match ops.get(i + 1) {
                        Some(crate::diff::DiffOp::Equal { b, .. }) => *b,
                        None => new.len(),
                        _ => unreachable!("consecutive Deletes are already coalesced, and an adjacent Insert was handled above"),
                    };
                    marks.insert(new_line.saturating_sub(1), DiffMark::Removed);
                    i += 1;
                }
            }
        }
    }
    marks
}

fn first_stderr_line(stderr: &[u8], fallback: &str) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines().next().unwrap_or(fallback).trim().to_string()
}

// git blame's author time as a plain date.
fn format_unix_date(epoch_secs: i64) -> String {
    crate::time::strftime("%F", &crate::time::local_time_at(epoch_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_record_is_its_four_fields_and_a_subject_keeps_its_tabs() {
        let bytes = b"abc\x1f2026-09-14\x1fA Person\x1fFix\tthis\x1e\ndef\x1f2026-09-13\x1fB\x1finitial\x1e\n";
        let got = parse_log(bytes);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], LogEntry { hash: "abc".into(), date: "2026-09-14".into(), author: "A Person".into(), subject: "Fix\tthis".into() });
        assert_eq!(got[1].subject, "initial");
        assert!(parse_log(b"").is_empty());
    }

    #[test]
    fn log_follows_a_file_through_its_rename_and_lists_everything_without_one() {
        if !available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-git-log-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| crate::gittest::run(&dir, args);
        crate::gittest::init(&dir);
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);
        std::fs::write(dir.join("other.txt"), "x\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "unrelated"]);
        run(&["mv", "a.txt", "b.txt"]);
        run(&["commit", "-q", "-m", "renamed"]);

        let subjects = |entries: Vec<LogEntry>| entries.into_iter().map(|e| e.subject).collect::<Vec<_>>();
        assert_eq!(
            subjects(log(&dir, Some(&dir.join("b.txt")), None).unwrap()),
            ["renamed", "initial"],
            "back past the rename, and not the commit that left it alone"
        );
        assert_eq!(subjects(log(&dir, None, None).unwrap()), ["renamed", "unrelated", "initial"]);
        assert_eq!(subjects(log(&dir, None, Some("HEAD~1")).unwrap()), ["unrelated", "initial"]);
        assert_eq!(log(&dir, None, Some("no-such-rev")).unwrap_err(), "unknown revision 'no-such-rev'");
        let entry = &log(&dir, None, None).unwrap()[0];
        assert_eq!((entry.hash.len(), entry.author.as_str()), (40, "Test User"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // The prompt runs git wherever you `cd`, so a repository's config must
    // not run a command.
    #[test]
    fn a_repositorys_own_config_cannot_run_a_command_when_bish_asks_about_it() {
        if !available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-git-fsmonitor-{}", std::process::id()));
        let marker = dir.join("EXECUTED");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| crate::gittest::run(&dir, args);
        crate::gittest::init(&dir);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);
        // `false` afterwards so git falls back to its normal scan: what
        // matters is whether the command ran.
        run(&["config", "core.fsmonitor", &format!("touch {}; false", marker.display())]);

        // Every automatic query: the prompt, the opener's file list, blame.
        let status = head_status(&dir, true);
        let files = tracked_files(&dir);
        let blamed = blame(&dir.join("a.txt"), None, true);

        let ran = marker.exists();
        std::fs::remove_dir_all(&dir).ok();
        assert!(!ran, "a repository's `core.fsmonitor` was executed by bish asking about it");
        // And the answers still arrived.
        assert_eq!(status.map(|s| s.branch), Some("main".to_string()));
        assert_eq!(files.unwrap(), ["a.txt"]);
        assert_eq!(blamed.unwrap().len(), 1);
    }

    // Untrusted: no git runs in the working tree, so a `clean` filter never
    // executes on a prompt draw. The branch comes from `.git/HEAD`, and
    // `dirty` is false, since deciding it would run the filter.
    #[test]
    fn untrusted_head_status_reads_the_branch_without_running_git() {
        if !available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-git-untrusted-{}", std::process::id()));
        let marker = dir.join("FILTER_RAN");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| crate::gittest::run(&dir, args);
        crate::gittest::init(&dir);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join(".gitattributes"), "a.txt filter=x\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);
        run(&["config", "filter.x.clean", &format!("touch {}; cat", marker.display())]);
        // Dirty the tree and bust the stat cache, so a trusted status would
        // filter.
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();

        let status = head_status(&dir, false).expect("branch comes from HEAD");
        assert_eq!(status.branch, "main");
        assert!(!status.dirty, "an untrusted repo reports no dirtiness");
        let ran = marker.exists();

        // Blame is gated too.
        let blamed = blame(&dir.join("a.txt"), None, false);

        std::fs::remove_dir_all(&dir).ok();
        assert!(!ran, "an untrusted directory ran git in its working tree");
        assert!(blamed.is_err(), "blame must refuse an untrusted directory");
    }

    #[test]
    fn branch_from_head_reads_a_ref_and_a_detached_head() {
        if !available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-git-head-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let run = |args: &[&str]| crate::gittest::run(&dir, args);
        crate::gittest::init(&dir);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);

        // From a subdirectory, walking up to the git dir.
        assert_eq!(branch_from_head(&dir.join("sub")).as_deref(), Some("main"));
        // Not a repository: nothing.
        assert_eq!(branch_from_head(std::path::Path::new("/")), None);
        // Detached: the short hash.
        run(&["checkout", "-q", "--detach"]);
        let detached = branch_from_head(&dir).expect("detached still has a HEAD");
        assert_eq!(detached.len(), 8);
        assert!(detached.chars().all(|c| c.is_ascii_hexdigit()));
        std::fs::remove_dir_all(&dir).ok();
    }

    // `--end-of-options` can't fence off a dash revision for `blame`: it
    // breaks the `--` separator. See `blame`.
    #[test]
    fn blame_refuses_a_revision_shaped_like_an_option() {
        if !available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-git-blame-dash-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| crate::gittest::run(&dir, args);
        crate::gittest::init(&dir);
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);

        assert_eq!(blame(&file, Some("-L1,1"), true).unwrap_err(), "bad revision '-L1,1'");
        // A named revision and none still work.
        assert_eq!(blame(&file, Some("HEAD"), true).unwrap().len(), 1);
        assert_eq!(blame(&file, None, true).unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tracked_files_are_listed_from_where_you_are_and_their_directories_derived() {
        if !available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-git-tracked-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("src/platform")).unwrap();
        let run = |args: &[&str]| crate::gittest::run(&dir, args);
        crate::gittest::init(&dir);
        for path in ["README.md", "src/repl.rs", "src/platform/unix.rs"] {
            std::fs::write(dir.join(path), "x\n").unwrap();
        }
        // Not added, so not tracked.
        std::fs::write(dir.join("src/scratch.rs"), "x\n").unwrap();
        run(&["add", "README.md", "src/repl.rs", "src/platform/unix.rs"]);
        run(&["commit", "-q", "-m", "initial"]);

        let files = tracked_files(&dir).unwrap();
        assert_eq!(files, ["README.md", "src/platform/unix.rs", "src/repl.rs"]);
        assert_eq!(tracked_directories(&files), ["src", "src/platform"], "every parent, and the top level is not one of them");
        // From a subdirectory: paths as typed from there, at or below it.
        assert_eq!(tracked_files(&dir.join("src")).unwrap(), ["platform/unix.rs", "repl.rs"]);
        assert!(tracked_directories(&["README.md".to_string()]).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_status_listing_reads_renames_as_one_entry_and_paths_as_they_are() {
        let listing = b"M\0src/a.rs\0R087\0old name\0new\tname\0A\0added\0D\0gone\0";
        let files = parse_name_status_z(listing);
        let entry = |status, old: Option<&str>, new: Option<&str>| ChangedFile {
            status,
            old_path: old.map(str::to_string),
            new_path: new.map(str::to_string),
        };
        assert_eq!(
            files,
            vec![
                entry('M', Some("src/a.rs"), Some("src/a.rs")),
                entry('R', Some("old name"), Some("new\tname")),
                entry('A', None, Some("added")),
                entry('D', Some("gone"), None),
            ]
        );
    }

    #[test]
    fn show_lists_what_a_commit_changed_against_its_parent_or_nothing_for_a_root() {
        if !available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bish-git-show-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let run = |args: &[&str]| crate::gittest::run(&dir, args);
        crate::gittest::init(&dir);
        std::fs::write(dir.join("keep.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("moved.txt"), "a\nb\nc\nd\ne\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "bye\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);
        std::fs::write(dir.join("keep.txt"), "one\nTWO\n").unwrap();
        run(&["add", "keep.txt"]);
        run(&["mv", "moved.txt", "sub/moved.txt"]);
        run(&["rm", "-q", "gone.txt"]);
        run(&["commit", "-q", "-m", "second\n\nwith a body"]);

        let commit = show(&dir.join("sub"), "HEAD").unwrap();
        assert_eq!(commit.header[0], format!("commit {}", commit.hash));
        assert_eq!(commit.header[1], "Author: Test User <test@example.com>");
        assert!(commit.header.iter().any(|l| l == "    with a body"), "{:?}", commit.header);
        assert!(!commit.merge);
        let parent = commit.parent.clone().unwrap();
        let mut statuses: Vec<(char, Option<String>, Option<String>)> =
            commit.files.iter().map(|f| (f.status, f.old_path.clone(), f.new_path.clone())).collect();
        statuses.sort();
        assert_eq!(
            statuses,
            vec![
                ('D', Some("gone.txt".to_string()), None),
                ('M', Some("keep.txt".to_string()), Some("keep.txt".to_string())),
                ('R', Some("moved.txt".to_string()), Some("sub/moved.txt".to_string())),
            ]
        );
        assert_eq!(blob(&commit.root, &parent, "keep.txt").unwrap(), b"one\ntwo\n");
        assert_eq!(blob(&commit.root, &commit.hash, "keep.txt").unwrap(), b"one\nTWO\n");

        let root = show(&dir, "HEAD~1").unwrap();
        assert_eq!(root.parent, None);
        assert_eq!(root.files.len(), 3, "a root commit adds everything in it");
        assert!(root.files.iter().all(|f| f.status == 'A'));

        assert_eq!(show(&dir, "no-such-rev").unwrap_err(), "unknown revision 'no-such-rev'");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // A repository's config changes git's output; `command`'s `-c` outranks
    // it.
    #[test]
    fn a_repositorys_own_config_cannot_reshape_what_is_parsed() {
        if !available() {
            return;
        }
        let dir = crate::tempdir::TempDir::new("git-hostile-config");
        let dir = dir.path();
        crate::gittest::init(dir);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        crate::gittest::run(dir, &["add", "."]);
        crate::gittest::run(dir, &["commit", "-q", "-m", "only commit"]);
        // Real settings: a date format, colour, and signature lines.
        crate::gittest::run(dir, &["config", "log.date", "raw"]);
        crate::gittest::run(dir, &["config", "color.ui", "always"]);
        crate::gittest::run(dir, &["config", "log.showSignature", "true"]);

        let entries = log(dir, None, None).unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].subject, "only commit", "a colour escape would land in the subject");
        assert_eq!(entries[0].date.len(), "2026-10-06".len(), "`--date=short` decides the date, not the repository: {:?}", entries[0].date);

        let commit = show(dir, "HEAD").unwrap();
        let date = commit.header.iter().find(|l| l.starts_with("Date:")).expect("a Date line").clone();
        // `log.date = raw` renders seconds and an offset, so a letter after
        // "Date:" shows the default format survived.
        let rendered = date.trim_start_matches("Date:").trim().to_string();
        assert!(rendered.chars().any(|c| c.is_ascii_alphabetic()), "the header date is still git's default format: {rendered:?}");
        assert!(!commit.header.iter().any(|l| l.contains('\u{1b}')), "no escape codes anywhere in it: {:?}", commit.header);
    }

    // A git hook exports `GIT_DIR`, inherited by everything it starts; left
    // in place, `:git log` in an editor opened from a hook would list the
    // hook's repository.
    #[test]
    fn a_git_dir_in_the_environment_does_not_decide_which_repository() {
        if !available() {
            return;
        }
        let dir = crate::tempdir::TempDir::new("git-env-dir");
        let dir = dir.path();
        crate::gittest::init(dir);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        crate::gittest::run(dir, &["add", "."]);
        crate::gittest::run(dir, &["commit", "-q", "-m", "the one bish was asked about"]);

        // SAFETY: every `git` this suite runs drops these three, which is
        // what's being tested, so no concurrent test is affected. Restored
        // either way.
        let restore = |name: &str, had: Option<String>| match had {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        };
        let had_dir = std::env::var("GIT_DIR").ok();
        let had_tree = std::env::var("GIT_WORK_TREE").ok();
        unsafe { std::env::set_var("GIT_DIR", "/nonexistent/somewhere-else.git") };
        unsafe { std::env::set_var("GIT_WORK_TREE", "/nonexistent") };
        let entries = log(dir, None, None);
        let head = head_status(dir, true);
        restore("GIT_DIR", had_dir);
        restore("GIT_WORK_TREE", had_tree);

        let entries = entries.expect("the repository on screen is the one answered about");
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].subject, "the one bish was asked about");
        assert_eq!(head.expect("a prompt still has a branch to show").branch, "main");
    }

    // Where a deletion is attached at the start, middle and end of a file,
    // each checked against `git diff --no-color -U0`.
    #[test]
    fn marks_from_diff_places_a_deletion_at_the_same_line_git_itself_does() {
        // Middle: `[a,b,c,d]` to `[a,d]` attaches to line 0 (`a`), before the
        // gap.
        assert_eq!(marks_from_diff(&["a", "b", "c", "d"], &["a", "d"]), HashMap::from([(0, DiffMark::Removed)]));
        // End: attaches to the new last line.
        assert_eq!(marks_from_diff(&["a", "b", "c", "d"], &["a", "b", "c"]), HashMap::from([(2, DiffMark::Removed)]));
        // Start: nothing precedes the gap, so the new first line.
        assert_eq!(marks_from_diff(&["a", "b", "c", "d"], &["b", "c", "d"]), HashMap::from([(0, DiffMark::Removed)]));
    }

    #[test]
    fn marks_from_diff_marks_a_pure_addition_and_a_changed_line() {
        let added = marks_from_diff(&["a", "b"], &["a", "NEW1", "NEW2", "b"]);
        assert_eq!(added, HashMap::from([(1, DiffMark::Added), (2, DiffMark::Added)]));

        let changed = marks_from_diff(&["one", "two", "three"], &["one", "two", "CHANGED"]);
        assert_eq!(changed, HashMap::from([(2, DiffMark::Changed)]));
    }

    #[test]
    fn marks_from_diff_is_empty_for_identical_content() {
        assert!(marks_from_diff(&["a", "b"], &["a", "b"]).is_empty());
    }

    #[test]
    fn parse_line_porcelain_reads_author_and_date_per_line() {
        let text = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1
author Jussi Kalliokoski
author-mail <jussi@example.com>
author-time 1700000000
author-tz +0000
committer Jussi Kalliokoski
committer-mail <jussi@example.com>
committer-time 1700000000
committer-tz +0000
summary A commit
filename src/main.rs
\tfn main() {}
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 2 2 1
author Someone Else
author-mail <someone@example.com>
author-time 1600000000
author-tz +0000
committer Someone Else
committer-mail <someone@example.com>
committer-time 1600000000
committer-tz +0000
summary Another commit
filename src/main.rs
\t
";
        let result = parse_line_porcelain(text).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].short_commit, "aaaaaaaa");
        assert_eq!(result[0].author, "Jussi Kalliokoski");
        assert_eq!(result[1].short_commit, "bbbbbbbb");
        assert_eq!(result[1].author, "Someone Else");
    }

    #[test]
    fn parse_line_porcelain_reports_truncated_input() {
        let text = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\nauthor Foo\n";
        assert!(parse_line_porcelain(text).is_err());
    }

    #[test]
    fn parse_line_porcelain_reports_an_empty_header() {
        assert!(parse_line_porcelain("\n").is_err());
    }

    #[test]
    fn format_unix_date_matches_a_known_utc_instant() {
        // `format_unix_date` uses the local timezone, so TZ is pinned to UTC
        // (with `tzset()` so glibc notices).
        unsafe { std::env::set_var("TZ", "UTC") };
        crate::platform::reload_timezone();
        assert_eq!(format_unix_date(0), "1970-01-01");
        // 1700000000 is 2023-11-14 22:13:20 UTC.
        assert_eq!(format_unix_date(1_700_000_000), "2023-11-14");
    }
}
